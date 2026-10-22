//! Handles instance commands for tool, toolset, and workflow instances

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Map, Value};
use thorium::models::{
    Entity, EntityKinds, EntityListOpts, EntityMetadata, ToolInstanceEntity, ToolSetInstanceEntity,
    WorkflowInstanceEntity,
};
use thorium::{Error, Thorium};
use uuid::Uuid;

use crate::args::Args;
use crate::args::instances::{
    DescribeInstance, DiffInstances, GetInstances, InstanceRoot, Instances,
};
use crate::handlers::agents::find;
use crate::utils;
use crate::utils::output::{self, OutputFormat, View};
use crate::utils::render;
use crate::utils::watch;

/// The kinds of entity that are instances
const INSTANCE_KINDS: [EntityKinds; 3] = [
    EntityKinds::ToolInstance,
    EntityKinds::ToolSetInstance,
    EntityKinds::WorkflowInstance,
];

/// Find the ids of the entities of one kind with a name
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `kind` - The kind of entity
/// * `name` - The name to find
async fn named(thorium: &Thorium, kind: EntityKinds, name: &str) -> Result<Vec<Uuid>, Error> {
    // list the entities of this kind and keep the ones with this exact name
    let opts = EntityListOpts::default().kind(kind).limit(10_000);
    let mut cursor = thorium.entities.list(&opts).await?;
    let mut matches = Vec::new();
    loop {
        matches.extend(
            cursor
                .data
                .drain(..)
                .filter(|line| line.name == name)
                .map(|line| line.id),
        );
        // stop once every page was read
        if cursor.exhausted() {
            break;
        }
        cursor.refill().await?;
    }
    Ok(matches)
}

/// Find the toolset and workflow instances named `{root}#r{revision}`
///
/// The roots are found by name and their instances by the tag that names their root, so no
/// other instance is read.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `root` - The name of the toolset or workflow
/// * `value` - The instance's full name
async fn revision_named(thorium: &Thorium, root: &str, value: &str) -> Result<Vec<Uuid>, Error> {
    // look in the workflows and toolsets with the root's name
    let mut matches = Vec::new();
    for (kind, tag, instance_kind) in [
        (
            EntityKinds::WorkflowCatalog,
            "WorkflowId",
            EntityKinds::WorkflowInstance,
        ),
        (
            EntityKinds::ToolSet,
            "ToolSetId",
            EntityKinds::ToolSetInstance,
        ),
    ] {
        for id in named(thorium, kind, root).await? {
            // list the root's instances and keep the one with this name
            let opts = EntityListOpts::default()
                .kind(instance_kind)
                .tag(tag, id.to_string())
                .limit(10_000);
            let mut cursor = thorium.entities.list(&opts).await?;
            loop {
                matches.extend(
                    cursor
                        .data
                        .drain(..)
                        .filter(|line| line.name == value)
                        .map(|line| line.id),
                );
                // stop once every page was read
                if cursor.exhausted() {
                    break;
                }
                cursor.refill().await?;
            }
        }
    }
    Ok(matches)
}

/// Find the tool instances named `{tool}@{version}`
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `tool` - The name of the tool
/// * `version` - The instance's version
async fn version_named(thorium: &Thorium, tool: &str, version: &str) -> Result<Vec<Uuid>, Error> {
    // each tool with this name lists its instances' versions
    let mut matches = Vec::new();
    for id in named(thorium, EntityKinds::Tool, tool).await? {
        if let EntityMetadata::Tool(meta) = thorium.entities.get(id).await?.metadata {
            matches.extend(
                meta.instances
                    .iter()
                    .filter(|summary| summary.version.to_string() == version)
                    .map(|summary| summary.id),
            );
        }
    }
    Ok(matches)
}

/// Find a tool, toolset, or workflow instance by its ID or its name
///
/// A name of the form `{root}#r{revision}` or `{tool}@{version}` is found through its root;
/// any other name is looked for among every instance.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `value` - The instance's ID or name
pub(crate) async fn find_instance(thorium: &Thorium, value: &str) -> Result<Entity, Error> {
    // an ID is read directly, and must be an instance
    if let Ok(id) = Uuid::parse_str(value) {
        let found = thorium.entities.get(id).await?;
        if !INSTANCE_KINDS.contains(&found.kind) {
            return Err(Error::new(format!(
                "{value} is a {}, not a tool, toolset, or workflow instance",
                found.kind
            )));
        }
        return Ok(found);
    }
    // otherwise find the instances with this name through their root when it names one
    let mut matches = match (value.rsplit_once("#r"), value.rsplit_once('@')) {
        (Some((root, revision)), _) if revision.parse::<u64>().is_ok() => {
            revision_named(thorium, root, value).await?
        }
        (_, Some((tool, version))) if !tool.is_empty() && !version.is_empty() => {
            version_named(thorium, tool, version).await?
        }
        _ => {
            let mut found = Vec::new();
            for kind in INSTANCE_KINDS {
                found.extend(named(thorium, kind, value).await?);
            }
            found
        }
    };
    // the name must name exactly one instance
    matches.sort();
    matches.dedup();
    match matches.as_slice() {
        [id] => thorium.entities.get(*id).await,
        [] => Err(Error::new(format!("No instance is named {value}"))),
        ids => Err(Error::new(format!(
            "{} instances are named {value}; use one of their IDs: {}",
            ids.len(),
            ids.iter()
                .map(ToString::to_string)
                .collect::<Vec<String>>()
                .join(", ")
        ))),
    }
}

/// One instance in a listing of a root's instances
#[derive(Serialize)]
struct InstanceRow {
    /// The instance's ID
    id: Uuid,
    /// The instance's name
    name: String,
    /// The instance's revision, for toolset and workflow instances
    #[serde(skip_serializing_if = "Option::is_none")]
    revision: Option<u64>,
    /// The instance's version label, for tool instances
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    /// Whether the instance is ready, invalid, or deprecated
    status: String,
    /// The instance's digest, or a tool instance's spec hash
    digest: String,
    /// When the instance was created
    created: DateTime<Utc>,
}

/// A root and its instances
#[derive(Serialize)]
struct InstancesView {
    /// The kind of root
    kind: EntityKinds,
    /// The root's ID
    root: Uuid,
    /// The root's name
    root_name: String,
    /// The newest revision of a toolset's or workflow's instances
    #[serde(skip_serializing_if = "Option::is_none")]
    instance_revision: Option<u64>,
    /// The root's latest instance
    latest_instance: Option<Uuid>,
    /// Every instance, newest first
    instances: Vec<InstanceRow>,
}

impl View for InstancesView {
    /// Render the instances as a table
    fn table(&self) -> String {
        // one row per instance
        let rows = self
            .instances
            .iter()
            .map(|row| {
                let revision = row
                    .revision
                    .map(|rev| format!("r{rev}"))
                    .or_else(|| row.version.clone())
                    .unwrap_or_default();
                let latest = if self.latest_instance == Some(row.id) {
                    "*"
                } else {
                    ""
                };
                vec![
                    row.name.clone(),
                    row.id.to_string(),
                    revision,
                    row.status.clone(),
                    latest.to_owned(),
                    row.digest.clone(),
                    row.created.to_rfc3339(),
                ]
            })
            .collect();
        output::table_from(
            &[
                "NAME", "ID", "REVISION", "STATUS", "LATEST", "DIGEST", "CREATED",
            ],
            rows,
        )
    }

    /// Render the instances as text
    ///
    /// # Arguments
    ///
    /// * `ansi` - Whether to style the text
    fn human(&self, ansi: bool) -> String {
        // name the root, then list its instances
        let mut lines = vec![render::header(&self.root_name, self.kind.as_str(), ansi)];
        lines.push(render::field("ID", &self.root.to_string(), ansi));
        lines.push(render::field(
            "Latest instance",
            &self
                .latest_instance
                .map_or_else(|| "-".to_owned(), |id| id.to_string()),
            ansi,
        ));
        lines.push(self.table());
        lines.join("\n")
    }

    /// The listing as one item, so a condition checks the root's newest revision
    fn wait_items(&self) -> Result<Vec<Value>, Error> {
        // a wait condition checks the whole listing
        Ok(vec![output::to_value(self)?])
    }
}

/// Describe a revisioned instance of a root for a listing
///
/// An instance that names another root is no row of this one, whatever it's tagged with.
///
/// # Arguments
///
/// * `root` - The toolset or workflow being listed
/// * `entity` - The instance
fn row_of(root: Uuid, entity: &Entity) -> Option<InstanceRow> {
    // only toolset and workflow instances of this root have rows
    let (revision, status, digest) = match &entity.metadata {
        EntityMetadata::ToolSetInstance(instance) if instance.tool_set == root => (
            instance.revision,
            instance.status.as_str(),
            instance.digest.clone(),
        ),
        EntityMetadata::WorkflowInstance(instance) if instance.workflow == root => (
            instance.revision,
            instance.status.as_str(),
            instance.digest.clone(),
        ),
        _ => return None,
    };
    Some(InstanceRow {
        id: entity.id,
        name: entity.name.clone(),
        revision: Some(revision),
        version: None,
        status: status.to_owned(),
        digest,
        created: entity.created,
    })
}

/// Read a root's instances as a listing
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The get command to execute
async fn read_instances(thorium: &Thorium, cmd: &GetInstances) -> Result<InstancesView, Error> {
    // find the root and the tag its instances carry
    let (kind, label, tag, instance_kind) = match cmd.kind {
        InstanceRoot::Tool => (EntityKinds::Tool, "a tool", "", EntityKinds::ToolInstance),
        InstanceRoot::Toolset => (
            EntityKinds::ToolSet,
            "a toolset",
            "ToolSetId",
            EntityKinds::ToolSetInstance,
        ),
        InstanceRoot::Workflow => (
            EntityKinds::WorkflowCatalog,
            "a workflow",
            "WorkflowId",
            EntityKinds::WorkflowInstance,
        ),
    };
    let root = find(thorium, kind, label, &cmd.root).await?;
    // a tool lists its versions, and toolsets and workflows list their tagged instances
    let (latest_instance, mut instances) = match &root.metadata {
        EntityMetadata::Tool(tool) => {
            let rows = tool
                .instances
                .iter()
                .map(|summary| InstanceRow {
                    id: summary.id,
                    name: format!("{}@{}", root.name, summary.version),
                    revision: None,
                    version: Some(summary.version.to_string()),
                    status: if summary.deprecated {
                        "Deprecated".to_owned()
                    } else {
                        "Ready".to_owned()
                    },
                    digest: summary.spec_hash.clone(),
                    created: summary.created,
                })
                .collect::<Vec<InstanceRow>>();
            (tool.latest, rows)
        }
        metadata => {
            let latest = match metadata {
                EntityMetadata::ToolSet(meta) => meta.latest_instance,
                EntityMetadata::WorkflowCatalog(meta) => meta.latest_instance,
                _ => None,
            };
            let opts = EntityListOpts::default()
                .groups(root.groups.clone())
                .tag(tag, root.id.to_string())
                .kind(instance_kind)
                .limit(10_000);
            let mut cursor = thorium.entities.list_details(&opts).await?;
            let mut found = std::mem::take(&mut cursor.data);
            while !cursor.exhausted() {
                cursor.refill().await?;
                found.append(&mut cursor.data);
            }
            let mut rows = found
                .iter()
                .filter_map(|entity| row_of(root.id, entity))
                .collect::<Vec<InstanceRow>>();
            rows.sort_by_key(|row| std::cmp::Reverse(row.revision));
            rows.dedup_by_key(|row| row.id);
            (latest, rows)
        }
    };
    // list the newest instance first
    if cmd.kind == InstanceRoot::Tool {
        instances.sort_by_key(|row| std::cmp::Reverse(row.created));
    }
    let instance_revision = instances.iter().filter_map(|row| row.revision).max();
    Ok(InstancesView {
        kind,
        root: root.id,
        root_name: root.name.clone(),
        instance_revision,
        latest_instance,
        instances,
    })
}

/// List a root's instances
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The get command to execute
async fn get(thorium: &Thorium, cmd: &GetInstances) -> Result<(), Error> {
    // list the instances in a table by default
    let format = cmd.format.format.unwrap_or(OutputFormat::Table);
    watch::show(format, &cmd.watch, Some(&cmd.wait), || {
        read_instances(thorium, cmd)
    })
    .await
}

/// Render a workflow instance's details as text
///
/// # Arguments
///
/// * `lines` - The lines to add to
/// * `instance` - The workflow instance
/// * `ansi` - Whether to style the text
fn workflow_text(lines: &mut Vec<String>, instance: &WorkflowInstanceEntity, ansi: bool) {
    // the workflow, revision, status, and digest
    lines.push(render::field(
        "Workflow",
        &instance.workflow.to_string(),
        ansi,
    ));
    lines.push(render::field(
        "Revision",
        &instance.revision.to_string(),
        ansi,
    ));
    status_lines(
        lines,
        instance.status.as_str(),
        match &instance.status {
            thorium::models::WorkflowInstanceStatus::Invalid { errors } => errors,
            thorium::models::WorkflowInstanceStatus::Ready => &[],
        },
        ansi,
    );
    lines.push(render::field("Digest", &instance.digest, ansi));
    lines.push(render::field("Triggered by", &instance.triggered_by, ansi));
    lines.push(render::field(
        "Trigger",
        &format!("{:?}", instance.trigger),
        ansi,
    ));
    lines.push(render::field(
        "Entry agent",
        &instance
            .entry_agent
            .map_or_else(|| "-".to_owned(), |id| id.to_string()),
        ansi,
    ));
    for warning in &instance.warnings {
        lines.push(render::field("Warning", warning, ansi));
    }
    // a table of agents and a table of tools
    let agents = instance
        .agents
        .iter()
        .map(|agent| {
            vec![
                agent.name.clone(),
                agent.agent.to_string(),
                agent.config_hash.clone(),
                agent.tools.len().to_string(),
                agent.sub_agents.len().to_string(),
            ]
        })
        .collect();
    lines.push(render::label("Agents", ansi));
    lines.push(output::table_from(
        &["NAME", "ID", "CONFIG HASH", "TOOLS", "SUB-AGENTS"],
        agents,
    ));
    let tools = instance
        .tools
        .iter()
        .map(|tool| {
            vec![
                tool.name.clone(),
                tool.version.clone(),
                tool.kind.to_string(),
                tool.function_name.clone().unwrap_or_else(|| "-".to_owned()),
                tool.pinned.to_string(),
                tool.dependency.to_string(),
                tool.spec_hash.clone(),
            ]
        })
        .collect();
    lines.push(render::label("Tools", ansi));
    lines.push(output::table_from(
        &[
            "NAME",
            "VERSION",
            "KIND",
            "FUNCTION",
            "PINNED",
            "DEPENDENCY",
            "SPEC HASH",
        ],
        tools,
    ));
}

/// Render a toolset instance's details as text
///
/// # Arguments
///
/// * `lines` - The lines to add to
/// * `instance` - The toolset instance
/// * `ansi` - Whether to style the text
fn toolset_text(lines: &mut Vec<String>, instance: &ToolSetInstanceEntity, ansi: bool) {
    // the toolset, revision, status, digest, and settings
    lines.push(render::field(
        "Toolset",
        &instance.tool_set.to_string(),
        ansi,
    ));
    lines.push(render::field(
        "Revision",
        &instance.revision.to_string(),
        ansi,
    ));
    status_lines(
        lines,
        instance.status.as_str(),
        match &instance.status {
            thorium::models::ToolSetInstanceStatus::Invalid { errors } => errors,
            thorium::models::ToolSetInstanceStatus::Ready => &[],
        },
        ansi,
    );
    lines.push(render::field("Digest", &instance.digest, ansi));
    lines.push(render::field(
        "Read only",
        &instance.read_only.to_string(),
        ansi,
    ));
    lines.push(render::field(
        "Selector",
        &instance
            .selector
            .as_ref()
            .map_or_else(|| "-".to_owned(), ToString::to_string),
        ansi,
    ));
    // a table of members and a table of exclusions with their reasons
    let members = instance
        .members
        .iter()
        .map(|member| {
            vec![
                member.name.clone(),
                member.version.clone(),
                member.function_name.clone(),
                member.inclusion.as_str().to_owned(),
                member.spec_hash.clone(),
            ]
        })
        .collect();
    lines.push(render::label("Members", ansi));
    lines.push(output::table_from(
        &["NAME", "VERSION", "FUNCTION", "INCLUSION", "SPEC HASH"],
        members,
    ));
    let excluded = instance
        .excluded
        .iter()
        .map(|exclusion| {
            vec![
                exclusion.name.clone(),
                exclusion.inclusion.as_str().to_owned(),
                exclusion.describe(),
            ]
        })
        .collect();
    lines.push(render::label("Excluded", ansi));
    lines.push(output::table_from(
        &["NAME", "INCLUSION", "REASON"],
        excluded,
    ));
}

/// Render a tool instance's details as text
///
/// # Arguments
///
/// * `lines` - The lines to add to
/// * `instance` - The tool instance
/// * `ansi` - Whether to style the text
fn tool_text(lines: &mut Vec<String>, instance: &ToolInstanceEntity, ansi: bool) {
    // the tool, version, kind, hash, and deprecation
    lines.push(render::field("Tool", &instance.tool.to_string(), ansi));
    lines.push(render::field(
        "Version",
        &instance.version.to_string(),
        ansi,
    ));
    lines.push(render::field("Kind", instance.spec.kind().as_ref(), ansi));
    lines.push(render::field("Spec hash", &instance.spec_hash, ansi));
    lines.push(render::field(
        "Function",
        instance.spec.function_name().unwrap_or("-"),
        ansi,
    ));
    lines.push(render::field(
        "Deprecated",
        &instance.deprecated.to_string(),
        ansi,
    ));
}

/// Add an instance's status and the errors that made it invalid
///
/// # Arguments
///
/// * `lines` - The lines to add to
/// * `status` - The name of the status
/// * `errors` - The errors that made the instance invalid
/// * `ansi` - Whether to style the text
fn status_lines(lines: &mut Vec<String>, status: &str, errors: &[String], ansi: bool) {
    // the status, then each error
    lines.push(render::field("Status", status, ansi));
    for error in errors {
        lines.push(render::field("Error", error, ansi));
    }
}

/// One instance described in full
#[derive(Serialize)]
#[serde(transparent)]
struct InstanceView(
    /// The instance as the API returned it
    Entity,
);

impl View for InstanceView {
    /// Render the instance as a one row table
    fn table(&self) -> String {
        // describe the instance in one row, as a row of its own root
        let root = match &self.0.metadata {
            EntityMetadata::ToolSetInstance(instance) => instance.tool_set,
            EntityMetadata::WorkflowInstance(instance) => instance.workflow,
            _ => Uuid::nil(),
        };
        let row = row_of(root, &self.0).map_or_else(
            || vec![self.0.name.clone(), self.0.id.to_string(), "-".to_owned()],
            |row| vec![row.name, row.id.to_string(), row.status],
        );
        output::table_from(&["NAME", "ID", "STATUS"], vec![row])
    }

    /// Render the instance as text
    ///
    /// # Arguments
    ///
    /// * `ansi` - Whether to style the text
    fn human(&self, ansi: bool) -> String {
        // start with the instance's name, kind, and groups
        let entity = &self.0;
        let mut lines = vec![render::header(
            &entity.name,
            &entity.groups.join(", "),
            ansi,
        )];
        lines.push(render::field("ID", &entity.id.to_string(), ansi));
        lines.push(render::field("Kind", entity.kind.as_str(), ansi));
        lines.push(render::field("Created", &entity.created.to_rfc3339(), ansi));
        // add what this kind of instance records
        match &entity.metadata {
            EntityMetadata::WorkflowInstance(instance) => {
                workflow_text(&mut lines, instance, ansi);
            }
            EntityMetadata::ToolSetInstance(instance) => {
                toolset_text(&mut lines, instance, ansi);
            }
            EntityMetadata::ToolInstance(instance) => tool_text(&mut lines, instance, ansi),
            _ => (),
        }
        lines.join("\n")
    }
}

/// Describe an instance
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The describe command to execute
async fn describe(thorium: &Thorium, cmd: &DescribeInstance) -> Result<(), Error> {
    // describe the instance in text by default
    let format = cmd.format.format.unwrap_or(OutputFormat::Human);
    watch::show(format, &cmd.watch, Some(&cmd.wait), || async {
        Ok(InstanceView(find_instance(thorium, &cmd.instance).await?))
    })
    .await
}

/// One field that differs between two instances
#[derive(Serialize)]
struct DiffRow {
    /// The field's path, with nested fields joined by `.`
    field: String,
    /// The field's value in the first instance, or null when it's missing
    first: Value,
    /// The field's value in the second instance, or null when it's missing
    second: Value,
}

/// The fields that differ between two instances
#[derive(Serialize)]
#[serde(transparent)]
struct DiffView(
    /// Each differing field
    Vec<DiffRow>,
);

impl View for DiffView {
    /// Render the differences as a table
    fn table(&self) -> String {
        // one row per differing field
        let rows = self
            .0
            .iter()
            .map(|row| {
                vec![
                    row.field.clone(),
                    row.first.to_string(),
                    row.second.to_string(),
                ]
            })
            .collect();
        output::table_from(&["FIELD", "FIRST", "SECOND"], rows)
    }

    /// Render the differences as text
    ///
    /// # Arguments
    ///
    /// * `ansi` - Whether to style the text
    fn human(&self, ansi: bool) -> String {
        // one line per differing field
        if self.0.is_empty() {
            return "The instances don't differ".to_owned();
        }
        self.0
            .iter()
            .map(|row| {
                render::field(
                    &row.field,
                    &format!("{} -> {}", row.first, row.second),
                    ansi,
                )
            })
            .collect::<Vec<String>>()
            .join("\n")
    }
}

/// Flatten a JSON value into its leaf fields by dotted path, keeping lists whole
///
/// # Arguments
///
/// * `prefix` - The path of this value
/// * `value` - The value to flatten
/// * `fields` - The fields to add to
fn flatten(prefix: &str, value: &Value, fields: &mut Map<String, Value>) {
    match value {
        // an object's fields are flattened under its path
        Value::Object(map) => {
            for (key, inner) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(&path, inner, fields);
            }
        }
        // every other value is a leaf
        leaf => {
            fields.insert(prefix.to_owned(), leaf.clone());
        }
    }
}

/// Get an instance's comparable fields: its name and its kind's metadata
///
/// # Arguments
///
/// * `entity` - The instance
fn comparable(entity: &Entity) -> Result<Map<String, Value>, Error> {
    // the metadata is wrapped in its kind's name, which both sides share
    let metadata = output::to_value(&entity.metadata)?;
    let inner = match metadata {
        Value::Object(map) => map.into_iter().next().map(|(_, inner)| inner),
        _ => None,
    }
    .unwrap_or(Value::Null);
    let mut fields = Map::new();
    fields.insert("name".to_owned(), Value::String(entity.name.clone()));
    flatten("", &inner, &mut fields);
    Ok(fields)
}

/// Show the fields that differ between two instances
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The diff command to execute
async fn diff(thorium: &Thorium, cmd: &DiffInstances) -> Result<(), Error> {
    // read both instances, which must be the same kind
    let first = find_instance(thorium, &cmd.first).await?;
    let second = find_instance(thorium, &cmd.second).await?;
    if first.kind != second.kind {
        return Err(Error::new(format!(
            "{} is a {} but {} is a {}; only instances of one kind can be compared",
            first.name, first.kind, second.name, second.kind
        )));
    }
    // compare every field of either side
    let (left, right) = (comparable(&first)?, comparable(&second)?);
    let mut paths = left
        .keys()
        .chain(right.keys())
        .cloned()
        .collect::<Vec<String>>();
    paths.sort();
    paths.dedup();
    let rows = paths
        .into_iter()
        .filter_map(|path| {
            let (a, b) = (
                left.get(&path).cloned().unwrap_or(Value::Null),
                right.get(&path).cloned().unwrap_or(Value::Null),
            );
            (a != b).then_some(DiffRow {
                field: path,
                first: a,
                second: b,
            })
        })
        .collect();
    output::print(
        &DiffView(rows),
        cmd.format.format.unwrap_or(OutputFormat::Table),
    )
}

/// Handle all instance commands
///
/// # Arguments
///
/// * `args` - The arguments passed to Thorctl
/// * `cmd` - The instances command to execute
pub async fn handle(args: &Args, cmd: &Instances) -> Result<(), Error> {
    // load our config and instance our client
    let (_, thorium) = utils::get_client(args).await?;
    // call the right instances handler
    let result = match cmd {
        Instances::Get(cmd) => get(&thorium, cmd).await,
        Instances::Describe(cmd) => describe(&thorium, cmd).await,
        Instances::Diff(cmd) => diff(&thorium, cmd).await,
    };
    // show the API's status code with any refusal
    result.map_err(|err| match err.status() {
        Some(_) => Error::new(err.to_string()),
        None => err,
    })
}

#[cfg(test)]
mod tests {
    use super::row_of;
    use thorium::models::{
        Entity, EntityKinds, EntityMetadata, ToolSetInstanceEntity, WorkflowInstanceEntity,
    };
    use uuid::Uuid;

    /// Build an instance entity of a kind from its metadata
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of instance
    /// * `metadata` - The instance's metadata
    fn instance(kind: EntityKinds, metadata: EntityMetadata) -> Entity {
        // an instance with only its kind and metadata set
        Entity {
            id: Uuid::new_v4(),
            name: "kit#r1".to_owned(),
            kind,
            metadata,
            description: None,
            submitter: "io".to_owned(),
            groups: vec!["minerva".to_owned()],
            tags: std::collections::HashMap::default(),
            image: None,
            created: chrono::Utc::now(),
        }
    }

    /// A listing of a root keeps only the instances that name that root, whatever their tags
    #[test]
    fn rows_only_of_their_root() {
        // a workflow instance and a toolset instance of one root each
        let (root, other) = (Uuid::new_v4(), Uuid::new_v4());
        let workflow = |id| {
            let meta: WorkflowInstanceEntity = serde_json::from_value(serde_json::json!({
                "workflow": id, "workflow_groups": [], "revision": 1, "previous": null,
                "status": "Ready", "trigger": "Manual", "entry_agent": null, "agents": [],
                "tools": [], "digest": "v1:0",
            }))
            .unwrap();
            instance(
                EntityKinds::WorkflowInstance,
                EntityMetadata::WorkflowInstance(meta),
            )
        };
        let tool_set = |id| {
            let meta: ToolSetInstanceEntity = serde_json::from_value(serde_json::json!({
                "tool_set": id, "tool_set_groups": [], "revision": 1, "previous": null,
                "status": "Ready", "trigger": "Manual", "read_only": false, "selector": null,
                "members": [], "excluded": [], "digest": "v1:0",
            }))
            .unwrap();
            instance(
                EntityKinds::ToolSetInstance,
                EntityMetadata::ToolSetInstance(meta),
            )
        };
        // each is a row of its own root only
        assert!(row_of(root, &workflow(root)).is_some());
        assert!(row_of(root, &workflow(other)).is_none());
        assert!(row_of(root, &tool_set(root)).is_some());
        assert!(row_of(root, &tool_set(other)).is_none());
    }
}
