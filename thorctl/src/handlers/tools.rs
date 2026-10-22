//! Handles tool commands

use serde::Serialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use thorium::client::tool_runs::{self, NoHooks, ToolRunKind, ToolRunOptions, ToolRunReport};
use thorium::models::{
    DEFAULT_TOOL_RESULTS_DIR, Entity, EntityKinds, EntityListOpts, EntityMetadata,
    EntityMetadataRequest, EntityMetadataUpdate, EntityRequest, EntityUpdate, GenericJobArgs,
    HarnessEnv, HarnessEnvVar, ToolEntityRequest, ToolInstanceSummary, ToolPortabilityReport,
    ToolRunRequest, ToolSpec,
};
use thorium::{Error, Thorium};
use uuid::Uuid;

use crate::args::Args;
use crate::args::tools::{
    CreateTool, DeprecateTool, DescribeTool, GetTools, HarnessEnvCmd, HarnessModels, PublishTool,
    RunTool, ToolRunResults, ToolRunStatus, ToolRuns, ToolVersions, Tools,
};
use crate::handlers::instances::find_instance;
use crate::handlers::runs::run_inputs;
use crate::utils;
use crate::utils::output::{self, OutputFormat, View};
use crate::utils::render;
use crate::utils::watch;

/// Get the version label of a tool's latest instance, if it has one
///
/// # Arguments
///
/// * `tool` - The tool to read
fn latest_version(tool: &Entity) -> String {
    // only tools have a latest version
    match &tool.metadata {
        EntityMetadata::Tool(meta) => meta
            .latest_version
            .as_ref()
            .map_or_else(|| "-".to_owned(), ToString::to_string),
        _ => "-".to_owned(),
    }
}

/// Get the kind of a tool as text
///
/// # Arguments
///
/// * `tool` - The tool to read
fn tool_kind(tool: &Entity) -> String {
    // only tools have a tool kind
    match &tool.metadata {
        EntityMetadata::Tool(meta) => meta.kind.to_string(),
        _ => tool.kind.to_string(),
    }
}

/// The tools one command lists
#[derive(Serialize)]
#[serde(transparent)]
struct ToolsView(
    /// The tools as the API returned them
    Vec<Entity>,
);

impl View for ToolsView {
    /// Render the tools as a table
    fn table(&self) -> String {
        // build one row per tool
        let rows = self
            .0
            .iter()
            .map(|tool| {
                vec![
                    tool.name.clone(),
                    tool.id.to_string(),
                    tool_kind(tool),
                    latest_version(tool),
                    tool.groups.join(", "),
                ]
            })
            .collect();
        output::table_from(&["NAME", "ID", "KIND", "LATEST", "GROUPS"], rows)
    }

    /// Render the tools as text
    ///
    /// # Arguments
    ///
    /// * `ansi` - Whether to style the text
    fn human(&self, ansi: bool) -> String {
        // describe each tool, separated by rules
        self.0
            .iter()
            .map(|tool| describe_text(tool, None, ansi))
            .collect::<Vec<String>>()
            .join(&format!("\n{}\n", render::separator(ansi)))
    }
}

/// Render a portability report as lines of text
///
/// # Arguments
///
/// * `report` - The report to render
/// * `ansi` - Whether to style the text
fn portability_lines(report: &ToolPortabilityReport, ansi: bool) -> Vec<String> {
    // say yes or no for a compatibility flag
    let answer = |compatible: bool| if compatible { "yes" } else { "no" };
    // list strict compatibility and its issues, then Gemini compatibility and its rewrites
    let mut lines = vec![render::field(
        "Strict compatible",
        answer(report.strict_compatible),
        ansi,
    )];
    lines.extend(
        report
            .strict_issues
            .iter()
            .map(|issue| format!("  - {issue}")),
    );
    lines.push(render::field(
        "Gemini compatible",
        answer(report.gemini_compatible),
        ansi,
    ));
    lines.extend(
        report
            .gemini_rewrites
            .iter()
            .map(|rewrite| format!("  - {rewrite}")),
    );
    lines
}

/// Print a portability report's warnings, if it has any
///
/// # Arguments
///
/// * `report` - The report to print, if there is one
fn print_portability_warnings(report: Option<&ToolPortabilityReport>) {
    // a portable spec has nothing to warn about
    let Some(report) =
        report.filter(|report| !report.strict_compatible || !report.gemini_compatible)
    else {
        return;
    };
    // print the warnings under a heading
    println!("Portability warnings (the tool was published; some model APIs change its schema):");
    for line in portability_lines(report, false) {
        println!("  {line}");
    }
}

/// Render the model-API details of an agentic tool instance as lines of text
///
/// # Arguments
///
/// * `instance` - The tool instance to describe
/// * `ansi` - Whether to style the text
fn instance_lines(instance: &Entity, ansi: bool) -> Vec<String> {
    // only agentic instances have model-API details
    let EntityMetadata::ToolInstance(meta) = &instance.metadata else {
        return Vec::new();
    };
    let ToolSpec::Agentic(agentic) = &meta.spec else {
        return Vec::new();
    };
    // list the fields a model API reads
    let mut lines = vec![
        render::field("Function", &agentic.function_name, ansi),
        render::field("Strict", &format!("{:?}", agentic.strict), ansi),
        render::field("Loading", &format!("{:?}", agentic.loading), ansi),
        render::field(
            "Open world",
            &agentic.capabilities.open_world.to_string(),
            ansi,
        ),
        render::field(
            "Input examples",
            &agentic.input_examples.len().to_string(),
            ansi,
        ),
        render::field("Icons", &agentic.icons.len().to_string(), ansi),
    ];
    // add the report the API computed
    if let Some(report) = &meta.portability {
        lines.extend(portability_lines(report, ansi));
    }
    lines
}

/// Render a tool's details as text
///
/// # Arguments
///
/// * `tool` - The tool to describe
/// * `latest` - The tool's latest instance, when it was read
/// * `ansi` - Whether to style the text
fn describe_text(tool: &Entity, latest: Option<&Entity>, ansi: bool) -> String {
    // start with the tool's name and groups
    let mut lines = vec![render::header(&tool.name, &tool.groups.join(", "), ansi)];
    lines.push(render::field("ID", &tool.id.to_string(), ansi));
    lines.push(render::field("Kind", &tool_kind(tool), ansi));
    lines.push(render::field("Groups", &tool.groups.join(", "), ansi));
    // add the latest version and how many versions there are
    if let EntityMetadata::Tool(meta) = &tool.metadata {
        let latest = match (&meta.latest_version, meta.latest) {
            (Some(version), Some(id)) => format!("{version} ({id})"),
            _ => "none".to_owned(),
        };
        lines.push(render::field("Latest", &latest, ansi));
        lines.push(render::field(
            "Instances",
            &meta.instances.len().to_string(),
            ansi,
        ));
        lines.push(render::field("Modified", &meta.modified.to_rfc3339(), ansi));
    }
    // add the latest instance's model-API details
    if let Some(latest) = latest {
        lines.extend(instance_lines(latest, ansi));
    }
    // add the description if there is one
    lines.push(render::field(
        "Description",
        tool.description.as_deref().unwrap_or("-"),
        ansi,
    ));
    lines.join("\n")
}

/// One tool described in full
#[derive(Serialize)]
struct ToolView {
    /// The tool as the API returned it
    #[serde(flatten)]
    tool: Entity,
    /// The tool's latest agentic instance as the API returned it, with its portability report
    #[serde(skip_serializing_if = "Option::is_none")]
    latest_instance: Option<Entity>,
}

impl View for ToolView {
    /// Render the tool as a one row table
    fn table(&self) -> String {
        // reuse the list table for a single tool
        ToolsView(vec![self.tool.clone()]).table()
    }

    /// Render the tool as text
    ///
    /// # Arguments
    ///
    /// * `ansi` - Whether to style the text
    fn human(&self, ansi: bool) -> String {
        // describe the one tool and its latest instance
        describe_text(&self.tool, self.latest_instance.as_ref(), ansi)
    }
}

/// A tool's versions, newest first
#[derive(Serialize)]
#[serde(transparent)]
struct VersionsView {
    /// The versions as the API returned them
    versions: Vec<ToolInstanceSummary>,
    /// The tool's latest instance
    #[serde(skip)]
    latest: Option<Uuid>,
}

impl View for VersionsView {
    /// Render the versions as a table with the latest one marked
    fn table(&self) -> String {
        // build one row per version
        let rows = self
            .versions
            .iter()
            .map(|summary| {
                vec![
                    summary.version.to_string(),
                    summary.id.to_string(),
                    if Some(summary.id) == self.latest {
                        "latest"
                    } else {
                        ""
                    }
                    .to_owned(),
                    summary.deprecated.to_string(),
                    summary.created.to_rfc3339(),
                    summary.spec_hash.clone(),
                ]
            })
            .collect();
        output::table_from(
            &[
                "VERSION",
                "ID",
                "LATEST",
                "DEPRECATED",
                "CREATED",
                "SPEC HASH",
            ],
            rows,
        )
    }

    /// Render the versions as text
    ///
    /// # Arguments
    ///
    /// * `_ansi` - Whether to style the text (version lists are never styled)
    fn human(&self, _ansi: bool) -> String {
        // one line per version
        self.versions
            .iter()
            .map(|summary| {
                // mark the latest and deprecated versions
                let latest = if Some(summary.id) == self.latest {
                    " (latest)"
                } else {
                    ""
                };
                let deprecated = if summary.deprecated {
                    " (deprecated)"
                } else {
                    ""
                };
                format!("{} {}{latest}{deprecated}", summary.version, summary.id)
            })
            .collect::<Vec<String>>()
            .join("\n")
    }
}

/// Read a tool spec from a YAML or JSON file
///
/// # Arguments
///
/// * `path` - The path to the spec
fn read_spec(path: &Path) -> Result<ToolSpec, Error> {
    // read the spec file
    let raw = std::fs::read_to_string(path)
        .map_err(|err| Error::new(format!("Failed to read {}: {err}", path.display())))?;
    // a YAML document may name the spec's kind with a tag like `!Agentic`
    if let Ok(spec) = serde_norway::from_str::<ToolSpec>(&raw) {
        return Ok(spec);
    }
    // otherwise read the document, YAML or JSON, with the kind as its top-level key
    let value: serde_json::Value = serde_norway::from_str(&raw)
        .map_err(|err| Error::new(format!("Failed to parse {}: {err}", path.display())))?;
    // check that the document is a tool spec
    serde_json::from_value(value).map_err(|err| {
        Error::new(format!(
            "{} is not a valid tool spec: {err}",
            path.display()
        ))
    })
}

/// Find a tool by its ID or name
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `tool` - The tool's ID or name
pub(crate) async fn find_tool(thorium: &Thorium, tool: &str) -> Result<Entity, Error> {
    // an ID is read directly, and must be a tool
    if let Ok(id) = Uuid::parse_str(tool) {
        let found = thorium.entities.get(id).await?;
        if found.kind != EntityKinds::Tool {
            return Err(Error::new(format!(
                "{tool} is a {}, not a tool",
                found.kind
            )));
        }
        return Ok(found);
    }
    // otherwise find the tools with this name
    let opts = EntityListOpts::default()
        .kind(EntityKinds::Tool)
        .limit(10_000);
    let mut cursor = thorium.entities.list(&opts).await?;
    let mut matches = Vec::new();
    loop {
        // keep the tools with this exact name
        matches.extend(
            cursor
                .data
                .drain(..)
                .filter(|line| line.name == tool)
                .map(|line| line.id),
        );
        // stop once every page was read
        if cursor.exhausted() {
            break;
        }
        // get the next page of tools
        cursor.refill().await?;
    }
    // the name must name exactly one tool
    matches.sort();
    matches.dedup();
    match matches.as_slice() {
        [id] => thorium.entities.get(*id).await,
        [] => Err(Error::new(format!("Tool {tool} not found"))),
        ids => Err(Error::new(format!(
            "Several tools are named {tool}; use one of their IDs: {}",
            ids.iter()
                .map(ToString::to_string)
                .collect::<Vec<String>>()
                .join(", ")
        ))),
    }
}

/// List tools
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The get command to execute
async fn get(thorium: &Thorium, cmd: &GetTools) -> Result<(), Error> {
    // list tools in a table by default
    let format = cmd.format.format.unwrap_or(OutputFormat::Table);
    watch::show(format, &cmd.watch, Some(&cmd.wait), || async {
        // list the details of the tools in the requested groups
        let opts = EntityListOpts::default()
            .groups(cmd.group.clone())
            .kind(EntityKinds::Tool)
            .limit(cmd.limit);
        let mut cursor = thorium.entities.list_details(&opts).await?;
        // read pages until the limit or the last tool
        let mut tools = std::mem::take(&mut cursor.data);
        while !cursor.exhausted() {
            cursor.refill().await?;
            tools.append(&mut cursor.data);
        }
        Ok(ToolsView(tools))
    })
    .await
}

/// Describe a tool
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The describe command to execute
async fn describe(thorium: &Thorium, cmd: &DescribeTool) -> Result<(), Error> {
    // describe the tool in text by default
    let format = cmd.format.format.unwrap_or(OutputFormat::Human);
    watch::show(format, &cmd.watch, Some(&cmd.wait), || async {
        // find the tool
        let tool = find_tool(thorium, &cmd.tool).await?;
        // read an agentic tool's latest instance for its model-API fields and report
        let latest_instance = match &tool.metadata {
            EntityMetadata::Tool(meta) if meta.kind == thorium::models::ToolKind::Agentic => {
                match meta.latest {
                    Some(id) => Some(thorium.entities.get(id).await?),
                    None => None,
                }
            }
            _ => None,
        };
        Ok(ToolView {
            tool,
            latest_instance,
        })
    })
    .await
}

/// List a tool's versions
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The versions command to execute
async fn versions(thorium: &Thorium, cmd: &ToolVersions) -> Result<(), Error> {
    // find the tool and its latest instance
    let tool = find_tool(thorium, &cmd.tool).await?;
    let latest = match &tool.metadata {
        EntityMetadata::Tool(meta) => meta.latest,
        _ => None,
    };
    // list its versions
    let versions = thorium.entities.tool_instances(tool.id).await?;
    // print them in a table by default
    let format = cmd.format.format.unwrap_or(OutputFormat::Table);
    output::print(&VersionsView { versions, latest }, format)
}

/// Create a tool and its first version
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The create command to execute
async fn create(thorium: &Thorium, cmd: &CreateTool) -> Result<(), Error> {
    // read the first version's spec
    let spec = read_spec(&cmd.spec)?;
    // build the tool request
    let meta = ToolEntityRequest::new(&cmd.version, &spec)?;
    let mut req = EntityRequest::new(
        &cmd.name,
        EntityMetadataRequest::Tool(meta),
        cmd.group.clone(),
    );
    req.description.clone_from(&cmd.description);
    // create the tool
    let resp = thorium.entities.create(req).await?;
    // print both ids
    let instance = resp
        .tool_instance
        .map_or_else(|| "-".to_owned(), |id| id.to_string());
    println!(
        "Created tool {} ({}) with version {} ({instance})",
        resp.name, resp.id, cmd.version
    );
    // warn about anything that isn't portable across model APIs
    print_portability_warnings(resp.portability.as_ref());
    Ok(())
}

/// Publish a new version of a tool
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The publish command to execute
async fn publish(thorium: &Thorium, cmd: &PublishTool) -> Result<(), Error> {
    // read the new version's spec
    let spec = read_spec(&cmd.spec)?;
    let encoded = serde_json::to_string(&spec)?;
    // find the tool
    let tool = find_tool(thorium, &cmd.tool).await?;
    // publish the new version through a tool update
    let update = EntityUpdate::default().metadata(EntityMetadataUpdate::Tool {
        version: Some(cmd.version.clone()),
        spec: Some(encoded),
        provides: cmd.provides.clone(),
        harness_env: None,
        clear_harness_env: None,
        disable_models: BTreeSet::new(),
        enable_models: BTreeSet::new(),
    });
    thorium.entities.update(tool.id, update).await?;
    // print the new latest version
    let tool = thorium.entities.get(tool.id).await?;
    let latest_id = match &tool.metadata {
        EntityMetadata::Tool(meta) => meta.latest,
        _ => None,
    };
    let latest = latest_id.map_or_else(|| "-".to_owned(), |id| id.to_string());
    println!("Published {} version {} ({latest})", tool.name, cmd.version);
    // warn about anything in an agentic spec that isn't portable across model APIs
    if matches!(spec, ToolSpec::Agentic(_))
        && let Some(id) = latest_id
    {
        // read the new instance, which carries the report the API computed
        let instance = thorium.entities.get(id).await?;
        if let EntityMetadata::ToolInstance(meta) = &instance.metadata {
            print_portability_warnings(meta.portability.as_ref());
        }
    }
    Ok(())
}

/// Deprecate a tool version
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The deprecate command to execute
async fn deprecate(thorium: &Thorium, cmd: &DeprecateTool) -> Result<(), Error> {
    // mark the instance deprecated
    let update = EntityUpdate::default().metadata(EntityMetadataUpdate::ToolInstance {
        deprecated: Some(true),
    });
    thorium.entities.update(cmd.tool_instance, update).await?;
    println!("Deprecated tool version {}", cmd.tool_instance);
    Ok(())
}

/// Build a harness env from `--set` values, each a value name with an optional `=NAME` rename
///
/// # Arguments
///
/// * `sets` - The values to export
fn parse_env_sets(sets: &[String]) -> Result<HarnessEnv, Error> {
    // export each value, renaming the ones given a name
    let mut env = HarnessEnv::default();
    for set in sets {
        // split off the rename, if there is one
        let (value, name) = match set.split_once('=') {
            Some((value, name)) => (value, Some(name)),
            None => (set.as_str(), None),
        };
        // the value must be one a harness can be given
        let var: HarnessEnvVar =
            serde_json::from_value(serde_json::Value::String(value.to_owned()))
                .map_err(|_| Error::new(format!("`{value}` is not a harness env value")))?;
        env.export.insert(var);
        if let Some(name) = name {
            env.names.insert(var, name.to_owned());
        }
    }
    Ok(env)
}

/// Build an update that only changes a harness tool's root settings
///
/// # Arguments
///
/// * `harness_env` - The new JSON encoded env, if it's set
/// * `clear_harness_env` - Whether to clear the env
/// * `disable_models` - The model ids to disable
/// * `enable_models` - The model ids to enable
fn harness_update(
    harness_env: Option<String>,
    clear_harness_env: Option<bool>,
    disable_models: BTreeSet<String>,
    enable_models: BTreeSet<String>,
) -> EntityUpdate {
    // no version or spec, so nothing is published
    EntityUpdate::default().metadata(EntityMetadataUpdate::Tool {
        version: None,
        spec: None,
        provides: Vec::new(),
        harness_env,
        clear_harness_env,
        disable_models,
        enable_models,
    })
}

/// Set or clear a harness tool's env values
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The harness-env command to execute
async fn harness_env(thorium: &Thorium, cmd: &HarnessEnvCmd) -> Result<(), Error> {
    // find the harness
    let tool = find_tool(thorium, &cmd.harness).await?;
    // clear the env, or replace it with the values given
    let update = if cmd.clear {
        harness_update(None, Some(true), BTreeSet::new(), BTreeSet::new())
    } else {
        let env = parse_env_sets(&cmd.set)?;
        harness_update(
            Some(serde_json::to_string(&env)?),
            None,
            BTreeSet::new(),
            BTreeSet::new(),
        )
    };
    thorium.entities.update(tool.id, update).await?;
    // print what the harness's runs now get
    let tool = thorium.entities.get(tool.id).await?;
    let env = match &tool.metadata {
        EntityMetadata::Tool(meta) => meta.harness_env.clone(),
        _ => None,
    };
    match env {
        Some(env) if !env.export.is_empty() => {
            let names = env
                .export
                .iter()
                .map(|var| format!("{var:?}={}", env.name_of(*var)))
                .collect::<Vec<String>>();
            println!("Harness {} exports {}", tool.name, names.join(", "));
        }
        _ => println!("Harness {} exports no model values", tool.name),
    }
    Ok(())
}

/// Disable or enable model ids on a harness tool
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The harness-models command to execute
async fn harness_models(thorium: &Thorium, cmd: &HarnessModels) -> Result<(), Error> {
    // find the harness
    let tool = find_tool(thorium, &cmd.harness).await?;
    // disable and enable the ids given
    let update = harness_update(
        None,
        None,
        cmd.disable_model.iter().cloned().collect(),
        cmd.enable_model.iter().cloned().collect(),
    );
    thorium.entities.update(tool.id, update).await?;
    // print the ids now disabled
    let tool = thorium.entities.get(tool.id).await?;
    let disabled = match &tool.metadata {
        EntityMetadata::Tool(meta) => meta
            .disabled_models
            .iter()
            .cloned()
            .collect::<Vec<String>>(),
        _ => Vec::new(),
    };
    if disabled.is_empty() {
        println!("Harness {} disables no models", tool.name);
    } else {
        println!("Harness {} disables {}", tool.name, disabled.join(", "));
    }
    Ok(())
}

/// Run a tool instance through the shared tool-run helper
///
/// Nothing is checked against a run's `tools.json`: the API's token scope decides which tool
/// instances a run token may run.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The run command to execute
pub(crate) async fn run_tool(thorium: &Thorium, cmd: &RunTool) -> ToolRunReport {
    // read the tool call input, if one was given
    let input = match &cmd.input {
        Some(raw) => match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(serde_json::Value::Object(input)) => input,
            Ok(_) => return ToolRunReport::usage("--input must be a JSON object"),
            Err(error) => {
                return ToolRunReport::usage(format!("--input isn't valid JSON: {error}"));
            }
        },
        None => serde_json::Map::new(),
    };
    // find the tool instance and plan its run
    let entity = match find_instance(thorium, &cmd.tool_instance).await {
        Ok(entity) => entity,
        Err(error) => return ToolRunReport::failed(error.to_string()),
    };
    let plan = match tool_runs::plan(&entity, &input) {
        Ok(plan) => plan,
        Err(error) => return ToolRunReport::usage(error),
    };
    // upload local files to the run group, or the instance's groups when none was given
    let groups = match &cmd.group {
        Some(group) => vec![group.clone()],
        None => entity.groups.clone(),
    };
    let mut inputs = match run_inputs(thorium, &cmd.inputs, &groups).await {
        Ok(inputs) => inputs,
        Err(error) => return ToolRunReport::failed(error.to_string()),
    };
    // a container tool's mapped input may name more files and gives the job's args
    let mut args = match plan.kind {
        ToolRunKind::Job(mapped) => {
            for sample in mapped.samples.unwrap_or_default() {
                inputs = inputs.sample(sample);
            }
            mapped.args
        }
        ToolRunKind::Mcp { .. } => GenericJobArgs::default(),
    };
    args.positionals.extend(cmd.args.iter().cloned());
    let mut req = ToolRunRequest::new(inputs).args(args);
    if let Some(group) = &cmd.group {
        req = req.group(group);
    }
    // run it, waiting only when asked
    let mut opts = ToolRunOptions::new(cmd.output.clone().unwrap_or(plan.output));
    opts.wait = cmd.wait;
    opts.max_result_bytes = plan.max_result_bytes;
    tool_runs::run(thorium, entity.id, &groups, &req, &opts, &mut NoHooks).await
}

/// Get a tool run's status through the shared tool-run helper
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The status command to execute
async fn run_status(thorium: &Thorium, cmd: &ToolRunStatus) -> ToolRunReport {
    tool_runs::status(thorium, cmd.reaction, cmd.group.as_deref()).await
}

/// Download a tool run's results through the shared tool-run helper
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The results command to execute
async fn run_results(thorium: &Thorium, cmd: &ToolRunResults) -> ToolRunReport {
    // download into the given directory or the run's default one
    let output = cmd
        .output
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("{DEFAULT_TOOL_RESULTS_DIR}/{}", cmd.reaction)));
    let opts = ToolRunOptions::new(output);
    tool_runs::results(
        thorium,
        cmd.reaction,
        cmd.group.as_deref(),
        &opts,
        &mut NoHooks,
    )
    .await
}

/// Handle all tools commands
///
/// # Arguments
///
/// * `args` - The arguments passed to Thorctl
/// * `cmd` - The tools command to execute
pub async fn handle(args: &Args, cmd: &Tools) -> Result<(), Error> {
    // load our config and instance our client
    let (_, thorium) = utils::get_client(args).await?;
    // tool run commands print one JSON object and exit with its code
    let report = match cmd {
        Tools::Run(cmd) => Some(run_tool(&thorium, cmd).await),
        Tools::Runs(ToolRuns::Status(cmd)) => Some(run_status(&thorium, cmd).await),
        Tools::Runs(ToolRuns::Results(cmd)) => Some(run_results(&thorium, cmd).await),
        _ => None,
    };
    if let Some(report) = report {
        println!("{}", report.output.to_line());
        let code = report.exit.code();
        if code != 0 {
            std::process::exit(code);
        }
        return Ok(());
    }
    // call the right tools handler
    let result = match cmd {
        Tools::Get(cmd) => get(&thorium, cmd).await,
        Tools::Describe(cmd) => describe(&thorium, cmd).await,
        Tools::Versions(cmd) => versions(&thorium, cmd).await,
        Tools::Create(cmd) => create(&thorium, cmd).await,
        Tools::Publish(cmd) => publish(&thorium, cmd).await,
        Tools::Deprecate(cmd) => deprecate(&thorium, cmd).await,
        Tools::HarnessEnv(cmd) => harness_env(&thorium, cmd).await,
        Tools::HarnessModels(cmd) => harness_models(&thorium, cmd).await,
        Tools::Run(_) | Tools::Runs(_) => Ok(()),
    };
    // show the API's status code with any refusal
    result.map_err(|err| match err.status() {
        Some(_) => Error::new(err.to_string()),
        None => err,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use thorium::Thorium;
    use thorium::models::{
        AgenticTool, AgenticToolBacking, ContainerToolBacking, Entity, EntityKinds, EntityMetadata,
        PlannedStage, Reaction, ReactionPlan, ReactionSource, ReactionStatus, ToolInstanceEntity,
        ToolSpec, ToolVersion,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use uuid::Uuid;

    use super::run_tool;
    use crate::args::tools::RunTool;

    /// The line `thorium-agent tool run` prints for the same fixed run
    const GOLDEN: &str = r#"{"status":"Completed","reaction":"2f1e7a4c-9b3d-4c55-8e21-6a0f3b9d7c11","result":{"functions":412},"files":[],"error":null,"retry_after_secs":null}"#;

    /// Start a mock API answering each request line with its router
    ///
    /// Returns the mock's url and the request lines it received.
    ///
    /// # Arguments
    ///
    /// * `router` - Picks the status and body of each answer from its request line
    async fn mock<F>(router: F) -> (String, Arc<Mutex<Vec<String>>>)
    where
        F: Fn(&str) -> (u16, String) + Send + Sync + 'static,
    {
        // bind to a random local port and answer in the background
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let lines = Arc::new(Mutex::new(Vec::new()));
        let inner = lines.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                // read the request head and its body
                let mut buf = Vec::new();
                let mut chunk = [0_u8; 8192];
                loop {
                    let read = stream.read(&mut chunk).await.unwrap_or(0);
                    buf.extend_from_slice(&chunk[..read]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    let done = text.find("\r\n\r\n").is_some_and(|end| {
                        let length = text[..end]
                            .lines()
                            .find_map(|line| {
                                line.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|len| len.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        buf.len() >= end + 4 + length
                    });
                    if done || read == 0 {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&buf).to_string();
                let line = text.lines().next().unwrap_or_default().to_owned();
                inner.lock().unwrap().push(line.clone());
                let (code, body) = router(&line);
                let resp = format!(
                    "HTTP/1.1 {code} Mock\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        (host, lines)
    }

    /// `tools run --wait` prints byte for byte what `thorium-agent tool run` prints for the same
    /// run, through the shared tool-run helper
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn tools_run_matches_the_wrapper_golden() {
        // a container tool whose run completes with a small result and no files
        let tool_instance = Uuid::new_v4();
        let reaction = Uuid::parse_str("2f1e7a4c-9b3d-4c55-8e21-6a0f3b9d7c11").unwrap();
        let spec = ToolSpec::Agentic(AgenticTool {
            function_name: "run_ghidra".to_owned(),
            title: None,
            model_description: "Decompile a file".to_owned(),
            input_schema: serde_json::Map::new(),
            output_schema: None,
            capabilities: thorium::models::ToolCapabilities::default(),
            backing: AgenticToolBacking::Container(ContainerToolBacking::default()),
            result_format: thorium::models::ToolResultFormat::Json,
            requires_approval: false,
            timeout: Some(600),
            input_examples: Vec::new(),
            strict: thorium::models::ToolStrictness::Auto,
            loading: thorium::models::ToolLoading::Eager,
            icons: Vec::new(),
        });
        let entity = Entity {
            id: tool_instance,
            name: "harvester@1.2.0".to_owned(),
            kind: EntityKinds::ToolInstance,
            metadata: EntityMetadata::ToolInstance(ToolInstanceEntity {
                tool: Uuid::new_v4(),
                version: ToolVersion::from("1.2.0"),
                previous: None,
                spec_hash: "hash".to_owned(),
                deprecated: false,
                spec,
                portability: None,
                provides: Vec::new(),
            }),
            description: None,
            submitter: "mcarson".to_owned(),
            groups: vec!["corn".to_owned()],
            tags: std::collections::HashMap::default(),
            image: None,
            created: chrono::Utc::now(),
        };
        let completed = Reaction {
            id: reaction,
            group: "corn".to_owned(),
            creator: "mcarson".to_owned(),
            pipeline: "harvester".to_owned(),
            status: ReactionStatus::Completed,
            current_stage: 0,
            current_stage_progress: 0,
            current_stage_length: 1,
            args: std::collections::HashMap::default(),
            sla: chrono::Utc::now(),
            jobs: Vec::new(),
            tags: Vec::new(),
            parent: None,
            sub_reactions: 0,
            completed_sub_reactions: 0,
            generators: Vec::new(),
            samples: vec![
                "63b9c2cbd4f4f1f6c2a0c2a1d7e0d2e6c2b6a8a3b1c4d5e6f7a8b9c0d1e2f3a4".to_owned(),
            ],
            ephemeral: Vec::new(),
            parent_ephemeral: std::collections::HashMap::default(),
            repos: Vec::new(),
            trigger_depth: None,
            has_cache: false,
            source: ReactionSource::Tool {
                tool_instance,
                provenance: thorium::models::RunProvenance::default(),
            },
            plan: ReactionPlan {
                stages: vec![vec![PlannedStage {
                    name: "harvester".to_owned(),
                    tool_instance,
                }]],
            },
            run_token: None,
            run: None,
        };
        let entity_json = serde_json::to_string(&entity).unwrap();
        let reaction_json = serde_json::to_string(&completed).unwrap();
        let outputs = json!({"results": {"harvester": [{
            "id": Uuid::new_v4(), "groups": ["corn"], "cmd": null, "uploaded": chrono::Utc::now(),
            "result": {"functions": 412}, "entities": {}, "display_type": "Json", "children": {},
        }]}})
        .to_string();
        let (host, _) = mock(move |line| {
            if line.starts_with("GET /api/entities/") {
                (200, entity_json.clone())
            } else if line.starts_with("POST /api/tools/instances/") {
                (200, json!({"id": reaction}).to_string())
            } else if line.starts_with("GET /api/reactions/corn/") {
                (200, reaction_json.clone())
            } else if line.starts_with("GET /api/files/results/") {
                (200, outputs.clone())
            } else {
                (404, r#"{"error":"not found"}"#.to_owned())
            }
        })
        .await;
        let thorium = Thorium::build(&host).token("token").build().await.unwrap();
        // run it with thorctl's command and compare the printed line to the wrapper's
        let output = std::env::temp_dir().join(format!("thorctl-tools-run-{}", Uuid::new_v4()));
        let cmd = RunTool {
            tool_instance: tool_instance.to_string(),
            group: None,
            inputs: crate::args::runs::RunInputArgs::default(),
            input: Some("{}".to_owned()),
            args: Vec::new(),
            wait: true,
            output: Some(output.clone()),
        };
        let report = run_tool(&thorium, &cmd).await;
        assert_eq!(report.output.to_line(), GOLDEN);
        assert_eq!(report.exit.code(), 0);
        let _ = std::fs::remove_dir_all(&output);
    }
}
