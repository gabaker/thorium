//! Resolving the tools that back pipeline stages into the tool instances reactions run
//!
//! A pipeline stage is backed by a Tool (which follows the tool's latest instance) or a pinned
//! Tool Instance. Creating a reaction resolves every stage to a pinned tool instance in the
//! reaction's plan, and each stage's jobs take their scaler and generator flag from that tool
//! instance's workload.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use tracing::instrument;
use uuid::Uuid;

use crate::models::backends::db;
use crate::models::{
    Entity, EntityKinds, EntityMetadata, Group, ImageJobInfo, ImageScaler, Pipeline, PlannedStage,
    ReactionPlan, RunProvenance, ToolSpec, User,
};
use crate::utils::{ApiError, Shared};
use crate::{bad, not_found};

/// The tools and tool instances read while handling one request, so each is read at most once
#[derive(Debug, Default)]
pub struct ToolInstanceMemo {
    /// The tool and tool instance entities read so far, by id
    entities: HashMap<Uuid, Entity>,
}

impl ToolInstanceMemo {
    /// Remember a tool instance this request already read, so it isn't read again
    ///
    /// # Arguments
    ///
    /// * `entity` - The tool instance entity
    pub fn remember(&mut self, entity: Entity) {
        // keep the first read of each entity
        self.entities.entry(entity.id).or_insert(entity);
    }

    /// Get an entity this request already read and found in a group
    ///
    /// # Arguments
    ///
    /// * `group` - The group the entity must have been found in
    /// * `id` - The id of the entity
    fn known(&self, group: &str, id: Uuid) -> Option<&Entity> {
        // only an entity a read found in this group is known in it
        self.entities
            .get(&id)
            .filter(|entity| entity.groups.iter().any(|known| known == group))
    }

    /// Read the entities in a group that no read so far found in it, in one batch
    ///
    /// An entity that isn't found in the group is left out without an error; a later lookup of
    /// it reads it on its own and answers the same error a lookup without a prefetch does.
    ///
    /// # Arguments
    ///
    /// * `group` - The group the entities must be in
    /// * `ids` - The ids of the entities to read
    /// * `shared` - Shared Thorium objects
    pub async fn prefetch(
        &mut self,
        group: &str,
        ids: &[Uuid],
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // keep each id that is not known in this group yet once
        let mut wanted = Vec::with_capacity(ids.len());
        for id in ids {
            if self.known(group, *id).is_none() && !wanted.contains(id) {
                wanted.push(*id);
            }
        }
        // there is nothing to read when every entity is already known
        if wanted.is_empty() {
            return Ok(());
        }
        // read the wanted entities in this group in one batch
        let found = db::entities::get_many_untagged(&[group.to_owned()], &wanted, shared).await?;
        // remember each entity and the group it was found in
        for entity in found {
            match self.entities.entry(entity.id) {
                // add the group to an entity another group's read found
                Entry::Occupied(entry) => {
                    entry.into_mut().groups.push(group.to_owned());
                }
                // remember an entity no read found yet
                Entry::Vacant(entry) => {
                    entry.insert(entity);
                }
            }
        }
        Ok(())
    }

    /// Read the tools and tool instances backing some stages, and the latest instance of each
    /// tool, in at most two batches
    ///
    /// # Arguments
    ///
    /// * `group` - The group of the pipeline
    /// * `ids` - The Tool or Tool Instance ids backing the stages
    /// * `shared` - Shared Thorium objects
    pub async fn prefetch_stages(
        &mut self,
        group: &str,
        ids: &[Uuid],
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // read the entities backing the stages
        self.prefetch(group, ids, shared).await?;
        // find the latest instance of each tool among them
        let latest = ids
            .iter()
            .filter_map(|id| self.known(group, *id))
            .filter_map(|entity| match &entity.metadata {
                EntityMetadata::Tool(tool) => tool.latest,
                _ => None,
            })
            .collect::<Vec<Uuid>>();
        // read those instances in one more batch
        self.prefetch(group, &latest, shared).await
    }

    /// Get a tool or tool instance in a group, reading it only when no read so far found it
    /// in that group
    ///
    /// A request can span groups, so an entity remembered from a read in other groups is read
    /// again in this group, and the group is added to the remembered entity's groups.
    ///
    /// # Arguments
    ///
    /// * `group` - The group the entity must be in
    /// * `id` - The id of the entity
    /// * `shared` - Shared Thorium objects
    async fn entity(
        &mut self,
        group: &str,
        id: Uuid,
        shared: &Shared,
    ) -> Result<&Entity, ApiError> {
        // read the entity unless a read so far found it in this group
        let entity = match self.entities.entry(id) {
            Entry::Occupied(entry) if entry.get().groups.iter().any(|known| known == group) => {
                entry.into_mut()
            }
            Entry::Occupied(entry) => {
                // the read fails unless the entity is in this group
                db::entities::get_untagged(&[group.to_owned()], id, shared).await?;
                // later lookups in this group are answered without another read
                let entity = entry.into_mut();
                entity.groups.push(group.to_owned());
                entity
            }
            Entry::Vacant(entry) => {
                entry.insert(db::entities::get_untagged(&[group.to_owned()], id, shared).await?)
            }
        };
        Ok(entity)
    }

    /// Resolve the tool backing a stage to the tool instance it runs
    ///
    /// A Tool resolves to its latest instance and a Tool Instance to itself.
    ///
    /// # Arguments
    ///
    /// * `group` - The group of the pipeline
    /// * `stage` - The name of the stage
    /// * `id` - The Tool or Tool Instance id backing the stage
    /// * `shared` - Shared Thorium objects
    pub async fn resolve(
        &mut self,
        group: &str,
        stage: &str,
        id: Uuid,
        shared: &Shared,
    ) -> Result<Uuid, ApiError> {
        // get the entity backing this stage
        let entity = self.entity(group, id, shared).await?;
        // a tool runs its latest instance and an instance runs itself
        let instance = match &entity.metadata {
            EntityMetadata::Tool(tool) => match tool.latest {
                Some(latest) => latest,
                None => {
                    return bad!(format!(
                        "stage `{stage}`: tool `{}` has no version to run",
                        entity.name
                    ));
                }
            },
            EntityMetadata::ToolInstance(_) => id,
            _ => {
                return bad!(format!(
                    "stage `{stage}` must be backed by a Tool or Tool Instance, not a {}",
                    entity.kind
                ));
            }
        };
        // make sure the instance runs a workload
        self.job_info(group, stage, instance, shared).await?;
        Ok(instance)
    }

    /// Get the job settings of a tool instance's workload
    ///
    /// # Arguments
    ///
    /// * `group` - The group the tool instance must be in
    /// * `stage` - The name of the stage that runs it
    /// * `instance` - The id of the tool instance
    /// * `shared` - Shared Thorium objects
    pub async fn job_info(
        &mut self,
        group: &str,
        stage: &str,
        instance: Uuid,
        shared: &Shared,
    ) -> Result<ImageJobInfo, ApiError> {
        // get the tool instance
        let entity = self.entity(group, instance, shared).await?;
        // only Image and Harness tool instances run a workload
        match &entity.metadata {
            EntityMetadata::ToolInstance(meta) => match &meta.spec {
                ToolSpec::Image(workload) => Ok(workload.job_info()),
                ToolSpec::Harness(harness) => Ok(harness.workload.job_info()),
                ToolSpec::Agentic(_) => bad!(format!(
                    "stage `{stage}`: tool instance `{}` is an Agentic tool instance, which has no workload to run",
                    entity.name
                )),
            },
            _ => bad!(format!(
                "stage `{stage}` must run a Tool Instance, not a {}",
                entity.kind
            )),
        }
    }

    /// Get the provenance of the tool instances a plan runs from the instances already read
    ///
    /// A plan's tool instances are all read while it's resolved, so this reads nothing.
    ///
    /// # Arguments
    ///
    /// * `plan` - The resolved plan
    #[must_use]
    pub fn provenance(&self, plan: &ReactionPlan) -> RunProvenance {
        // describe each planned tool instance this request already read
        let instances = plan.tool_instances();
        RunProvenance::from_tools(instances.iter().filter_map(|id| self.entities.get(id)))
    }

    /// Get the job settings of every tool instance in a step of a plan, by stage name
    ///
    /// # Arguments
    ///
    /// * `group` - The group of the reaction
    /// * `step` - The stages of the step
    /// * `shared` - Shared Thorium objects
    pub async fn step_info<'a>(
        &mut self,
        group: &str,
        step: &'a [PlannedStage],
        shared: &Shared,
    ) -> Result<HashMap<&'a String, ImageJobInfo>, ApiError> {
        // read the step's tool instances in one batch
        let ids = step
            .iter()
            .map(|stage| stage.tool_instance)
            .collect::<Vec<Uuid>>();
        self.prefetch(group, &ids, shared).await?;
        // get each stage's tool instance settings
        let mut info = HashMap::with_capacity(step.len());
        for stage in step {
            let job = self
                .job_info(group, &stage.name, stage.tool_instance, shared)
                .await?;
            info.insert(&stage.name, job);
        }
        Ok(info)
    }
}

/// Resolve every stage of a pipeline to the tool instance a new reaction runs
///
/// A pipeline with a stage that no tool backs is refused, naming those stages.
///
/// # Arguments
///
/// * `pipeline` - The pipeline the reaction is for
/// * `memo` - The tools already read for this request
/// * `shared` - Shared Thorium objects
#[instrument(name = "stage_tools::resolve_plan", skip_all, fields(pipeline = &pipeline.name), err(Debug))]
pub async fn resolve_plan(
    pipeline: &Pipeline,
    memo: &mut ToolInstanceMemo,
    shared: &Shared,
) -> Result<ReactionPlan, ApiError> {
    // refuse pipelines with stages that no tool backs yet
    let unconverted = pipeline.unconverted_stages();
    if !unconverted.is_empty() {
        return bad!(format!(
            "pipeline `{}` has stages with no tool: {}; set them with `thorctl pipelines stage-tool`",
            pipeline.name,
            quoted(unconverted)
        ));
    }
    // read the tools and instances backing every stage in at most two batches
    let ids = pipeline
        .stage_tools
        .values()
        .copied()
        .collect::<Vec<Uuid>>();
    memo.prefetch_stages(&pipeline.group, &ids, shared).await?;
    // resolve each stage of each step
    let mut stages = Vec::with_capacity(pipeline.order.len());
    for step in &pipeline.order {
        let mut planned = Vec::with_capacity(step.len());
        for stage in step {
            // every stage has a tool since none are unconverted
            let id = pipeline.stage_tools[stage];
            let tool_instance = memo.resolve(&pipeline.group, stage, id, shared).await?;
            planned.push(PlannedStage {
                name: stage.clone(),
                tool_instance,
            });
        }
        stages.push(planned);
    }
    Ok(ReactionPlan { stages })
}

/// Quote and join names for an error message
///
/// # Arguments
///
/// * `names` - The names to quote
fn quoted<'a, I: IntoIterator<Item = &'a String>>(names: I) -> String {
    // wrap each name in backticks and join them
    names
        .into_iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<String>>()
        .join(", ")
}

/// Get the scaler of the workload a tool instance entity runs, if it runs one
///
/// # Arguments
///
/// * `entity` - The tool instance
fn instance_scaler(entity: &Entity) -> Option<ImageScaler> {
    // only Image and Harness tool instances run a workload
    match &entity.metadata {
        EntityMetadata::ToolInstance(meta) => match &meta.spec {
            ToolSpec::Image(workload) => Some(workload.environment.scaler()),
            ToolSpec::Harness(harness) => Some(harness.workload.environment.scaler()),
            ToolSpec::Agentic(_) => None,
        },
        _ => None,
    }
}

/// Check the tools a pipeline's stages name
///
/// Each must be a Tool or an Image or Harness Tool Instance in the pipeline's group that the user
/// can see, a Tool must have a version to run, and the user must be a developer for the scaler of
/// each stage's workload. A missing or invisible tool is `404`, the wrong kind or a tool outside
/// the pipeline's group is `400`, and a missing developer role is `401`; each error names the
/// stage.
///
/// # Arguments
///
/// * `user` - The user creating or updating the pipeline
/// * `group` - The group of the pipeline
/// * `stage_tools` - The stage tools to check, by stage name
/// * `shared` - Shared Thorium objects
#[instrument(name = "stage_tools::check", skip_all, fields(group = &group.name), err(Debug))]
pub async fn check(
    user: &User,
    group: &Group,
    stage_tools: &BTreeMap<String, Uuid>,
    shared: &Shared,
) -> Result<(), ApiError> {
    // there is nothing to check without stage tools
    if stage_tools.is_empty() {
        return Ok(());
    }
    // read the stages' tools and their latest instances in the pipeline's group in one batch
    let mut memo = ToolInstanceMemo::default();
    let ids = stage_tools.values().copied().collect::<Vec<Uuid>>();
    memo.prefetch_stages(&group.name, &ids, shared).await?;
    // a user in the pipeline's group sees every entity found in it
    let sees_group = user.is_admin() || user.groups.contains(&group.name);
    // the stages whose tool was found in the group as a tool or instance need no row lookup
    let answered = ids
        .iter()
        .copied()
        .filter(|id| {
            sees_group
                && memo.known(&group.name, *id).is_some_and(|entity| {
                    entity.kind == EntityKinds::Tool || entity.kind == EntityKinds::ToolInstance
                })
        })
        .collect::<HashSet<Uuid>>();
    // get the group and kind of every row of the other named entities in one lookup
    let unanswered = ids
        .iter()
        .filter(|id| !answered.contains(id))
        .copied()
        .collect::<Vec<Uuid>>();
    let rows = if unanswered.is_empty() {
        Vec::new()
    } else {
        db::entities::groups_and_kinds(&unanswered, shared).await?
    };
    // check each stage's tool
    for (stage, id) in stage_tools {
        // a stage answered by the batch passes the visibility, kind and group checks
        if !answered.contains(id) {
            // get this entity's rows
            let rows = rows.iter().filter(|row| row.id == *id).collect::<Vec<_>>();
            // a missing entity and one the user can't see get the same answer
            let visible =
                user.is_admin() || rows.iter().any(|row| user.groups.contains(&row.group));
            if rows.is_empty() || !visible {
                return not_found!(format!(
                    "stage `{stage}`: tool `{id}` doesn't exist or you can't see it"
                ));
            }
            // only tools and tool instances can back a stage
            let kind = rows[0].kind;
            if kind != EntityKinds::Tool && kind != EntityKinds::ToolInstance {
                return bad!(format!(
                    "stage `{stage}` must be backed by a Tool or Tool Instance, not a {kind}"
                ));
            }
            // the tool must be in the pipeline's group
            if !rows.iter().any(|row| row.group == group.name) {
                return bad!(format!(
                    "stage `{stage}`: tool `{id}` is not in the pipeline's group `{}`",
                    group.name
                ));
            }
        }
        // resolve the tool instance this stage runs and make sure it runs a workload
        let instance = memo.resolve(&group.name, stage, *id, shared).await?;
        // a tool must be an Image or Harness tool
        let entity = memo.entity(&group.name, instance, shared).await?;
        let Some(scaler) = instance_scaler(entity) else {
            return bad!(format!(
                "stage `{stage}`: tool `{}` has no workload to run",
                entity.name
            ));
        };
        // the user must be a developer for the stage's workload
        group.developer(user, scaler)?;
    }
    Ok(())
}
