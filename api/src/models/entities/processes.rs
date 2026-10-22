//! A process extracted from a memory dump, VM, or other running system
//!
//! Processes have different metadata depending on what operating system they
//! originated from:
//!
//! Windows - [`WindowsProcessTree`] / [`WindowsProcessEntity`]

use chrono::prelude::*;
use futures::{StreamExt, stream};
use std::collections::HashMap;
use uuid::Uuid;

#[cfg(feature = "client")]
use crate::models::{
    AssociationKind, AssociationRequest, AssociationTarget, EntityMetadataRequest, EntityRequest,
};

/// The root entity for a tree of processes in windows
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash)]
#[serde(from = "WindowsProcessTreeEntityCompat")]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct WindowsProcessTreeEntity {
    /// The tool instances that dropped this process tree
    pub tools: Vec<Uuid>,
    /// The names of tools that dropped this process tree that are not linked to a tool instance
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub legacy_tools: Vec<String>,
}

/// The stored shape of a process tree, accepting tool names written before tool instances
#[derive(Debug, Clone, Default, Deserialize)]
struct WindowsProcessTreeEntityCompat {
    /// The tools that dropped this process tree as tool instance IDs or legacy tool names
    #[serde(default)]
    tools: Vec<String>,
    /// The names of tools that are not linked to a tool instance
    #[serde(default)]
    legacy_tools: Vec<String>,
}

impl From<WindowsProcessTreeEntityCompat> for WindowsProcessTreeEntity {
    /// Convert a stored process tree into a process tree entity
    ///
    /// # Arguments
    ///
    /// * `compat` - The stored process tree
    fn from(compat: WindowsProcessTreeEntityCompat) -> Self {
        // split the stored tools into tool instances and legacy names
        let (tools, legacy_tools) =
            super::shared::split_tool_refs(compat.tools, compat.legacy_tools);
        WindowsProcessTreeEntity {
            tools,
            legacy_tools,
        }
    }
}

impl WindowsProcessTreeEntity {
    /// Create a [`WindowsProcessTreeBuilder`]
    ///
    /// # Arguments
    ///
    /// * `name` - The name to set for this windows process tree
    pub fn builder(name: impl Into<String>) -> WindowsProcessTreeBuilder {
        WindowsProcessTreeBuilder {
            name: name.into(),
            tools: Vec::default(),
            processes: HashMap::default(),
        }
    }

    /// Create a new [`WindowsProcessTreeEntity`] entity with the info in the form
    ///
    /// # Arguments
    ///
    /// * `form` -  The update form
    #[cfg(feature = "api")]
    pub fn from_form(form: super::EntityMetadataForm) -> Result<Self, crate::utils::ApiError> {
        // record each tool instance once in the order it was sent
        let mut tools = Vec::with_capacity(form.tool_ids.len());
        super::shared::add_tool_ids(&mut tools, form.tool_ids);
        // build our [`WindowsProcessTreeEntity`] entity; legacy tool names are never written by
        // a form
        Ok(WindowsProcessTreeEntity {
            tools,
            legacy_tools: Vec::new(),
        })
    }

    /// Get the form fields this process tree sends, by name
    ///
    /// Legacy tool names are never sent, since no form writes them.
    #[must_use]
    pub fn form_fields(&self) -> Vec<(&'static str, String)> {
        // the kind of this entity
        let mut fields = vec![(
            "kind",
            super::EntityKinds::WindowsProcessTree.as_str().to_owned(),
        )];
        // the tool instances that dropped this process tree
        fields.extend(
            self.tools
                .iter()
                .map(|tool| ("metadata[tool_ids][]", tool.to_string())),
        );
        fields
    }

    /// Add this windows process tree entity metadata to a form
    ///
    /// # Arguments
    ///
    /// * `form` - The form to add too
    #[cfg(feature = "client")]
    pub fn add_to_form(
        self,
        form: reqwest::multipart::Form,
    ) -> Result<reqwest::multipart::Form, crate::Error> {
        // add each of our fields to the form
        let form = self
            .form_fields()
            .into_iter()
            .fold(form, |form, (name, value)| form.text(name, value));
        Ok(form)
    }
}

/// A windows process
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct WindowsProcessEntity {
    /// This processes id
    pub pid: u64,
    /// This processes parent PID
    pub parent_pid: Option<u64>,
    /// The name of the executable for this processes
    pub name: Option<String>,
    /// The path to this executable
    pub image_path: Option<String>,
    /// The full cmd for this process
    pub command: Option<String>,
    /// The offset for this process
    pub offset: Option<u64>,
    /// the number of threads this process spawned
    pub threads: Option<u32>,
    /// The number of handles this process had open
    pub handles: Option<u32>,
    /// Whether this process is using the wow64 emulator or not
    pub is_wow64: Option<bool>,
    /// The session id for this process
    pub session_id: Option<u32>,
    /// When this process was spawned (not created in Thorium)
    pub create_time: Option<DateTime<Utc>>,
    /// When this process exited
    pub exit_time: Option<DateTime<Utc>>,
}

impl WindowsProcessEntity {
    /// Create a new windows process entity
    ///
    /// This will not save this entity to Thorium.
    ///
    /// # Arguments
    ///
    /// * `pid` - The pid for this process
    pub fn new(pid: u64) -> Self {
        // create our process entity with only a pid set
        WindowsProcessEntity {
            pid,
            parent_pid: None,
            name: None,
            image_path: None,
            command: None,
            offset: None,
            threads: None,
            handles: None,
            is_wow64: None,
            session_id: None,
            create_time: None,
            exit_time: None,
        }
    }

    /// Set the parent pid for this [`WindowsProcessEntity`]
    ///
    /// # Arguments
    ///
    /// * `parent_pid` - The parent pid for this process
    pub fn parent_pid(mut self, parent_pid: u64) -> Self {
        self.parent_pid = Some(parent_pid);
        self
    }

    /// Set the executable/image name for this [`WindowsProcessEntity`]
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the executable for this process
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Set the path to the executable for this [`WindowsProcessEntity`]
    ///
    /// # Arguments
    ///
    /// * `image_path` - The image_path to the executable for this process
    pub fn image_path(mut self, image_path: impl Into<String>) -> Self {
        self.image_path = Some(image_path.into());
        self
    }

    /// Set the command for this [`WindowsProcessEntity`]
    ///
    /// # Arguments
    ///
    /// * `command` - The command for this process
    pub fn command(mut self, command: impl Into<String>) -> Self {
        self.command = Some(command.into());
        self
    }

    /// Set the offset for this process in virtual memory for this [`WindowsProcessEntity`]
    ///
    /// # Arguments
    ///
    /// * `offset` - The offset for this process in virtual memory
    pub fn offset(mut self, offset: u64) -> Self {
        self.offset = Some(offset);
        self
    }

    /// Set the thread count for this [`WindowsProcessEntity`]
    ///
    /// # Arguments
    ///
    /// * `threads` - The numer of active threads for this process
    pub fn threads(mut self, threads: u32) -> Self {
        self.threads = Some(threads);
        self
    }

    /// Set the open handle count for this [`WindowsProcessEntity`]
    ///
    /// # Arguments
    ///
    /// * `threads` - The numer of open file handles for this process
    pub fn handles(mut self, handles: u32) -> Self {
        self.handles = Some(handles);
        self
    }

    /// Set whether this process is using the 64 bit emulator
    ///
    /// If this is true then this executable is 32 bit.
    ///
    /// # Arguments
    ///
    /// * `is_wow64` - Whether this process is using the wow64 emulator or not
    pub fn is_wow64(mut self, is_wow64: bool) -> Self {
        self.is_wow64 = Some(is_wow64);
        self
    }

    /// Set the session id for this process
    ///
    /// Session IDs tell you who/what generally spawned this process.
    ///
    /// 0 = services
    /// 1+ = user spawned (desktop or RDP)
    ///
    /// # Arguments
    ///
    /// * `session_id` - The session id to set for this process
    pub fn session_id(mut self, session_id: u32) -> Self {
        self.session_id = Some(session_id);
        self
    }

    /// Set the time this process was created.
    ///
    /// This is the processes create/spawn time not when it was added to Thorium.
    ///
    /// # Arguments
    ///
    /// * `create_time` - The timestamp for when this process was created
    pub fn create_time(mut self, create_time: DateTime<Utc>) -> Self {
        self.create_time = Some(create_time);
        self
    }

    /// Set the time this process exited.
    ///
    /// # Arguments
    ///
    /// * `exit_time` - The timestamp for when this process exited
    pub fn exit_time(mut self, exit_time: DateTime<Utc>) -> Self {
        self.exit_time = Some(exit_time);
        self
    }

    /// Create a new [`WindowsProcess`] with the info in the form
    ///
    /// # Errors
    ///
    /// * A pid was not found in the form
    ///
    /// # Arguments
    ///
    /// * `form` -  The update form
    #[cfg(feature = "api")]
    pub fn from_form(form: super::EntityMetadataForm) -> Result<Self, crate::utils::ApiError> {
        // if we don't have the pid field then return an error
        let pid = match form.pid {
            Some(pid) => pid,
            None => {
                return crate::bad!("Windows process entities must have a pid!".to_owned());
            }
        };
        // build our windows process entity
        Ok(WindowsProcessEntity {
            pid,
            parent_pid: form.parent_pid,
            name: form.name,
            image_path: form.image_path,
            command: form.command,
            offset: form.offset,
            handles: form.handles,
            threads: form.threads,
            is_wow64: form.is_wow64,
            session_id: form.session_id,
            create_time: form.create_time,
            exit_time: form.exit_time,
        })
    }

    /// Add this [`WindowsProcessEntity`]'s metadata to a form
    ///
    /// # Arguments
    ///
    /// * `form` - The form to add too
    #[cfg(feature = "client")]
    pub fn add_to_form(
        mut self,
        form: reqwest::multipart::Form,
    ) -> Result<reqwest::multipart::Form, crate::Error> {
        // always set our entity kind
        let form = form
            .text("kind", super::EntityKinds::WindowsProcess.as_str())
            .text("metadata[pid]", self.pid.to_string());
        // set the parent pid for this process if we have it
        let form = crate::multipart_text_to_string!(form, "metadata[parent_pid]", self.parent_pid);
        // set our executable field
        let form = crate::multipart_text!(form, "metadata[name]", self.name);
        // set our path field
        let form = crate::multipart_text!(form, "metadata[image_path]", self.image_path);
        // set our cmd field
        let form = crate::multipart_text!(form, "metadata[command]", self.command);
        // convert and set our offset field
        let form = crate::multipart_text_to_string!(form, "metadata[offset]", self.offset);
        // convert and set our threads field
        let form = crate::multipart_text_to_string!(form, "metadata[threads]", self.threads);
        // convert and set our handles field
        let form = crate::multipart_text_to_string!(form, "metadata[handles]", self.handles);
        // convert and set our is_wow64 field
        let form = crate::multipart_text_to_string!(form, "metadata[is_wow64]", self.is_wow64);
        // convert and set our session_id field
        let form = crate::multipart_text_to_string!(form, "metadata[session_id]", self.session_id);
        // if we have a create time then serialize and set it
        let form = crate::multipart_date!(form, "metadata[create_time]", self.create_time);
        // if we have a exit time then serialize and set it
        let form = crate::multipart_date!(form, "metadata[exit_time]", self.exit_time);
        Ok(form)
    }
}

/// Build a request for this Windows process entity
///
/// # Arguments
///
/// * `pid` - The id of the process to create
/// * `process` - The entity for the process to create
/// * `groups` - The groups to set for this process request
fn create_windows_process_req(
    pid: u64,
    process: WindowsProcessEntity,
    groups: Vec<String>,
) -> EntityRequest {
    // get our name from one of several possible values
    let name = match (&process.name, &process.command, &process.image_path) {
        (Some(name), _, _) => name.clone(),
        (None, Some(command), _) => command.clone(),
        (None, None, Some(image_path)) => image_path.clone(),
        (None, None, None) => format!("Process {pid}"),
    };
    // build the metadata request for this process
    let metadata = EntityMetadataRequest::WindowsProcess(WindowsProcessEntity {
        pid,
        parent_pid: process.parent_pid,
        name: process.name,
        image_path: process.image_path,
        command: process.command,
        offset: process.offset,
        threads: process.threads,
        handles: process.handles,
        is_wow64: process.is_wow64,
        session_id: process.session_id,
        create_time: process.create_time,
        exit_time: process.exit_time,
    });
    // build the process entity request
    EntityRequest::new(name, metadata, groups)
}

/// Build a request for this Windows process entity with empty groups
///
/// # Arguments
///
/// * `pid` - The id of the process to create
/// * `process` - The entity for the process to create
fn create_windows_process_req_empty_groups(
    pid: u64,
    process: WindowsProcessEntity,
) -> EntityRequest {
    // get our name from one of several possible values
    let name = match (&process.name, &process.command, &process.image_path) {
        (Some(name), _, _) => name.clone(),
        (None, Some(command), _) => command.clone(),
        (None, None, Some(image_path)) => image_path.clone(),
        (None, None, None) => format!("Process {pid}"),
    };
    // build the metadata request for this process
    let metadata = EntityMetadataRequest::WindowsProcess(WindowsProcessEntity {
        pid,
        parent_pid: process.parent_pid,
        name: process.name,
        image_path: process.image_path,
        command: process.command,
        offset: process.offset,
        threads: process.threads,
        handles: process.handles,
        is_wow64: process.is_wow64,
        session_id: process.session_id,
        create_time: process.create_time,
        exit_time: process.exit_time,
    });
    // don't specify groups in our stored entity requests
    let groups = Vec::<String>::default();
    // build the process entity request
    EntityRequest::new(name, metadata, groups)
}

/// Create this windows process in Thorium
///
/// # Arguments
///
/// * `pid` - The id of the process to create
/// * `process` - The entity for the process to create
/// * `groups` - The groups to create this process in
/// * `thorium` - A Thorium client
#[cfg(feature = "client")]
async fn create_windows_process_helper(
    pid: u64,
    process: WindowsProcessEntity,
    groups: &[String],
    thorium: &crate::Thorium,
) -> Result<(u64, Option<u64>, String, uuid::Uuid), crate::Error> {
    // get our parent process id
    let parent_pid = process.parent_pid;
    // build the request for this windows process
    let entity_req = create_windows_process_req(pid, process, groups.to_vec());
    // get our new entities name
    let name = entity_req.name.clone();
    // create this entity
    let resp = thorium.entities.create(entity_req).await?;
    // return our pid, our parent pid, and this processes entity id
    Ok((pid, parent_pid, name, resp.id))
}

/// A single entry into a windows process trees pid_map context
///
/// The format for this is (<parent_pid>, <entity_name>, <entity_id>)
#[cfg(feature = "client")]
type PidMapEntry = (Option<u64>, String, uuid::Uuid);

/// The context needed to properly build a windows process tree
#[cfg(feature = "client")]
struct WindowsProcTreeContext {
    /// The id for our root windows process tree
    id: uuid::Uuid,
    /// A map of pids and their parent, entity names/ids
    pid_map: HashMap<u64, PidMapEntry>,
}

/// Constructs a windows process tree that a user wants to submit to Thorium
#[derive(Debug)]
#[cfg(feature = "client")]
pub struct WindowsProcessTreeBuilder {
    /// The name for this windows process tree to build
    pub name: String,
    /// The tool instances that are generating this process tree
    pub tools: Vec<Uuid>,
    /// A map of processes by their PID
    pub processes: HashMap<u64, WindowsProcessEntity>,
}

#[cfg(feature = "client")]
impl WindowsProcessTreeBuilder {
    /// Add a tool instance that dumped this windows process tree
    ///
    /// # Arguments
    ///
    /// * `tool` - The tool instance to add
    #[must_use]
    pub fn tool(mut self, tool: Uuid) -> Self {
        // add this tool instance
        self.tools.push(tool);
        self
    }

    /// Build the metadata of this process tree's root entity, which records the tool instances
    /// that dumped it
    #[must_use]
    pub fn root_metadata(&self) -> EntityMetadataRequest {
        // the root records every tool instance that dumped this process tree
        EntityMetadataRequest::WindowsProcessTree(WindowsProcessTreeEntity {
            tools: self.tools.clone(),
            legacy_tools: Vec::new(),
        })
    }

    /// Add a new process to this process tree
    ///
    /// # Arguments
    ///
    /// * `process` - The process to add
    pub fn add_mut(&mut self, process: WindowsProcessEntity) {
        // get this processes pid
        let pid = process.pid;
        // add this process to our process map
        self.processes.insert(pid, process);
    }

    /// Create a root process tree entity
    ///
    /// # Arguments
    ///
    /// * `groups` - The groups to add this process tree to
    /// * `thorium` - A Thorium client
    async fn create_root(
        &mut self,
        groups: &[String],
        thorium: &crate::Thorium,
    ) -> Result<WindowsProcTreeContext, crate::Error> {
        // build the entity request for our root with the tool instances that dumped it
        let req = EntityRequest::new(&self.name, self.root_metadata(), groups);
        // create our root process entity
        let resp = thorium.entities.create(req).await?;
        // instance a pid map of the correct size
        let pid_map = HashMap::with_capacity(self.processes.len());
        // build our builder context
        let context = WindowsProcTreeContext {
            id: resp.id,
            pid_map,
        };
        Ok(context)
    }

    /// Create all of the process entities
    ///
    /// # Arguments
    ///
    /// * `context` - The context to use when bulding this process tree
    /// * `groups` - The groups to add process entities to
    /// * `thorium` - A Thorium client
    async fn create_entities(
        &mut self,
        context: &mut WindowsProcTreeContext,
        groups: &[String],
        thorium: &crate::Thorium,
    ) -> Result<(), crate::Error> {
        // create all of our windows processes 10 at a time but do not link them
        let creates = stream::iter(self.processes.drain())
            .map(|(pid, process)| create_windows_process_helper(pid, process, groups, thorium))
            .buffer_unordered(10)
            .collect::<Vec<Result<(u64, Option<u64>, String, uuid::Uuid), _>>>()
            .await;
        // process our process create stream
        for result in creates {
            // get this pids info
            let (pid, parent_pid, name, entity_id) = result?;
            // add this pid to our pid map
            context.pid_map.insert(pid, (parent_pid, name, entity_id));
        }
        Ok(())
    }

    /// Link our process entities together with associations
    ///
    /// # Arguments
    ///
    /// * `context` - The context to use when bulding this process tree
    /// * `groups` - The groups to add associations too
    /// * `thorium` - A Thorium client
    async fn link_entities(
        &self,
        context: &mut WindowsProcTreeContext,
        groups: &[String],
        thorium: &crate::Thorium,
    ) -> Result<(), crate::Error> {
        // build a list of associations to create
        let mut reqs = Vec::with_capacity(context.pid_map.len());
        // iterate over all of our processes and link them using associations
        for (parent_pid, name, entity_id) in context.pid_map.values() {
            // build the target for this association
            let target = AssociationTarget::Entity {
                id: *entity_id,
                name: name.clone(),
            };
            //  get our parents entity id if it exists
            if let Some(parent_pid) = parent_pid {
                // try to get our parent pids entity id
                if let Some((_, parent_name, parent_id)) = context.pid_map.get(parent_pid) {
                    // we have a parent entity so build the association request for that
                    let source = AssociationTarget::Entity {
                        id: *parent_id,
                        name: parent_name.clone(),
                    };
                    // build a base association request
                    let assoc_req = AssociationRequest::new(AssociationKind::ChildProcess, source)
                        .target(target);
                    // add this request to our set of requests to submit
                    reqs.push(assoc_req);
                    // continue to the next
                    continue;
                }
            }
            // we don't have a parent process or a parent entity so just link this directly to our process tree
            // we have a parent entity so build the association request for that
            let source = AssociationTarget::Entity {
                id: context.id,
                name: self.name.clone(),
            };
            // build a base association request
            let assoc_req = AssociationRequest::new(AssociationKind::ChildProcess, source)
                .target(target)
                .groups(groups);
            // add this request to our set of requests to submit
            reqs.push(assoc_req);
        }
        // submit all of our association links in bulk 10 at a time
        stream::iter(&reqs)
            .map(|req| thorium.associations.create(req))
            .buffer_unordered(10)
            .collect::<Vec<Result<_, crate::Error>>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;
        Ok(())
    }

    /// Create all of our processes in Thorium
    ///
    /// # Arguments
    ///
    /// * `groups` - The groups to create this process tree for
    /// * `thorium` - A Thorium client
    pub async fn create_all(
        mut self,
        groups: &[String],
        thorium: &crate::Thorium,
    ) -> Result<uuid::Uuid, crate::Error> {
        // create our root process tree
        let mut context = self.create_root(groups, thorium).await?;
        // create our entities
        self.create_entities(&mut context, groups, thorium).await?;
        // link our entities
        self.link_entities(&mut context, groups, thorium).await?;
        Ok(context.id)
    }

    /// Convert all of this process tree to a a list of EntityRequests
    pub fn to_requests(self) -> Vec<EntityRequest> {
        // correctly size our list of entity requests (one extra with our root entity)
        let mut reqs = Vec::with_capacity(self.processes.len() + 1);
        // don't specify groups in our stored entity requests
        let groups = Vec::<String>::default();
        // build the entity request for our root with the tool instances that dumped it
        reqs.push(EntityRequest::new(&self.name, self.root_metadata(), groups));
        // add all of the processes we find to our list
        for (pid, process) in self.processes {
            // convert this process to a request
            reqs.push(create_windows_process_req_empty_groups(pid, process));
        }
        reqs
    }
}

/// Tests for reading, writing, and sending process tree entities
#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::WindowsProcessTreeEntity;

    /// A stored process tree without tools reads with no tool references at all
    #[test]
    fn compat_reads_missing_and_empty_tools() {
        // read a row without a tools key and a row with empty tools
        for raw in ["{}", r#"{"tools":[]}"#] {
            // read this row
            let tree: WindowsProcessTreeEntity = serde_json::from_str(raw).unwrap();
            // neither list has anything in it
            assert!(tree.tools.is_empty(), "{raw}");
            assert!(tree.legacy_tools.is_empty(), "{raw}");
        }
    }

    /// A process tree stored with tool instances and legacy names reads back unchanged
    #[test]
    fn compat_reads_new_shape_unchanged() {
        // build a row in the current shape
        let id = Uuid::new_v4();
        let raw = format!(r#"{{"tools":["{id}"],"legacy_tools":["rekall"]}}"#);
        // read the row
        let tree: WindowsProcessTreeEntity = serde_json::from_str(&raw).unwrap();
        // both lists are what was stored
        assert_eq!(tree.tools, vec![id]);
        assert_eq!(tree.legacy_tools, vec!["rekall".to_owned()]);
    }

    /// A process tree serializes and deserializes unchanged, and an empty `legacy_tools` is left
    /// out
    #[test]
    fn new_shape_round_trips() {
        // build a tree with legacy names and one without
        let with_legacy = WindowsProcessTreeEntity {
            tools: vec![Uuid::new_v4()],
            legacy_tools: vec!["rekall".to_owned()],
        };
        let without_legacy = WindowsProcessTreeEntity {
            legacy_tools: Vec::new(),
            ..with_legacy.clone()
        };
        // round trip each of them through JSON
        for tree in [with_legacy, without_legacy] {
            // serialize this tree
            let value = serde_json::to_value(&tree).unwrap();
            // an empty legacy list isn't written
            assert_eq!(
                value.get("legacy_tools").is_some(),
                !tree.legacy_tools.is_empty()
            );
            // the tool instances are written as uuid strings
            assert_eq!(
                value["tools"],
                serde_json::json!([tree.tools[0].to_string()])
            );
            // reading it back gives the same tree
            let read: WindowsProcessTreeEntity = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(&read).unwrap(), value);
        }
    }

    /// A process tree request sends its tool instances as `tool_ids` and never sends tool names
    #[test]
    fn form_fields_send_tool_ids() {
        // build a tree with a tool instance and a legacy name
        let id = Uuid::new_v4();
        let tree = WindowsProcessTreeEntity {
            tools: vec![id],
            legacy_tools: vec!["rekall".to_owned()],
        };
        // the kind and tool instance are sent and nothing else
        assert_eq!(
            tree.form_fields(),
            vec![
                ("kind", "WindowsProcessTree".to_owned()),
                ("metadata[tool_ids][]", id.to_string()),
            ]
        );
    }

    /// The process tree builder records tool instances on the root's create request, which
    /// sends them as tool instance ids
    #[cfg(feature = "client")]
    #[test]
    fn builder_records_tool_instances() {
        use crate::models::EntityMetadataRequest;
        // build a process tree found by two tool instances
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let builder = WindowsProcessTreeEntity::builder("tree")
            .tool(first)
            .tool(second);
        // the builder keeps both tool instances in order
        assert_eq!(builder.tools, vec![first, second]);
        // the root's create request carries both tool instances
        let EntityMetadataRequest::WindowsProcessTree(root) = builder.root_metadata() else {
            panic!("not a process tree request");
        };
        assert_eq!(root.tools, vec![first, second]);
        assert_eq!(root.legacy_tools, Vec::<String>::new());
        // the stored requests carry them on the root too
        let reqs = builder.to_requests();
        let EntityMetadataRequest::WindowsProcessTree(stored) = &reqs[0].metadata else {
            panic!("the first stored request is not the root");
        };
        assert_eq!(stored.tools, vec![first, second]);
        // a tree without tool instances has a root without any
        let EntityMetadataRequest::WindowsProcessTree(empty) =
            WindowsProcessTreeEntity::builder("tree").root_metadata()
        else {
            panic!("not a process tree request");
        };
        assert_eq!(empty.tools, Vec::<Uuid>::new());
    }
}
