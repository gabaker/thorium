//! Wrappers for interacting with status logs within Thorium with different backends
//! Currently only Redis is supported

use chrono::Utc;
use std::collections::HashMap;

use crate::models::{
    Actions, JobActions, RawJob, Reaction, ReactionActions, StatusRequest, StatusUpdate,
};
use crate::utils::ApiError;
use crate::{deserialize, force_serialize};

impl StatusRequest {
    /// Builds the update part of the status update based on a RawJob and an action
    ///
    /// # Arguments
    ///
    /// * `job` - The job this status update is for
    /// * `action` - The action that caused this status update
    pub(self) fn build_update_job(job: &RawJob, action: &JobActions) -> HashMap<String, String> {
        let mut update = HashMap::new();
        // inject job id
        update.insert("job".to_string(), job.id.to_string());

        // build correct update based on action
        match action {
            JobActions::Created => {
                update.insert("reaction".to_owned(), job.reaction.to_string());
                update.insert("id".to_owned(), job.id.to_string());
                update.insert("group".to_owned(), job.group.to_owned());
                update.insert("pipeline".to_owned(), job.pipeline.to_owned());
                update.insert("stage".to_owned(), job.stage.to_owned());
                update.insert("deadline".to_owned(), job.deadline.to_string());
                update.insert("status".to_owned(), force_serialize!(&job.status));
            }
            JobActions::Running => {
                update.insert("status".to_string(), "Running".to_string());
            }
            JobActions::Reset(_) => {
                update.insert("status".to_string(), "Created".to_string());
            }
            JobActions::Completed => {
                update.insert("status".to_string(), "Completed".to_string());
            }
            JobActions::Errored => {
                update.insert("status".to_string(), "Failed".to_string());
            }
        };
        // return update
        update
    }

    /// Build the update part of the status update based on a Reaction and an action
    ///
    /// # Arguments
    ///
    /// * `reaction` - The reaction this status update is for
    /// * `action` - The action that caused this status update to occur
    pub(self) fn build_update_reaction(
        reaction: &Reaction,
        action: &ReactionActions,
    ) -> HashMap<String, String> {
        let mut update = HashMap::new();

        match action {
            ReactionActions::Created => {
                update.insert("id".to_owned(), reaction.id.to_string());
                update.insert("group".to_owned(), reaction.group.to_owned());
                update.insert("pipeline".to_owned(), reaction.pipeline.to_owned());
                update.insert("status".to_owned(), force_serialize!(&reaction.status));
                update.insert(
                    "current_stage".to_owned(),
                    reaction.current_stage.to_string(),
                );
                update.insert("sla".to_owned(), reaction.sla.to_string());
            }
            ReactionActions::Proceeding => {
                update.insert(
                    "current_stage".to_owned(),
                    reaction.current_stage.to_string(),
                );
            }
            ReactionActions::Completed => {
                update.insert("status".to_owned(), "Completed".to_owned());
            }
            ReactionActions::Failed => {
                update.insert("status".to_owned(), "Failed".to_owned());
            }
            ReactionActions::Enabled => {
                update.insert("status".to_owned(), "Enabled".to_owned());
            }
            ReactionActions::Disabled => {
                update.insert("status".to_owned(), "Disabled".to_owned());
            }
        };
        update
    }

    /// Build a job claim status update based on a `RawJob` and a worker
    ///
    /// The update names the claimed job and its stage beside the worker, since a worker is named
    /// after its tool and may claim the jobs of any stage that runs that tool.
    ///
    /// # Arguments
    ///
    /// * `job` - The job that this update is for
    /// * `worker` - The worker that claimed this job
    pub fn claim_job<T: Into<String>>(job: &RawJob, worker: T) -> Self {
        // build a status update naming the claimed job, its stage, and the worker that claimed it
        let mut update = HashMap::with_capacity(4);
        update.insert("status".to_string(), "running".to_string());
        update.insert("worker".to_string(), worker.into());
        update.insert("job".to_string(), job.id.to_string());
        update.insert("stage".to_string(), job.stage.clone());
        // build a status request from this job claim
        StatusRequest {
            group: job.group.clone(),
            pipeline: job.pipeline.clone(),
            reaction: job.reaction.to_string(),
            action: Actions::JobRunning,
            update,
        }
    }

    /// Build a status update based on a RawJob and an action
    ///
    /// # Arguments
    ///
    /// * `job` - The job that this update is for
    /// * `action` - The action that caused this update
    pub fn from_job(job: &RawJob, action: JobActions) -> Self {
        // build the update for this action
        let update = Self::build_update_job(job, &action);
        // convert this action
        let action_cast = match action {
            JobActions::Created => Actions::JobCreated,
            JobActions::Running => Actions::JobRunning,
            JobActions::Reset(requestor) => Actions::JobReset(requestor),
            JobActions::Completed => Actions::JobCompleted,
            JobActions::Errored => Actions::JobFailed,
        };
        // build our status request
        StatusRequest {
            group: job.group.clone(),
            pipeline: job.pipeline.clone(),
            reaction: job.reaction.to_string(),
            action: action_cast,
            update,
        }
    }

    /// Build a status update based on a Reaction and an action
    ///
    /// # Arguments
    ///
    /// * `reaction` - The reaction that this update is for
    /// * `action` - The action that caused this update
    pub fn from_reaction(reaction: &Reaction, action: ReactionActions) -> Self {
        let action_cast = match action {
            ReactionActions::Created => Actions::ReactionCreated,
            ReactionActions::Completed => Actions::ReactionCompleted,
            ReactionActions::Proceeding => Actions::ReactionProceeding,
            ReactionActions::Failed => Actions::ReactionFailed,
            ReactionActions::Enabled => Actions::ReactionEnabled,
            ReactionActions::Disabled => Actions::ReactionDisabled,
        };

        StatusRequest {
            group: reaction.group.clone(),
            pipeline: reaction.pipeline.clone(),
            reaction: reaction.id.to_string(),
            action: action_cast,
            update: Self::build_update_reaction(reaction, &action),
        }
    }
}

impl StatusUpdate {
    /// Creates a status update object from a job
    ///
    /// # Arguments
    ///
    /// * `request` - The status update request to insert into the status log
    /// * `msg` - The message that we should include in this status update
    pub(super) fn new(request: StatusRequest, msg: Option<String>) -> Self {
        // create status update
        StatusUpdate {
            group: request.group.clone(),
            pipeline: request.pipeline.clone(),
            reaction: request.reaction.to_string(),
            timestamp: Utc::now(),
            action: request.action,
            msg,
            update: request.update,
        }
    }

    /// Okay wrap deserializing StatusUpdate since Error can't be deserialized
    ///
    /// # Arguments
    ///
    /// * `raw` - The string to deserialize into a status update
    pub fn deserialize(raw: &str) -> Result<Self, ApiError> {
        Ok(deserialize!(raw))
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use std::collections::HashMap;
    use uuid::Uuid;

    use crate::models::{Actions, ImageScaler, JobStatus, RawJob, StatusRequest, StatusUpdate};

    /// Build a queued job of a pipeline stage
    ///
    /// # Arguments
    ///
    /// * `stage` - The stage of the job
    fn job(stage: &str) -> RawJob {
        // a created job with fixed ids so updates can be compared
        RawJob {
            group: "jupiter".to_owned(),
            pipeline: "orbit".to_owned(),
            reaction: Uuid::from_u128(1),
            stage: stage.to_owned(),
            creator: "europa".to_owned(),
            id: Uuid::from_u128(2),
            args: "{}".to_owned(),
            status: JobStatus::Created,
            deadline: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            worker: None,
            parent: None,
            generator: false,
            scaler: ImageScaler::K8s,
            samples: Vec::new(),
            ephemeral: Vec::new(),
            parent_ephemeral: HashMap::new(),
            repos: Vec::new(),
            trigger_depth: None,
            tool_instance: Uuid::from_u128(3),
            run: None,
        }
    }

    /// A claim's status update names the claimed job and its stage beside the worker and status
    #[test]
    fn claim_update_names_the_job_and_stage() {
        // a worker named after its tool claims the job of a stage named otherwise
        let job = job("stub-stage");
        let request = StatusRequest::claim_job(&job, "jup-stub-tool-1-0-0-x1");
        // the update is a job running update in the job's reaction
        assert!(matches!(request.action, Actions::JobRunning));
        assert_eq!(request.group, "jupiter");
        assert_eq!(request.pipeline, "orbit");
        assert_eq!(request.reaction, job.reaction.to_string());
        // it names the job, its stage, the worker, and the running status
        let expected = [
            ("job", job.id.to_string()),
            ("stage", "stub-stage".to_owned()),
            ("worker", "jup-stub-tool-1-0-0-x1".to_owned()),
            ("status", "running".to_owned()),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect::<HashMap<String, String>>();
        assert_eq!(request.update, expected);
    }

    /// Claim updates read back from a status log both with their job and stage and, as older
    /// status logs hold them, with only their worker
    #[test]
    fn claim_updates_read_back_with_or_without_a_job() {
        // a claim update keeps its job and stage through the status log's JSON
        let job = job("stub-stage");
        let update = StatusUpdate::new(
            StatusRequest::claim_job(&job, "jup-stub-tool-1-0-0-x1"),
            None,
        );
        let raw = serde_json::to_string(&update).expect("a serialized claim update");
        let read = StatusUpdate::deserialize(&raw).expect("a claim update");
        assert!(matches!(read.action, Actions::JobRunning));
        assert_eq!(read.update.get("job"), Some(&job.id.to_string()));
        assert_eq!(
            read.update.get("stage").map(String::as_str),
            Some("stub-stage")
        );
        // a claim update naming only its worker still reads
        let older = r#"{"group":"jupiter","pipeline":"orbit","reaction":"00000000-0000-0000-0000-000000000001","action":"JobRunning","timestamp":"2026-10-28T10:32:58.529118835Z","msg":null,"update":{"status":"running","worker":"jup-stub-tool-1-0-0-x1"}}"#;
        let read = StatusUpdate::deserialize(older).expect("an older claim update");
        assert!(matches!(read.action, Actions::JobRunning));
        assert_eq!(
            read.update.get("worker").map(String::as_str),
            Some("jup-stub-tool-1-0-0-x1")
        );
        assert!(!read.update.contains_key("job"));
        assert!(!read.update.contains_key("stage"));
    }
}
