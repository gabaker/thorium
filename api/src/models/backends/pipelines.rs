//! Wrappers for interacting with pipelines within Thorium with different backends
//! Currently only Redis is supported

use std::collections::{BTreeMap, HashMap, HashSet};
use tracing::instrument;
use uuid::Uuid;

use crate::models::backends::{NotificationSupport, db, stage_tools};
use crate::models::{
    DEFAULT_PIPELINE_SLA, Group, GroupAllowAction, Pipeline, PipelineBanKind, PipelineBanUpdate,
    PipelineDetailsList, PipelineKey, PipelineList, PipelineRequest, PipelineStats, PipelineUpdate,
    User,
};
use crate::utils::{ApiError, Shared, bounder};
use crate::{
    bad, can_delete, conflict, deserialize_ext, deserialize_opt, extract, is_admin, not_found,
    update_clear, update_opt_empty,
};

impl PipelineRequest {
    /// Casts a PipelineRequest to a Pipeline
    ///
    /// Every stage in the order must have a tool in the request's stage tools, and each stage tool
    /// must back a stage in the order and be able to run its workload.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is creating this pipeline
    /// * `group` - The group this PipelineRequest is for
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "PipelineRequest::cast", skip(user, group, shared), err(Debug))]
    pub async fn cast(
        self,
        user: &User,
        group: &Group,
        shared: &Shared,
    ) -> Result<Pipeline, ApiError> {
        // if no sla is given assume 1 week
        let sla = self.sla.unwrap_or(DEFAULT_PIPELINE_SLA);
        // bounds check sla
        bounder::number(sla as i64, "sla", 1, 3.154e+9 as i64)?;
        // bounds check our pipeline order
        let order = bounder::pipeline_order(&self.order)?;
        // every stage must be backed by a tool
        let missing = order
            .iter()
            .flatten()
            .filter(|stage| !self.stage_tools.contains_key(*stage))
            .map(|stage| format!("`{stage}`"))
            .collect::<Vec<String>>();
        if !missing.is_empty() {
            return bad!(format!(
                "every stage needs a tool in stage_tools; these stages have none: {}",
                missing.join(", ")
            ));
        }
        // every stage tool must back a stage in the order
        check_stage_names(&order, self.stage_tools.keys())?;
        // validate our triggers
        bounder::triggers(&self.triggers)?;
        // make sure each stage's tool can back it and we can develop for its workload
        stage_tools::check(user, group, &self.stage_tools, shared).await?;
        // build pipeline
        let pipeline = Pipeline {
            group: self.group,
            name: self.name,
            creator: user.username.clone(),
            order,
            sla,
            triggers: self.triggers,
            description: self.description,
            bans: HashMap::default(),
            stage_tools: self.stage_tools,
        };
        Ok(pipeline)
    }
}

/// Make sure every named stage is a stage in a pipeline's order
///
/// # Arguments
///
/// * `order` - The pipeline's order
/// * `stages` - The stage names to check
fn check_stage_names<'a, I: Iterator<Item = &'a String>>(
    order: &[Vec<String>],
    stages: I,
) -> Result<(), ApiError> {
    // find the named stages that aren't in the order
    let unknown = stages
        .filter(|stage| !order.iter().flatten().any(|name| name == *stage))
        .map(|stage| format!("`{stage}`"))
        .collect::<Vec<String>>();
    if !unknown.is_empty() {
        return bad!(format!(
            "stage_tools names stages that aren't in the order: {}",
            unknown.join(", ")
        ));
    }
    Ok(())
}

impl PipelineList {
    /// Creates new pipeline list object
    ///
    /// # Arguments
    ///
    /// * `cursor` - A cursor used to page to the next list of pipelines
    /// * `names` - A list of pipeline names
    pub(super) fn new(cursor: Option<usize>, names: Vec<String>) -> Self {
        PipelineList { cursor, names }
    }

    /// Turns a [`PipelineList`] into a [`PipelineDetailsList`]
    ///
    /// # Arguments
    ///
    /// * `group` - The group these reactions are from
    pub(crate) async fn details(
        self,
        group: &Group,
        shared: &Shared,
    ) -> Result<PipelineDetailsList, ApiError> {
        // use correct backend to list reaction details
        let details = db::pipelines::list_details(&group.name, &self.names, shared).await?;
        // cast to reaction details list
        let details_list = PipelineDetailsList::new(self.cursor, details);
        Ok(details_list)
    }
}

impl PipelineDetailsList {
    /// Creates a new pipeline details list object
    ///
    /// # Arguments
    ///
    /// * `cursor` - A cursor used to page to the next list of pipelines
    /// * `details` - A list of pipeline details
    pub(super) fn new(cursor: Option<usize>, details: Vec<Pipeline>) -> Self {
        PipelineDetailsList { cursor, details }
    }
}

impl PipelineBanUpdate {
    /// Updates a pipeline's ban list
    ///
    /// # Arguments
    ///
    /// * `image` - The image to apply this update to
    /// * `user` - The user attempting to apply this update
    pub fn update(self, pipeline: &mut Pipeline, user: &User) -> Result<(), ApiError> {
        // exit immediately if there are no bans to add/remove
        if self.is_empty() {
            return Ok(());
        }
        // check that the user is an admin and return unauthorized if not
        is_admin!(user);
        // check that all the bans in the list of bans to be removed actually exist
        for ban_remove in &self.bans_removed {
            if !pipeline.bans.contains_key(ban_remove) {
                return not_found!(format!(
                    "The ban with id '{ban_remove}' is not contained in the image ban list",
                ));
            }
        }
        // check that all bans to be added don't already exist
        for ban_add in &self.bans_added {
            if pipeline.bans.contains_key(&ban_add.id) {
                return bad!(format!(
                    "A ban with id '{}' already exists. Bans cannot be updated, only added or removed.",
                    ban_add.id
                ));
            }
        }
        // add the requested bans
        for ban in self.bans_added {
            pipeline.bans.insert(ban.id, ban);
        }
        // remove the requested bans
        for ban_id in &self.bans_removed {
            pipeline.bans.remove(ban_id);
        }
        Ok(())
    }
}

impl Pipeline {
    /// Creates a new pipeline
    ///
    /// This also ensures that the group exists and the pipeline data is valid
    ///
    /// # Arguments
    ///
    /// * `user` - The user creating a pipeline
    /// * `req` - The pipeline request to create
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Pipeline::create", skip_all, err(Debug))]
    pub async fn create(
        user: &User,
        req: PipelineRequest,
        shared: &Shared,
    ) -> Result<Self, ApiError> {
        // ensure name is alphanumeric and lowercase
        bounder::string_lower(&req.name, "name", 1, 25)?;
        // a new name can't be the word for an entity kind
        bounder::entity_name(&req.name)?;
        // authorize this user is apart of this group
        let group = Group::authorize(user, &req.group, shared).await?;
        // make sure we can create pipelines in this group
        group.allowable(GroupAllowAction::Pipelines)?;
        // make sure this group is editable
        group.editable(user)?;
        // check if the pipeline exists now that the user is authenticated in the group
        if db::pipelines::exists_authenticated(&req.name, &group, shared).await? {
            return conflict!(format!(
                "Pipeline {} already exists in group {}",
                &req.name, &req.group
            ));
        }
        // create pipeline in backend
        db::pipelines::create(user, &group, req, shared).await
    }

    /// Gets a pipeline from the backend
    ///
    ///
    /// # Arguments
    ///
    /// * `user` - The user getting a pipeline
    /// * `group` - The group the Pipeline is in
    /// * `name` - The name of the pipeline to get
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Pipeline::get", skip(user, shared), err(Debug))]
    pub async fn get(
        user: &User,
        group: &str,
        name: &str,
        shared: &Shared,
    ) -> Result<(Group, Self), ApiError> {
        // make sure we are a member of this group and it exists
        let group_obj = Group::authorize(user, group, shared).await?;
        // get pipeline data from backend
        let pipeline = db::pipelines::get(group, name, shared).await?;
        Ok((group_obj, pipeline))
    }

    /// Lists pipelines in a group
    ///
    /// # Arguments
    ///
    /// * `group` - The group to list pipelines from
    /// * `cursor` - The cursor to use to page through pipelines
    /// * `limit` - The max number of pipelines to attempt to return (not strongly enforced)
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Pipeline::list", skip(group, shared), err(Debug))]
    pub async fn list(
        group: &Group,
        cursor: usize,
        limit: usize,
        shared: &Shared,
    ) -> Result<PipelineList, ApiError> {
        // get all pipelines in group
        db::pipelines::list(&group.name, cursor, limit, shared).await
    }

    /// Deletes a pipeline from the backend
    ///
    /// # Arguments
    ///
    /// * `user` - The user deleting this pipeline
    /// * `group` - The group to delete the pipeline from
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Pipeline::delete", skip_all, fields(group = &self.name), err(Debug))]
    pub async fn delete(self, user: &User, group: &Group, shared: &Shared) -> Result<(), ApiError> {
        // make sure we have modify capabilities in this group or we own this pipeline
        can_delete!(self, group, user);
        // delete the pipeline
        db::pipelines::delete(user, group, self, true, shared).await
    }

    /// Deletes all pipelines and their reactions/jobs in a group from the backend
    ///
    /// # Arguments
    ///
    /// * `group` - The group to delete all pipelines from
    /// * `user` - The user deleting this pipeline
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Pipeline::delete_all", skip_all, fields(group = &group.name), err(Debug))]
    pub async fn delete_all(user: &User, group: &Group, shared: &Shared) -> Result<(), ApiError> {
        // make sure we are an owner of this group
        group.modifiable(user)?;
        // delete all pipelines in group
        db::pipelines::delete_all(user, group, shared).await
    }

    /// Updates a pipeline
    ///
    /// Every stage that stays in the order keeps a tool, every stage added to the order needs one,
    /// and each tool must be able to back its stage. The tools of stages that leave the order are
    /// dropped.
    ///
    /// # Arguments
    ///
    /// * `update` - The updates to apply to this pipeline
    /// * `user` - The user that is updating this pipeline
    /// * `group` - The group the pipeline to update is from
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Pipeline::update", skip(user, group, shared), err(Debug))]
    pub async fn update(
        mut self,
        mut update: PipelineUpdate,
        user: &User,
        group: &Group,
        shared: &Shared,
    ) -> Result<Self, ApiError> {
        // make sure we can create pipelines in this group
        group.allowable(GroupAllowAction::Pipelines)?;
        // make sure this group is editable
        group.editable(user)?;
        // get the stages that are currently in this pipeline
        let old = self
            .order
            .iter()
            .flatten()
            .cloned()
            .collect::<HashSet<String>>();
        // overlay the new order ontop of pipeline
        if let Some(order) = &update.order {
            self.order = bounder::pipeline_order(order)?;
        }
        // build the set of the stages in our updated pipeline
        let new = self
            .order
            .iter()
            .flatten()
            .cloned()
            .collect::<HashSet<String>>();
        // a stage that stays in the order can't lose its tool
        let kept = update
            .remove_stage_tools
            .iter()
            .filter(|stage| new.contains(*stage) && !update.add_stage_tools.contains_key(*stage))
            .map(|stage| format!("`{stage}`"))
            .collect::<Vec<String>>();
        if !kept.is_empty() {
            return bad!(format!(
                "stages that stay in the order can't lose their tool: {}",
                kept.join(", ")
            ));
        }
        // every added stage tool must back a stage in the order
        check_stage_names(&self.order, update.add_stage_tools.keys())?;
        // remove the stage tools this update removes
        for stage in &update.remove_stage_tools {
            self.stage_tools.remove(stage);
        }
        // add or replace the stage tools this update sets
        self.stage_tools.extend(
            update
                .add_stage_tools
                .iter()
                .map(|(stage, id)| (stage.clone(), *id)),
        );
        // drop the tools of stages no longer in the order
        self.stage_tools.retain(|stage, _| new.contains(stage));
        // every stage added to the order needs a tool
        let missing = self
            .order
            .iter()
            .flatten()
            .filter(|stage| !old.contains(*stage) && !self.stage_tools.contains_key(*stage))
            .map(|stage| format!("`{stage}`"))
            .collect::<Vec<String>>();
        if !missing.is_empty() {
            return bad!(format!(
                "every stage needs a tool in stage_tools; these stages have none: {}",
                missing.join(", ")
            ));
        }
        // make sure each stage's tool can back it and we can develop for its workload
        stage_tools::check(user, group, &self.stage_tools, shared).await?;
        // get the added and removed stages
        let add = new.difference(&old).cloned().collect::<Vec<String>>();
        let remove = old.difference(&new).cloned().collect::<Vec<String>>();
        // priority
        if let Some(sla) = update.sla {
            self.sla = bounder::unsigned(sla, "sla", 0, 3.154e+9 as u64)?;
        }
        // add in any new triggers
        self.triggers.extend(update.triggers);
        // remove any deleted triggers
        self.triggers
            .retain(|name, _| !update.remove_triggers.contains(name));
        // validate our triggers
        bounder::triggers(&self.triggers)?;
        // update description
        update_opt_empty!(self.description, update.description);
        // clear description if flag is set
        update_clear!(self.description, update.clear_description);
        // save a copy of our bans before updating
        let mut bans_update = update.bans.clone();
        // update our ban list if there are any to update
        update.bans.update(&mut self, user)?;
        // remove bans if their associated image has been removed
        let mut bans_removed = self
            .bans
            .extract_if(|_, ban| match &ban.ban_kind {
                PipelineBanKind::BannedImage(ban_kind) => remove.contains(&ban_kind.image),
                _ => false,
            })
            .map(|(id, _)| id)
            .collect::<Vec<Uuid>>();
        // use correct backend to update pipeline
        db::pipelines::update(&self, &add, &remove, shared).await?;
        // update the pipeline's notifications based on the updated bans
        bans_removed.append(&mut bans_update.bans_removed);
        let key = PipelineKey::from(&self);
        self.update_ban_notifications(&key, &bans_update.bans_added, &bans_removed, shared)
            .await?;
        Ok(self)
    }

    /// Get the length of a stage in a pipeline
    ///
    /// # Arguments
    ///
    /// * `stage` - Index of stage to get length of
    pub fn stage_length(&self, stage: usize) -> Result<usize, ApiError> {
        // bounds check iterator
        if stage < self.order.len() {
            Ok(self.order[stage].len())
        } else {
            bad!(format!(
                "Cannot get stage length of {}:{}[{}] as stage does not exist",
                self.group, self.name, stage
            ))
        }
    }

    /// Get the status for all stages of this pipeline for all users in a group
    ///
    /// # Arguments
    ///
    /// * `users` - The users to get status updates for
    pub async fn status(
        &self,
        users: &[&String],
        shared: &Shared,
    ) -> Result<PipelineStats, ApiError> {
        // get this pipelines status from the backend
        db::pipelines::status(self, users, shared).await
    }
}

impl TryFrom<HashMap<String, String>> for Pipeline {
    type Error = ApiError;

    /// Try to cast a HashMap of strings into a Pipeline
    ///
    /// # Arguments
    ///
    /// * `raw` - The HashMap to cast into a Pipeline
    fn try_from(mut raw: HashMap<String, String>) -> Result<Self, Self::Error> {
        // check if any data was returned
        if raw.is_empty() {
            return not_found!("Pipeline data not found".to_owned());
        }

        // cast to Pipeline
        let pipeline = Pipeline {
            group: extract!(raw, "group"),
            name: extract!(raw, "name"),
            creator: extract!(raw, "creator"),
            order: deserialize_ext!(raw, "order"),
            sla: extract!(raw, "sla").parse::<u64>()?,
            triggers: deserialize_ext!(raw, "triggers", HashMap::default()),
            description: deserialize_opt!(raw, "description"),
            bans: deserialize_ext!(raw, "bans", HashMap::default()),
            stage_tools: deserialize_ext!(raw, "stage_tools", BTreeMap::default()),
        };
        Ok(pipeline)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::models::Pipeline;

    /// A pipeline hash written before stage tools existed reads with no stage tools
    #[test]
    fn pipeline_hash_without_stage_tools() {
        // a pipeline hash with only the fields earlier versions wrote
        let raw = [
            ("group", "jupiter"),
            ("name", "orbit"),
            ("creator", "europa"),
            ("order", "[[\"plant\"],[\"harvest\"]]"),
            ("sla", "600"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect::<HashMap<String, String>>();
        // it reads with an empty stage tool map, so every stage is unconverted
        let pipeline = Pipeline::try_from(raw).expect("a pipeline");
        assert!(pipeline.stage_tools.is_empty());
        assert_eq!(pipeline.unconverted_stages(), vec!["plant", "harvest"]);
    }
}
