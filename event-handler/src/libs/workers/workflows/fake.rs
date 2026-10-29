//! An instance API over an in-memory graph for the workflows worker's unit tests

use chrono::Utc;
use std::collections::BTreeSet;
use std::sync::Mutex;
use thorium::Error;
use thorium::models::entities::workflows::MemoryGraph;
use thorium::models::{
    Entity, EntityKinds, EntityMetadata, Event, EventData, NotificationRequest,
    ToolSetInstanceEntity, User, WorkflowGraphChange, WorkflowGraphEvent, WorkflowInstanceEntity,
};
use uuid::Uuid;

use super::InstanceApi;

/// The kind of instance a create made
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Created {
    /// A toolset instance
    ToolSet,
    /// A workflow instance
    Workflow,
}

/// An instance API over an in-memory graph that records what it's asked to write
#[derive(Default)]
pub struct FakeApi {
    /// The graph every read goes through
    pub graph: MemoryGraph,
    /// Every workflow instance created
    pub created: Mutex<Vec<WorkflowInstanceEntity>>,
    /// Every toolset instance created
    pub created_tool_sets: Mutex<Vec<ToolSetInstanceEntity>>,
    /// The kind of every instance created, in the order they were created
    pub order: Mutex<Vec<Created>>,
    /// Every notification sent, with its recipient
    pub notes: Mutex<Vec<(String, NotificationRequest<User>)>>,
    /// The users whose notifications fail
    pub failing_notes: BTreeSet<String>,
    /// How many creates are refused with a revision conflict before one succeeds
    pub conflicts: Mutex<usize>,
}

impl FakeApi {
    /// Get the workflow instances created so far
    pub fn created(&self) -> Vec<WorkflowInstanceEntity> {
        // copy the recorded creates
        self.created.lock().unwrap().clone()
    }

    /// Get the toolset instances created so far
    pub fn created_tool_sets(&self) -> Vec<ToolSetInstanceEntity> {
        // copy the recorded creates
        self.created_tool_sets.lock().unwrap().clone()
    }

    /// Get the kind of every instance created so far, in the order they were created
    pub fn order(&self) -> Vec<Created> {
        // copy the recorded order
        self.order.lock().unwrap().clone()
    }

    /// Get the notifications sent so far
    pub fn notes(&self) -> Vec<(String, NotificationRequest<User>)> {
        // copy the recorded notifications
        self.notes.lock().unwrap().clone()
    }

    /// Refuse a create with a revision conflict while conflicts are left
    fn refuse(&self) -> Result<(), Error> {
        // take one conflict if any are left
        let refused = {
            let mut left = self.conflicts.lock().unwrap();
            let refused = *left > 0;
            *left = left.saturating_sub(1);
            refused
        };
        if refused {
            return Err(Error::Thorium {
                code: reqwest::StatusCode::CONFLICT,
                msg: Some("revision conflict".to_owned()),
            });
        }
        Ok(())
    }
}

// the trait's futures are async, though the in-memory writes never wait
#[allow(clippy::unused_async_trait_impl)]
impl InstanceApi for FakeApi {
    /// The fake reads the in-memory graph it writes to
    type Reader = MemoryGraph;

    /// Get the in-memory graph
    fn reader(&self) -> &MemoryGraph {
        &self.graph
    }

    /// Record a created workflow instance, after refusing as many creates as were asked for
    ///
    /// # Arguments
    ///
    /// * `instance` - The instance to create
    async fn create_workflow_instance(
        &self,
        instance: &WorkflowInstanceEntity,
    ) -> Result<Uuid, Error> {
        // refuse a create while conflicts are left
        self.refuse()?;
        // record the instance and when it was created
        self.created.lock().unwrap().push(instance.clone());
        self.order.lock().unwrap().push(Created::Workflow);
        Ok(Uuid::new_v4())
    }

    /// Record a created toolset instance, after refusing as many creates as were asked for
    ///
    /// # Arguments
    ///
    /// * `instance` - The instance to create
    async fn create_tool_set_instance(
        &self,
        instance: &ToolSetInstanceEntity,
    ) -> Result<Uuid, Error> {
        // refuse a create while conflicts are left
        self.refuse()?;
        // record the instance and when it was created
        self.created_tool_sets
            .lock()
            .unwrap()
            .push(instance.clone());
        self.order.lock().unwrap().push(Created::ToolSet);
        Ok(Uuid::new_v4())
    }

    /// Record a notification, or fail it for a failing user
    ///
    /// # Arguments
    ///
    /// * `username` - The user to notify
    /// * `req` - The notification
    async fn notify(&self, username: &str, req: &NotificationRequest<User>) -> Result<(), Error> {
        // a failing user's notification fails
        if self.failing_notes.contains(username) {
            return Err(Error::Thorium {
                code: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                msg: Some("notification failed".to_owned()),
            });
        }
        // record the notification
        self.notes
            .lock()
            .unwrap()
            .push((username.to_owned(), req.clone()));
        Ok(())
    }
}

/// Build an entity
///
/// # Arguments
///
/// * `kind` - The entity's kind
/// * `name` - The entity's name
/// * `groups` - The entity's groups
/// * `metadata` - The entity's metadata
pub fn entity(kind: EntityKinds, name: &str, groups: &[&str], metadata: EntityMetadata) -> Entity {
    // a fresh entity with no tags
    Entity {
        id: Uuid::new_v4(),
        name: name.to_owned(),
        kind,
        metadata,
        description: None,
        submitter: "thorium".to_owned(),
        groups: groups.iter().map(|group| (*group).to_owned()).collect(),
        tags: std::collections::HashMap::default(),
        image: None,
        created: Utc::now(),
    }
}

/// Build a workflow graph event
///
/// # Arguments
///
/// * `user` - The user who caused the event
/// * `groups` - The groups of the changed entity
/// * `change` - What changed
pub fn event(user: &str, groups: &[&str], change: WorkflowGraphChange) -> Event {
    // a fresh event with no parent
    Event {
        id: Uuid::new_v4(),
        timestamp: Utc::now(),
        parent: None,
        user: user.to_owned(),
        data: EventData::WorkflowGraph(WorkflowGraphEvent {
            groups: groups.iter().map(|group| (*group).to_owned()).collect(),
            change,
        }),
        depth: 0,
    }
}
