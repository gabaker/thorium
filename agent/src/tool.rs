//! The `tool` subcommands: the CLI wrapper harnesses run tools behind Thorium with
//!
//! `thorium-agent tool run`, `tool status`, and `tool results` each print exactly one JSON
//! object. Inside a workflow run (a `run.json` exists) `tool run` only runs the tool instances
//! listed in the run's `tools.json`, checked before any API call; the API's run-token scope checks
//! every tool run again.

use clap::{ArgGroup, Parser, Subcommand};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use thorium::Thorium;
use thorium::client::Keys;
use thorium::client::tool_runs::{self, ToolRunKind, ToolRunOptions, ToolRunReport};
use thorium::models::{
    DEFAULT_TOOL_RESULTS_DIR, HarnessRunFile, HarnessToolEntry, RunInputs, ToolRunRequest,
};
use uuid::Uuid;

use crate::libs;
use crate::libs::tool_runs::{DEFAULT_TOOL_RUNS, JsonlHooks, ToolRunLine};

/// Where the thorium-agent writes the run's keys file
pub const DEFAULT_KEYS: &str = "/tmp/thorium/keys.yml";

/// Where the API's run request file is downloaded
pub const DEFAULT_RUN_FILE: &str = "/tmp/thorium/ephemeral/run.json";

/// The name of the run's tools file, next to its run request file
pub const TOOLS_FILE: &str = "tools.json";

/// The env var holding the Thorium API url in per-run pods
pub const URL_ENV: &str = "THORIUM_URL";

/// The env var holding the run-scoped token in per-run pods
pub const TOKEN_ENV: &str = "THORIUM_TOKEN";

/// The CLI wrapper's args
#[derive(Parser, Debug)]
#[clap(name = "thorium-agent tool", version, author)]
pub struct ToolArgs {
    /// The tool command to run
    #[clap(subcommand)]
    pub cmd: ToolCmd,
    /// The keys file to authenticate with when `THORIUM_URL` and `THORIUM_TOKEN` aren't set
    #[clap(long, global = true, default_value = DEFAULT_KEYS)]
    pub keys: PathBuf,
    /// The run's request file; the run's `tools.json` is next to it
    #[clap(long, global = true, default_value = DEFAULT_RUN_FILE)]
    pub run_file: PathBuf,
    /// Where the tool runs this wrapper starts are recorded
    #[clap(long, global = true, default_value = DEFAULT_TOOL_RUNS)]
    pub tool_runs: PathBuf,
}

/// The tool commands
#[derive(Subcommand, Debug)]
pub enum ToolCmd {
    /// Run one tool and, by default, wait for it
    Run(RunTool),
    /// Print a tool run's status without waiting
    Status(StatusTool),
    /// Download and print a finished tool run's results
    Results(ResultsTool),
}

/// The args of `tool run`
#[derive(clap::Args, Debug)]
#[clap(group(ArgGroup::new("call_input").required(true).args(["input", "input_file"])))]
pub struct RunTool {
    /// The tool instance to run
    pub tool_instance: Uuid,
    /// The tool call's input as a JSON object
    #[clap(long)]
    pub input: Option<String>,
    /// A file holding the tool call's input as a JSON object
    #[clap(long)]
    pub input_file: Option<PathBuf>,
    /// The directory to download results into (the tool's output path by default)
    #[clap(long)]
    pub output: Option<PathBuf>,
    /// The most seconds to wait (the tool's timeout by default; it can only be shortened)
    #[clap(long)]
    pub timeout: Option<u64>,
    /// Print the run's reaction id right after starting it
    #[clap(long)]
    pub no_wait: bool,
}

/// The args of `tool status`
#[derive(clap::Args, Debug)]
pub struct StatusTool {
    /// The tool run's reaction id
    pub reaction: Uuid,
}

/// The args of `tool results`
#[derive(clap::Args, Debug)]
pub struct ResultsTool {
    /// The tool run's reaction id
    pub reaction: Uuid,
    /// The directory to download results into (where `tool run` would have put them by default)
    #[clap(long)]
    pub output: Option<PathBuf>,
}

/// Check that a tool instance is one the run's `tools.json` lists
///
/// Outside a workflow run there's no list, so every tool instance is left to the API's token
/// scope.
///
/// # Arguments
///
/// * `tools` - The run's `tools.json` entries, when inside a workflow run
/// * `tool_instance` - The tool instance to run
pub fn check_listed(
    tools: Option<&BTreeMap<Uuid, HarnessToolEntry>>,
    tool_instance: Uuid,
) -> Result<Option<&HarnessToolEntry>, String> {
    // outside a workflow run the API's token scope alone decides
    let Some(tools) = tools else {
        return Ok(None);
    };
    // inside one only the tools the run's tools.json lists can be run
    match tools.get(&tool_instance) {
        Some(entry) => Ok(Some(entry)),
        None => Err(format!(
            "tool instance {tool_instance} isn't one of this run's tools in {TOOLS_FILE}"
        )),
    }
}

/// The workflow run the wrapper runs tools for
struct RunContext {
    /// The run's request file
    run: HarnessRunFile,
    /// The run's tools, by tool instance
    tools: BTreeMap<Uuid, HarnessToolEntry>,
}

/// Read the workflow run the wrapper is in, if it's in one
///
/// No run file means no workflow run. A run file without a readable `tools.json` beside it is an
/// error, so the run's tool list can't be skipped by removing it.
///
/// # Arguments
///
/// * `run_file` - The run's request file
fn read_run(run_file: &Path) -> Result<Option<RunContext>, String> {
    // no run file means the wrapper isn't in a workflow run
    let text = match std::fs::read_to_string(run_file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!("unable to read {}: {error}", run_file.display()));
        }
    };
    let run: HarnessRunFile = serde_json::from_str(&text)
        .map_err(|error| format!("unable to parse {}: {error}", run_file.display()))?;
    // the run's tools are listed next to its run file
    let tools_file = run_file.with_file_name(TOOLS_FILE);
    let text = std::fs::read_to_string(&tools_file)
        .map_err(|error| format!("unable to read {}: {error}", tools_file.display()))?;
    let tools = serde_json::from_str(&text)
        .map_err(|error| format!("unable to parse {}: {error}", tools_file.display()))?;
    Ok(Some(RunContext { run, tools }))
}

/// Get the keys to authenticate with: the pod env values when both are set, else the keys file
///
/// # Arguments
///
/// * `args` - The tool command's args
fn load_keys(args: &ToolArgs) -> Result<Keys, String> {
    // a per-run pod gives the url and token as env values
    match (std::env::var(URL_ENV), std::env::var(TOKEN_ENV)) {
        (Ok(url), Ok(token)) if !url.is_empty() && !token.is_empty() => {
            Ok(Keys::new_token(url, token))
        }
        // otherwise read the keys file the thorium-agent wrote
        _ => Keys::from_path(&args.keys).map_err(|error| {
            format!(
                "unable to read the keys file {}: {error}",
                args.keys.display()
            )
        }),
    }
}

/// Read a tool call's input object from `--input` or `--input-file`
///
/// # Arguments
///
/// * `cmd` - The `tool run` args
fn read_input(cmd: &RunTool) -> Result<serde_json::Map<String, Value>, String> {
    // get the input's text from the flag or the file
    let text = match (&cmd.input, &cmd.input_file) {
        (Some(input), _) => input.clone(),
        (None, Some(path)) => std::fs::read_to_string(path)
            .map_err(|error| format!("unable to read {}: {error}", path.display()))?,
        (None, None) => return Err("one of --input or --input-file is required".to_owned()),
    };
    // the input must be a JSON object
    match serde_json::from_str(&text) {
        Ok(Value::Object(input)) => Ok(input),
        Ok(_) => Err("the tool call's input must be a JSON object".to_owned()),
        Err(error) => Err(format!("the tool call's input isn't valid JSON: {error}")),
    }
}

/// Run one tool and, by default, wait for it
///
/// # Arguments
///
/// * `args` - The tool command's args
/// * `cmd` - The `tool run` args
async fn run(args: &ToolArgs, cmd: &RunTool) -> ToolRunReport {
    // read the input and the run before making any API call
    let input = match read_input(cmd) {
        Ok(input) => input,
        Err(error) => return ToolRunReport::usage(error),
    };
    let context = match read_run(&args.run_file) {
        Ok(context) => context,
        Err(error) => return ToolRunReport::usage(error),
    };
    // inside a workflow run only the run's own tools are run
    let entry = match check_listed(
        context.as_ref().map(|context| &context.tools),
        cmd.tool_instance,
    ) {
        Ok(entry) => entry.cloned(),
        Err(error) => return ToolRunReport::usage(error),
    };
    // a timeout can only shorten the tool's own timeout
    let too_long = |limit: Option<u64>| match (cmd.timeout, limit) {
        (Some(asked), Some(limit)) if asked > limit => Some(format!(
            "--timeout {asked} is longer than the tool's timeout of {limit} seconds"
        )),
        _ => None,
    };
    if let Some(error) = too_long(entry.as_ref().and_then(|entry| entry.timeout)) {
        return ToolRunReport::usage(error);
    }
    // connect with the run's token
    let keys = match load_keys(args) {
        Ok(keys) => keys,
        Err(error) => return ToolRunReport::usage(error),
    };
    let thorium = match Thorium::from_keys(keys.clone()).await {
        Ok(thorium) => thorium,
        Err(error) => return ToolRunReport::failed(error.to_string()),
    };
    // read the tool instance and plan its run
    let entity = match thorium.entities.get(cmd.tool_instance).await {
        Ok(entity) => entity,
        Err(error) => return ToolRunReport::failed(error.to_string()),
    };
    let plan = match tool_runs::plan(&entity, &input) {
        Ok(plan) => plan,
        Err(error) => return ToolRunReport::usage(error),
    };
    let limit = entry
        .as_ref()
        .and_then(|entry| entry.timeout)
        .or(plan.timeout);
    if let Some(error) = too_long(limit) {
        return ToolRunReport::usage(error);
    }
    // follow the run for at most the timeout and return at most the run's result size
    let max_result_bytes = entry
        .as_ref()
        .map_or(plan.max_result_bytes, |entry| entry.max_result_bytes);
    let mut opts = ToolRunOptions::new(cmd.output.clone().unwrap_or(plan.output));
    opts.timeout = cmd.timeout.or(limit).map(Duration::from_secs);
    opts.wait = !cmd.no_wait;
    opts.max_result_bytes = max_result_bytes;
    match plan.kind {
        // a Thorium MCP tool is called through Thorium's MCP server
        ToolRunKind::Mcp { tool, input } => {
            let Some(token) = &keys.token else {
                return ToolRunReport::usage("calling a Thorium MCP tool needs a token");
            };
            let url = format!("{}/api/mcp", keys.api.trim_end_matches('/'));
            tool_runs::call_mcp(&url, token, &tool, &input, &opts).await
        }
        // any other tool runs as a sub run of the workflow run
        ToolRunKind::Job(mapped) => {
            let inputs = mapped
                .samples
                .unwrap_or_default()
                .into_iter()
                .fold(RunInputs::default(), RunInputs::sample);
            let mut req = ToolRunRequest::new(inputs).args(mapped.args);
            if let Some(context) = &context {
                req = req.parent(context.run.reaction);
            }
            // record the run in tool-runs.jsonl when it starts and when it's collected
            let mut hooks = JsonlHooks {
                path: args.tool_runs.clone(),
                tool_instance: Some(cmd.tool_instance),
                output: opts.output.clone(),
                max_result_bytes,
            };
            tool_runs::run(
                &thorium,
                cmd.tool_instance,
                &entity.groups,
                &req,
                &opts,
                &mut hooks,
            )
            .await
        }
    }
}

/// Build a client from the tool command's keys
///
/// # Arguments
///
/// * `args` - The tool command's args
async fn client(args: &ToolArgs) -> Result<Thorium, Box<ToolRunReport>> {
    // read the keys, then connect
    let keys = load_keys(args).map_err(|error| Box::new(ToolRunReport::usage(error)))?;
    Thorium::from_keys(keys)
        .await
        .map_err(|error| Box::new(ToolRunReport::failed(error.to_string())))
}

/// Get what `tool-runs.jsonl` recorded about a run, if anything
///
/// # Arguments
///
/// * `args` - The tool command's args
/// * `reaction` - The run's reaction
fn recorded(args: &ToolArgs, reaction: Uuid) -> Option<ToolRunLine> {
    // an unreadable record is the same as none
    libs::tool_runs::read(&args.tool_runs)
        .ok()
        .and_then(|mut runs| runs.remove(&reaction))
}

/// Print a tool run's status without waiting
///
/// # Arguments
///
/// * `args` - The tool command's args
/// * `cmd` - The `tool status` args
async fn status(args: &ToolArgs, cmd: &StatusTool) -> ToolRunReport {
    // connect with the run's token
    let thorium = match client(args).await {
        Ok(thorium) => thorium,
        Err(report) => return *report,
    };
    // read the run from the group it was started in, when the wrapper started it
    let line = recorded(args, cmd.reaction);
    tool_runs::status(
        &thorium,
        cmd.reaction,
        line.as_ref().map(|line| line.group.as_str()),
    )
    .await
}

/// Download and print a finished tool run's results
///
/// # Arguments
///
/// * `args` - The tool command's args
/// * `cmd` - The `tool results` args
async fn results(args: &ToolArgs, cmd: &ResultsTool) -> ToolRunReport {
    // connect with the run's token
    let thorium = match client(args).await {
        Ok(thorium) => thorium,
        Err(report) => return *report,
    };
    // collect into the path and with the limit the run was started with, when the wrapper
    // started it
    let line = recorded(args, cmd.reaction);
    let output = cmd
        .output
        .clone()
        .or_else(|| line.as_ref().and_then(|line| line.output.clone()))
        .unwrap_or_else(|| PathBuf::from(format!("{DEFAULT_TOOL_RESULTS_DIR}/{}", cmd.reaction)));
    let mut opts = ToolRunOptions::new(output);
    opts.max_result_bytes = line.as_ref().map_or(0, |line| line.max_result_bytes);
    let mut hooks = JsonlHooks {
        path: args.tool_runs.clone(),
        tool_instance: line.as_ref().and_then(|line| line.tool_instance),
        output: opts.output.clone(),
        max_result_bytes: opts.max_result_bytes,
    };
    let group = line.as_ref().map(|line| line.group.as_str());
    tool_runs::results(&thorium, cmd.reaction, group, &opts, &mut hooks).await
}

/// Run a tool command and get what it prints and how it exits
///
/// # Arguments
///
/// * `args` - The tool command's args
pub async fn execute(args: &ToolArgs) -> ToolRunReport {
    // run the right command
    match &args.cmd {
        ToolCmd::Run(cmd) => run(args, cmd).await,
        ToolCmd::Status(cmd) => status(args, cmd).await,
        ToolCmd::Results(cmd) => results(args, cmd).await,
    }
}

/// Run a tool command line and get the text it prints and its exit code
///
/// Help and version requests get clap's text and exit 0; any other parse error is a usage error
/// printed as the JSON object.
///
/// # Arguments
///
/// * `argv` - The command line starting at the `tool` argument
pub async fn run_line(argv: &[String]) -> (String, i32) {
    // parse the command line
    match ToolArgs::try_parse_from(argv) {
        Ok(parsed) => {
            // run the command and print its one object
            let report = execute(&parsed).await;
            (report.output.to_line(), report.exit.code())
        }
        // help and version requests print clap's text
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            (error.to_string().trim_end().to_owned(), 0)
        }
        // any other parse error is a usage error
        Err(error) => {
            let report = ToolRunReport::usage(error.to_string().trim_end());
            (report.output.to_line(), report.exit.code())
        }
    }
}

/// Run a tool command line, print what it prints, and get its exit code
///
/// # Arguments
///
/// * `argv` - The command line starting at the `tool` argument
pub async fn main(argv: Vec<String>) -> i32 {
    // run the command line and print its one JSON object or help text
    let (printed, code) = run_line(&argv).await;
    println!("{printed}");
    code
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use serde_json::{Value, json};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use thorium::Thorium;
    use thorium::client::tool_runs::{self as helper, NoHooks, ToolRunOptions};
    use thorium::models::{
        AgenticTool, AgenticToolBacking, ArgTarget, ContainerToolBacking, HarnessRunFile,
        HarnessToolEntry, ReactionStatus, RunInputs, ToolAnnotations, ToolCapabilities,
        ToolInputMapping, ToolInvocation, ToolOutputDelivery, ToolPortabilityReport,
        ToolResultFormat, ToolRunRequest, ToolSpec,
    };
    use uuid::Uuid;

    use super::{ToolArgs, run_line};
    use crate::libs::test_server::{self, MockThorium, SHA256};
    use crate::libs::tool_runs;

    /// The token test keys files hold; it must never be printed
    const TOKEN: &str = "run-secret-token-6d1f";

    /// The line `tool run` prints for the fixed run every client of the shared helper is given
    pub const GOLDEN: &str = r#"{"status":"Completed","reaction":"2f1e7a4c-9b3d-4c55-8e21-6a0f3b9d7c11","result":{"functions":412},"files":[],"error":null,"retry_after_secs":null}"#;

    /// The reaction id of the fixed run behind the golden line
    const GOLDEN_REACTION: &str = "2f1e7a4c-9b3d-4c55-8e21-6a0f3b9d7c11";

    /// A tool run the mock API serves
    struct Scenario {
        /// The tool instance being run
        tool_instance: Uuid,
        /// The tool instance's spec
        spec: ToolSpec,
        /// The run's reaction
        reaction: Uuid,
        /// How many reads return the run as started before it finishes
        started_reads: usize,
        /// The status the run finishes with
        finished: ReactionStatus,
        /// The result stored on the run's input file
        result: Value,
        /// The names of the run's result files
        files: Vec<String>,
    }

    impl Scenario {
        /// A container tool run that completes on its first read with a small result
        ///
        /// # Arguments
        ///
        /// * `output` - Where the tool delivers its results
        fn container(output: &Path) -> Self {
            // a tool that maps a depth onto a kwarg
            let mut args = BTreeMap::new();
            args.insert("depth".to_owned(), ArgTarget::Kwarg("--depth".to_owned()));
            let backing = AgenticToolBacking::Container(ContainerToolBacking {
                tool_instance: Uuid::new_v4(),
                inputs: ToolInputMapping {
                    args,
                    samples: None,
                },
                outputs: ToolOutputDelivery {
                    path: Some(output.to_string_lossy().into_owned()),
                    max_result_bytes: 0,
                },
            });
            Scenario {
                tool_instance: Uuid::new_v4(),
                spec: ToolSpec::Agentic(agentic(backing)),
                reaction: Uuid::new_v4(),
                started_reads: 0,
                finished: ReactionStatus::Completed,
                result: json!({"functions": 412}),
                files: vec!["decompiled.c".to_owned()],
            }
        }

        /// Start a mock API serving this run
        async fn serve(&self) -> MockThorium {
            // the tool instance, the run's reaction while started and once finished, its results,
            // and its result files
            let entity = test_server::tool_instance_entity(
                self.tool_instance,
                "harvester",
                "1.2.0",
                self.spec.clone(),
            );
            let entity_json = serde_json::to_string(&entity).unwrap();
            let id = self.reaction;
            let mut started = test_server::reaction(id, ReactionStatus::Started, None);
            started.parent = None;
            let started_json = serde_json::to_string(&started).unwrap();
            let mut finished = started.clone();
            finished.status = self.finished.clone();
            let finished_json = serde_json::to_string(&finished).unwrap();
            let output = thorium::models::Output {
                id: Uuid::new_v4(),
                groups: vec!["corn".to_owned()],
                tool_version: None,
                cmd: None,
                uploaded: chrono::Utc::now(),
                deserialization_error: None,
                result: self.result.clone(),
                files: self.files.clone(),
                entities: std::collections::HashMap::default(),
                display_type: thorium::models::OutputDisplayType::Json,
                children: std::collections::HashMap::default(),
            };
            let outputs = json!({"results": {"harvester": [output]}}).to_string();
            let entity_path = format!("GET /api/entities/{} ", self.tool_instance);
            let runs_path = format!("POST /api/tools/instances/{}/runs ", self.tool_instance);
            let reaction_path = format!("GET /api/reactions/corn/{id} ");
            let started_reads = self.started_reads;
            let reads = Arc::new(Mutex::new(0_usize));
            MockThorium::start(move |line| {
                if line.starts_with(&entity_path) {
                    (200, entity_json.clone())
                } else if line.starts_with(&runs_path) {
                    (200, json!({"id": id}).to_string())
                } else if line.starts_with(&reaction_path) {
                    let mut count = reads.lock().unwrap();
                    *count += 1;
                    if *count <= started_reads {
                        (200, started_json.clone())
                    } else {
                        (200, finished_json.clone())
                    }
                } else if line.starts_with("GET /api/reactions/logs/") {
                    (200, "[]".to_owned())
                } else if line.starts_with("GET /api/users/whoami ") {
                    (200, test_server::user_json())
                } else if line.starts_with("GET /api/files/results/") {
                    (200, outputs.clone())
                } else if line.starts_with("GET /api/files/result-files/") {
                    (200, "int main() {}".to_owned())
                } else {
                    (404, r#"{"error":"not found"}"#.to_owned())
                }
            })
            .await
        }
    }

    /// Build an agentic tool with a backing and a 600 second timeout
    ///
    /// # Arguments
    ///
    /// * `backing` - The tool's backing
    fn agentic(backing: AgenticToolBacking) -> AgenticTool {
        AgenticTool {
            function_name: "run_ghidra".to_owned(),
            title: None,
            model_description: "Decompile a file".to_owned(),
            input_schema: serde_json::Map::new(),
            output_schema: None,
            capabilities: ToolCapabilities::default(),
            backing,
            result_format: ToolResultFormat::Json,
            requires_approval: false,
            timeout: Some(600),
            input_examples: Vec::new(),
            strict: thorium::models::ToolStrictness::Auto,
            loading: thorium::models::ToolLoading::Eager,
            icons: Vec::new(),
        }
    }

    /// Build the `tools.json` entry of a tool bound to the CLI wrapper
    ///
    /// # Arguments
    ///
    /// * `tool_instance` - The bound tool instance
    /// * `max_result_bytes` - The effective result size limit
    fn entry(tool_instance: Uuid, max_result_bytes: u64) -> HarnessToolEntry {
        HarnessToolEntry {
            function_name: "run_ghidra".to_owned(),
            title: None,
            model_description: "Decompile a file".to_owned(),
            input_schema: serde_json::Map::new(),
            output_schema: None,
            input_examples: Vec::new(),
            strict: thorium::models::ToolStrictness::Auto,
            loading: thorium::models::ToolLoading::Eager,
            icons: Vec::new(),
            capabilities: ToolCapabilities::default(),
            annotations: ToolAnnotations::default(),
            portability: ToolPortabilityReport::default(),
            result_format: ToolResultFormat::Json,
            requires_approval: false,
            max_result_bytes,
            timeout: Some(600),
            invocation: ToolInvocation::Cli {
                command: vec![
                    "/opt/thorium/thorium-agent".to_owned(),
                    "tool".to_owned(),
                    "run".to_owned(),
                    tool_instance.to_string(),
                ],
            },
        }
    }

    /// A scratch run directory with a keys file for a mock API, and optionally a workflow run's
    /// `run.json` and `tools.json`
    struct RunDir {
        /// The scratch directory
        dir: PathBuf,
        /// The workflow run's reaction, when there is a run
        run: Option<Uuid>,
    }

    impl RunDir {
        /// Write a keys file and, when tools are given, a workflow run listing them
        ///
        /// # Arguments
        ///
        /// * `host` - The mock API's url
        /// * `tools` - The `tools.json` entries, or none outside a workflow run
        fn new(host: &str, tools: Option<BTreeMap<Uuid, HarnessToolEntry>>) -> Self {
            // a keys file holding the run's token
            let dir = test_server::scratch();
            std::fs::write(
                dir.join("keys.yml"),
                format!("api: {host}\ntoken: {TOKEN}\n"),
            )
            .unwrap();
            // a workflow run's request and tools files
            let run = tools.map(|tools| {
                let run = HarnessRunFile {
                    reaction: Uuid::new_v4(),
                    samples: vec![SHA256.to_owned()],
                    repos: Vec::new(),
                    workflow_instance: Uuid::new_v4(),
                    digest: "sha256:abc".to_owned(),
                    agent: Uuid::new_v4(),
                    limits: thorium::models::WorkflowLimits::default(),
                    max_result_bytes: 0,
                    models: Vec::new(),
                };
                std::fs::write(dir.join("run.json"), serde_json::to_string(&run).unwrap()).unwrap();
                std::fs::write(
                    dir.join("tools.json"),
                    serde_json::to_string(&tools).unwrap(),
                )
                .unwrap();
                run.reaction
            });
            RunDir { dir, run }
        }

        /// Build a tool command line using this directory's files
        ///
        /// # Arguments
        ///
        /// * `args` - The args after `tool`
        fn line(&self, args: &[&str]) -> Vec<String> {
            // the files every command reads
            let mut line = vec!["tool".to_owned()];
            line.extend(args.iter().map(|arg| (*arg).to_owned()));
            for (flag, name) in [
                ("--keys", "keys.yml"),
                ("--run-file", "run.json"),
                ("--tool-runs", "tool-runs.jsonl"),
            ] {
                line.push(flag.to_owned());
                line.push(self.dir.join(name).to_string_lossy().into_owned());
            }
            line
        }
    }

    impl Drop for RunDir {
        /// Remove the scratch directory
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Parse a printed line as the wrapper's JSON object
    ///
    /// # Arguments
    ///
    /// * `printed` - The printed line
    fn object(printed: &str) -> Value {
        // the line must be exactly one JSON object
        assert_eq!(printed.lines().count(), 1, "{printed}");
        let value: Value = serde_json::from_str(printed).unwrap();
        let mut keys: Vec<&String> = value.as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "error",
                "files",
                "reaction",
                "result",
                "retry_after_secs",
                "status"
            ],
            "{printed}"
        );
        value
    }

    /// `tool` commands parse without a worker's args, `--input` and `--input-file` are exclusive
    /// and one is required, bad input exits 2 with an error, and every command prints one object
    #[tokio::test]
    async fn tool_args_parse_and_refuse_bad_input() {
        // status parses with no cluster, group, name, or environment
        let id = Uuid::new_v4().to_string();
        let args = ToolArgs::try_parse_from(["tool", "status", &id]).unwrap();
        assert!(
            matches!(args.cmd, super::ToolCmd::Status(ref status) if status.reaction.to_string() == id)
        );
        // a worker command without the leading tool still needs its args
        assert!(crate::args::Args::try_parse_from(["thorium-agent", "status", &id]).is_err());
        // a mock API that must never be called
        let mock = test_server::MockThorium::start(|_| (500, "{}".to_owned())).await;
        let dir = RunDir::new(&mock.host, None);
        let input_file = dir.dir.join("input.json");
        std::fs::write(&input_file, "{}").unwrap();
        let input_file = input_file.to_string_lossy().into_owned();
        // both inputs, no input, bad JSON, a non-object, and a missing instance exit 2
        for args in [
            vec!["run", &id, "--input", "{}", "--input-file", &input_file],
            vec!["run", &id],
            vec!["run", &id, "--input", "{not json"],
            vec!["run", &id, "--input", "[1, 2]"],
            vec!["run", "--input", "{}"],
            vec!["run", "not-a-uuid", "--input", "{}"],
        ] {
            let (printed, code) = run_line(&dir.line(&args)).await;
            assert_eq!(code, 2, "{args:?}: {printed}");
            let value = object(&printed);
            assert!(value["error"].is_string(), "{args:?}: {printed}");
            assert!(!printed.contains(TOKEN));
        }
        assert!(mock.lines().is_empty(), "{:?}", mock.lines());
        // help is clap's text and exits 0
        let (printed, code) = run_line(&["tool".to_owned(), "--help".to_owned()]).await;
        assert_eq!(code, 0);
        assert!(
            printed.contains("run") && printed.contains("status"),
            "{printed}"
        );
    }

    /// `tool run` of a tool instance missing from the run's `tools.json` exits 2 naming it before
    /// any API call; a listed one is submitted with the run as its parent; without `run.json` the
    /// check is skipped; and the shared helper itself makes the request
    #[tokio::test]
    async fn tool_run_refuses_unlisted_tool_instances() {
        // a run whose tools.json lists only one tool instance
        let output = test_server::scratch();
        let listed = Scenario::container(&output);
        let mock = listed.serve().await;
        let unlisted = Uuid::new_v4();
        let mut tools = BTreeMap::new();
        tools.insert(listed.tool_instance, entry(listed.tool_instance, 0));
        let dir = RunDir::new(&mock.host, Some(tools));
        // the unlisted tool instance is refused before any API call
        let unlisted_id = unlisted.to_string();
        let (printed, code) = run_line(&dir.line(&["run", &unlisted_id, "--input", "{}"])).await;
        assert_eq!(code, 2, "{printed}");
        let value = object(&printed);
        assert!(
            value["error"].as_str().unwrap().contains(&unlisted_id),
            "{printed}"
        );
        assert_eq!(value["reaction"], Value::Null);
        assert!(mock.lines().is_empty(), "{:?}", mock.lines());
        // the listed tool instance is run with the workflow run as its parent
        let listed_id = listed.tool_instance.to_string();
        let (printed, code) = run_line(&dir.line(&["run", &listed_id, "--input", "{}"])).await;
        assert_eq!(code, 0, "{printed}");
        assert!(!printed.contains(TOKEN));
        let runs = mock.matching(&format!("/api/tools/instances/{listed_id}/runs"));
        assert_eq!(runs.len(), 1, "{:?}", mock.lines());
        let parent = dir.run.unwrap().to_string();
        assert!(
            runs[0].contains(&format!("\"parent\":\"{parent}\"")),
            "{}",
            runs[0]
        );
        // a run whose tools.json is missing runs nothing
        std::fs::rename(dir.dir.join("tools.json"), dir.dir.join("tools.json.bak")).unwrap();
        let calls = mock.lines().len();
        let (printed, code) = run_line(&dir.line(&["run", &listed_id, "--input", "{}"])).await;
        assert_eq!(code, 2, "{printed}");
        assert!(
            object(&printed)["error"]
                .as_str()
                .unwrap()
                .contains("tools.json")
        );
        assert_eq!(mock.lines().len(), calls, "{:?}", mock.lines());
        // without a run.json the unlisted tool instance is sent and the API decides
        let outside = RunDir::new(&mock.host, None);
        let (printed, _) = run_line(&outside.line(&["run", &unlisted_id, "--input", "{}"])).await;
        assert!(
            mock.lines().iter().any(|line| line.contains(&unlisted_id)),
            "{printed}"
        );
        // the shared helper called directly, as thorctl calls it, sends the unlisted instance
        let before = mock
            .matching(&format!("/api/tools/instances/{unlisted_id}/runs"))
            .len();
        let thorium = Thorium::build(&mock.host)
            .token(TOKEN)
            .build()
            .await
            .unwrap();
        let req = ToolRunRequest::new(RunInputs::default());
        let opts = ToolRunOptions::new(output.join("direct"));
        let _ = helper::run(
            &thorium,
            unlisted,
            &["corn".to_owned()],
            &req,
            &opts,
            &mut NoHooks,
        )
        .await;
        let after = mock
            .matching(&format!("/api/tools/instances/{unlisted_id}/runs"))
            .len();
        assert_eq!(after, before + 1);
        std::fs::remove_dir_all(&output).unwrap();
    }

    /// `tool run` fetches the spec, maps the input, polls until the run finishes, downloads its
    /// results into the tool's output path, truncates the result to the run's limit, records the
    /// run, and exits 0, or 1 when the run failed
    #[tokio::test]
    async fn tool_run_waits_and_collects() {
        // a container run that completes on its third read
        let output = test_server::scratch();
        let mut scenario = Scenario::container(&output);
        scenario.started_reads = 2;
        let mock = scenario.serve().await;
        let mut tools = BTreeMap::new();
        tools.insert(scenario.tool_instance, entry(scenario.tool_instance, 10));
        let dir = RunDir::new(&mock.host, Some(tools));
        let id = scenario.tool_instance.to_string();
        let (printed, code) =
            run_line(&dir.line(&["run", &id, "--input", r#"{"depth": 2}"#])).await;
        // the run completed and its file was downloaded into the tool's output path
        assert_eq!(code, 0, "{printed}");
        let value = object(&printed);
        assert_eq!(value["status"], "Completed");
        assert_eq!(value["reaction"], scenario.reaction.to_string());
        let file = output.join("decompiled.c");
        assert_eq!(value["files"], json!([file.to_string_lossy()]));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "int main() {}");
        // the result was cut to the run's 10 byte limit
        let text = json!({"functions": 412}).to_string();
        assert_eq!(value["result"], Value::String(text[..10].to_owned()));
        // the input's depth was mapped onto the job's kwarg
        let runs = mock.matching("/runs ");
        assert!(runs[0].contains(r#""--depth":["2"]"#), "{}", runs[0]);
        // the run was recorded and marked collected
        let recorded = tool_runs::read(&dir.dir.join("tool-runs.jsonl")).unwrap();
        assert!(recorded[&scenario.reaction].collected);
        assert!(!printed.contains(TOKEN));
        // a failed run exits 1 with no result
        let mut failing = Scenario::container(&output);
        failing.finished = ReactionStatus::Failed;
        let mock = failing.serve().await;
        let failing_dir = RunDir::new(&mock.host, None);
        let id = failing.tool_instance.to_string();
        let (printed, code) = run_line(&failing_dir.line(&["run", &id, "--input", "{}"])).await;
        assert_eq!(code, 1, "{printed}");
        let value = object(&printed);
        assert_eq!(value["status"], "Failed");
        assert_eq!(value["result"], Value::Null);
        std::fs::remove_dir_all(&output).unwrap();
    }

    /// A Thorium MCP tool is called through the MCP server and prints no reaction
    #[tokio::test]
    async fn tool_run_calls_thorium_mcp_tools() {
        // an MCP server answering one tool call, and the MCP tool's instance
        let tool_instance = Uuid::new_v4();
        let spec = ToolSpec::Agentic(agentic(AgenticToolBacking::ThoriumMcp {
            tool: "get_sample".to_owned(),
        }));
        let entity = test_server::tool_instance_entity(tool_instance, "get-sample", "1.0.0", spec);
        let entity_json = serde_json::to_string(&entity).unwrap();
        let entity_path = format!("GET /api/entities/{tool_instance} ");
        let mock = MockThorium::start(move |line| {
            if line.starts_with(&entity_path) {
                (200, entity_json.clone())
            } else if line.starts_with("POST /api/mcp ") {
                (
                    200,
                    json!({"jsonrpc": "2.0", "id": 1, "result": {
                        "content": [{"type": "text", "text": "{\"sha256\":\"abc\"}"}],
                        "structuredContent": {"sha256": "abc"}, "isError": false,
                        "protocolVersion": "2025-03-26", "capabilities": {},
                        "serverInfo": {"name": "thorium", "version": "1"}}})
                    .to_string(),
                )
            } else {
                (404, "{}".to_owned())
            }
        })
        .await;
        let dir = RunDir::new(&mock.host, None);
        let id = tool_instance.to_string();
        let (printed, code) =
            run_line(&dir.line(&["run", &id, "--input", r#"{"sha256": "abc"}"#])).await;
        assert_eq!(code, 0, "{printed}");
        let value = object(&printed);
        assert_eq!(value["reaction"], Value::Null);
        assert_eq!(value["result"], json!({"sha256": "abc"}));
        // the token went to the MCP server as a bearer token and was never printed
        let calls = mock.matching("POST /api/mcp ");
        assert!(
            calls
                .iter()
                .all(|call| call.contains(&format!("Bearer {TOKEN}"))),
            "{calls:?}"
        );
        assert!(!printed.contains(TOKEN));
        assert_eq!(mock.matching("/runs"), Vec::<String>::new());
    }

    /// `--timeout` can only shorten the tool's timeout, reaching it exits 3 while the run goes on,
    /// `--no-wait` returns the reaction at once, and `tool status` and `tool results` resume it
    #[tokio::test]
    async fn timeouts_no_wait_and_resuming() {
        // a run that stays started
        let output = test_server::scratch();
        let mut scenario = Scenario::container(&output);
        scenario.started_reads = usize::MAX;
        let mock = scenario.serve().await;
        let mut tools = BTreeMap::new();
        tools.insert(scenario.tool_instance, entry(scenario.tool_instance, 0));
        let dir = RunDir::new(&mock.host, Some(tools));
        let id = scenario.tool_instance.to_string();
        // a longer timeout than the tool's is refused before any API call
        let (printed, code) =
            run_line(&dir.line(&["run", &id, "--input", "{}", "--timeout", "601"])).await;
        assert_eq!(code, 2, "{printed}");
        assert!(object(&printed)["error"].as_str().unwrap().contains("600"));
        assert!(mock.lines().is_empty(), "{:?}", mock.lines());
        // reaching the timeout prints the status and reaction and exits 3
        let (printed, code) =
            run_line(&dir.line(&["run", &id, "--input", "{}", "--timeout", "1"])).await;
        assert_eq!(code, 3, "{printed}");
        let value = object(&printed);
        assert_eq!(value["status"], "Started");
        assert_eq!(value["reaction"], scenario.reaction.to_string());
        // not waiting returns the reaction right away, and the tool's own timeout is allowed
        let (printed, code) =
            run_line(&dir.line(&["run", &id, "--input", "{}", "--no-wait", "--timeout", "600"]))
                .await;
        assert_eq!(code, 0, "{printed}");
        assert_eq!(object(&printed)["reaction"], scenario.reaction.to_string());
        // status resumes the run and hints when to poll again
        let reaction = scenario.reaction.to_string();
        let (printed, code) = run_line(&dir.line(&["status", &reaction])).await;
        assert_eq!(code, 0, "{printed}");
        let value = object(&printed);
        assert_eq!(value["status"], "Started");
        assert!(value["retry_after_secs"].is_u64(), "{printed}");
        // results of an unfinished run say so
        let (printed, code) = run_line(&dir.line(&["results", &reaction])).await;
        assert_eq!(code, 3, "{printed}");
        assert!(object(&printed)["error"].is_string());
        // a reaction outside the token's groups is the API's 404
        let missing = Uuid::new_v4().to_string();
        for cmd in ["status", "results"] {
            let (printed, code) = run_line(&dir.line(&[cmd, &missing])).await;
            assert_eq!(code, 1, "{printed}");
            let error = object(&printed)["error"].as_str().unwrap().to_owned();
            assert!(error.contains("404") && error.contains(&missing), "{error}");
        }
        // results resume a finished run into the path it was started with
        let mut done = Scenario::container(&output);
        done.reaction = scenario.reaction;
        done.tool_instance = scenario.tool_instance;
        let done_mock = done.serve().await;
        std::fs::write(
            dir.dir.join("keys.yml"),
            format!("api: {}\ntoken: {TOKEN}\n", done_mock.host),
        )
        .unwrap();
        let (printed, code) = run_line(&dir.line(&["results", &reaction])).await;
        assert_eq!(code, 0, "{printed}");
        let value = object(&printed);
        assert_eq!(
            value["files"],
            json!([output.join("decompiled.c").to_string_lossy()])
        );
        let recorded = tool_runs::read(&dir.dir.join("tool-runs.jsonl")).unwrap();
        assert!(recorded[&scenario.reaction].collected);
        std::fs::remove_dir_all(&output).unwrap();
    }

    /// The wrapper prints the same bytes as every other client of the shared helper for the
    /// same run
    #[tokio::test]
    async fn wrapper_output_matches_the_shared_golden() {
        // the fixed run behind the golden line, with no result files
        let output = test_server::scratch();
        let mut scenario = Scenario::container(&output);
        scenario.reaction = Uuid::parse_str(GOLDEN_REACTION).unwrap();
        scenario.files = Vec::new();
        let mock = scenario.serve().await;
        let dir = RunDir::new(&mock.host, None);
        let id = scenario.tool_instance.to_string();
        let (printed, code) = run_line(&dir.line(&["run", &id, "--input", "{}"])).await;
        assert_eq!(code, 0, "{printed}");
        assert_eq!(printed, GOLDEN);
        std::fs::remove_dir_all(&output).unwrap();
    }
}
