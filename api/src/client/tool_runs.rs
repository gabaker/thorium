//! The shared tool-run helper
//!
//! The thorium-agent's CLI wrapper (`thorium-agent tool`) and `thorctl tools run` both start tool
//! runs, follow them, and collect their results through this module, so both print the same JSON
//! object for the same run.

use http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::time::Instant;
use uuid::Uuid;

use super::{Error, ResultsClient, Thorium};
use crate::models::{
    AgenticToolBacking, DEFAULT_TOOL_RESULTS_DIR, Entity, EntityMetadata, MappedToolInput, Output,
    Reaction, ReactionStatus, ResultGetParams, TOOL_RUN_POLL_MAX_SECS, TOOL_RUN_POLL_START_SECS,
    ToolInstanceEntity, ToolRunRequest, ToolSpec, tool_run_retry_after,
};

/// The MCP protocol revision the helper negotiates with Thorium's MCP server
const MCP_PROTOCOL_VERSION: &str = "2025-03-26";

/// The header Thorium's MCP server names a session with
const MCP_SESSION_HEADER: &str = "mcp-session-id";

/// The JSON object every tool-run command prints
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ToolRunOutput {
    /// The tool run's status (`null` when no run was found or started)
    pub status: Option<ReactionStatus>,
    /// The tool run's reaction id (`null` for a call of a Thorium MCP tool)
    pub reaction: Option<Uuid>,
    /// The tool run's result, truncated to the effective result size (`null` until completed)
    pub result: Option<Value>,
    /// The paths of the downloaded result files
    pub files: Vec<String>,
    /// Why the call failed or timed out
    pub error: Option<String>,
    /// How many seconds to wait before polling this run again (`null` once it's finished)
    pub retry_after_secs: Option<u64>,
}

impl ToolRunOutput {
    /// Build the output of a call that failed before any run was found or started
    ///
    /// # Arguments
    ///
    /// * `error` - Why the call failed
    #[must_use]
    pub fn refused<T: Into<String>>(error: T) -> Self {
        // only the error is set
        ToolRunOutput {
            error: Some(error.into()),
            ..ToolRunOutput::default()
        }
    }

    /// Render this output as one line of JSON
    #[must_use]
    pub fn to_line(&self) -> String {
        // every field is plain JSON, so rendering only fails if serde itself does
        serde_json::to_string(self).unwrap_or_else(|_| {
            r#"{"status":null,"reaction":null,"result":null,"files":[],"error":"unable to render the tool run","retry_after_secs":null}"#
                .to_owned()
        })
    }
}

/// How a tool-run command ended, which picks its exit code
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolRunExit {
    /// The run completed, or the command succeeded without waiting (exit code 0)
    Completed,
    /// The run failed or the API refused the call (exit code 1)
    Failed,
    /// The command's input or usage was bad (exit code 2)
    Usage,
    /// The run hadn't finished when the command stopped waiting (exit code 3)
    TimedOut,
}

impl ToolRunExit {
    /// Get the process exit code for this ending
    #[must_use]
    pub fn code(self) -> i32 {
        // each ending has its own code
        match self {
            ToolRunExit::Completed => 0,
            ToolRunExit::Failed => 1,
            ToolRunExit::Usage => 2,
            ToolRunExit::TimedOut => 3,
        }
    }
}

/// What a tool-run command prints and how it exits
#[derive(Debug, Clone, PartialEq)]
pub struct ToolRunReport {
    /// The JSON object to print
    pub output: ToolRunOutput,
    /// How the command ended
    pub exit: ToolRunExit,
}

impl ToolRunReport {
    /// Build the report of a command whose input or usage was bad
    ///
    /// # Arguments
    ///
    /// * `error` - What was wrong with the input or usage
    #[must_use]
    pub fn usage<T: Into<String>>(error: T) -> Self {
        ToolRunReport {
            output: ToolRunOutput::refused(error),
            exit: ToolRunExit::Usage,
        }
    }

    /// Build the report of a call the API refused or that failed before a run existed
    ///
    /// # Arguments
    ///
    /// * `error` - Why the call failed
    #[must_use]
    pub fn failed<T: Into<String>>(error: T) -> Self {
        ToolRunReport {
            output: ToolRunOutput::refused(error),
            exit: ToolRunExit::Failed,
        }
    }

    /// Build the report of a call that failed after its run was found
    ///
    /// # Arguments
    ///
    /// * `reaction` - The run's reaction
    /// * `error` - Why the call failed
    fn failed_run<T: Into<String>>(reaction: &Reaction, error: T) -> Self {
        // keep the run's status and id with the error
        ToolRunReport {
            output: ToolRunOutput {
                status: Some(reaction.status.clone()),
                reaction: Some(reaction.id),
                error: Some(error.into()),
                ..ToolRunOutput::default()
            },
            exit: ToolRunExit::Failed,
        }
    }
}

/// How a tool instance is run for a tool call
#[derive(Debug, Clone, PartialEq)]
pub enum ToolRunKind {
    /// Start a tool run whose job gets these args and runs on these files
    Job(MappedToolInput),
    /// Call a tool of Thorium's MCP server
    Mcp {
        /// The name of the MCP tool
        tool: String,
        /// The tool call's input
        input: serde_json::Map<String, Value>,
    },
}

/// What a tool instance's spec says about running it
#[derive(Debug, Clone, PartialEq)]
pub struct ToolRunPlan {
    /// How the tool instance is run
    pub kind: ToolRunKind,
    /// The tool's own timeout in seconds, if it has one
    pub timeout: Option<u64>,
    /// The directory the tool's results are delivered to unless another is given
    pub output: PathBuf,
    /// The tool's own result size limit (0 = unlimited)
    pub max_result_bytes: u64,
}

/// Get the name of the tool a tool instance is a version of
///
/// # Arguments
///
/// * `entity` - The tool instance
/// * `instance` - The tool instance's metadata
fn tool_name(entity: &Entity, instance: &ToolInstanceEntity) -> String {
    // instances are named after their tool and version
    let suffix = format!("@{}", instance.version);
    entity
        .name
        .strip_suffix(&suffix)
        .unwrap_or(&entity.name)
        .to_owned()
}

/// Plan how to run a tool instance for a tool call input
///
/// A container tool maps the input onto its job, a Thorium MCP tool is called through Thorium's
/// MCP server, and any other tool instance takes no input.
///
/// # Arguments
///
/// * `entity` - The tool instance to run
/// * `input` - The tool call's input object
pub fn plan(
    entity: &Entity,
    input: &serde_json::Map<String, Value>,
) -> Result<ToolRunPlan, String> {
    // only tool instances can be run
    let EntityMetadata::ToolInstance(instance) = &entity.metadata else {
        return Err(format!(
            "{} is a {}, not a tool instance",
            entity.id, entity.kind
        ));
    };
    match &instance.spec {
        ToolSpec::Agentic(tool) => match &tool.backing {
            // a container tool maps its input onto its job and delivers to its output path
            AgenticToolBacking::Container(container) => Ok(ToolRunPlan {
                kind: ToolRunKind::Job(container.inputs.map_input(input)?),
                timeout: tool.timeout,
                output: PathBuf::from(container.outputs.resolved_path(&tool.function_name)),
                max_result_bytes: container.outputs.max_result_bytes,
            }),
            // a Thorium MCP tool is called with the input as is
            AgenticToolBacking::ThoriumMcp { tool: mcp } => Ok(ToolRunPlan {
                kind: ToolRunKind::Mcp {
                    tool: mcp.clone(),
                    input: input.clone(),
                },
                timeout: tool.timeout,
                output: PathBuf::from(format!("{DEFAULT_TOOL_RESULTS_DIR}/{}", tool.function_name)),
                max_result_bytes: 0,
            }),
            // a native tool only runs inside the harness that provides it
            AgenticToolBacking::HarnessNative { .. } => Err(format!(
                "tool instance {} is built into a harness, so only that harness can run it",
                entity.id
            )),
        },
        // any other tool instance has nowhere to put an input, and the API decides if it runs
        spec if input.is_empty() => Ok(ToolRunPlan {
            kind: ToolRunKind::Job(MappedToolInput::default()),
            timeout: match spec {
                ToolSpec::Image(workload) => workload.execution.timeout,
                _ => None,
            },
            output: PathBuf::from(format!(
                "{DEFAULT_TOOL_RESULTS_DIR}/{}",
                tool_name(entity, instance)
            )),
            max_result_bytes: 0,
        }),
        _ => Err(format!(
            "tool instance {} isn't a container tool, so it takes no input",
            entity.id
        )),
    }
}

/// How a tool-run command follows and collects a run
#[derive(Debug, Clone)]
pub struct ToolRunOptions {
    /// The directory to download result files into
    pub output: PathBuf,
    /// The longest to wait for the run (unset = until it finishes)
    pub timeout: Option<Duration>,
    /// Whether to wait for the run to finish
    pub wait: bool,
    /// The most bytes of the result to return (0 = unlimited)
    pub max_result_bytes: u64,
    /// The first wait between polls
    pub poll_start: Duration,
    /// The longest wait between polls
    pub poll_max: Duration,
}

impl ToolRunOptions {
    /// Build options that wait for a run with the standard polling backoff
    ///
    /// # Arguments
    ///
    /// * `output` - The directory to download result files into
    #[must_use]
    pub fn new<P: Into<PathBuf>>(output: P) -> Self {
        ToolRunOptions {
            output: output.into(),
            timeout: None,
            wait: true,
            max_result_bytes: 0,
            poll_start: Duration::from_secs(TOOL_RUN_POLL_START_SECS),
            poll_max: Duration::from_secs(TOOL_RUN_POLL_MAX_SECS),
        }
    }
}

/// Steps a caller takes when a run starts and when its results are collected
pub trait ToolRunHooks: Send {
    /// Called once the run's reaction exists
    ///
    /// # Arguments
    ///
    /// * `reaction` - The run's reaction
    fn started(&mut self, reaction: &Reaction) -> Result<(), Error> {
        let _ = reaction;
        Ok(())
    }

    /// Called once the run's results are downloaded
    ///
    /// # Arguments
    ///
    /// * `reaction` - The run's reaction
    fn collected(&mut self, reaction: &Reaction) -> Result<(), Error> {
        let _ = reaction;
        Ok(())
    }
}

/// Hooks that do nothing
pub struct NoHooks;

impl ToolRunHooks for NoHooks {}

/// Whether a reaction status is final
///
/// # Arguments
///
/// * `status` - The status to check
fn finished(status: &ReactionStatus) -> bool {
    matches!(status, ReactionStatus::Completed | ReactionStatus::Failed)
}

/// Whether an error means a group doesn't hold a reaction the caller can read
///
/// # Arguments
///
/// * `error` - The error from reading a reaction
fn is_missing(error: &Error) -> bool {
    // a missing reaction and a group the caller can't read both mean "not here"
    matches!(
        error.status(),
        Some(StatusCode::NOT_FOUND | StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
    )
}

/// Read a reaction from the first of some groups that holds it
///
/// # Arguments
///
/// * `thorium` - The client to read with
/// * `id` - The reaction's id
/// * `groups` - The groups to look in
async fn find_in(
    thorium: &Thorium,
    id: Uuid,
    groups: &[String],
) -> Result<Option<Reaction>, Error> {
    // read the reaction from every group at once
    let reads =
        futures::future::join_all(groups.iter().map(|group| thorium.reactions.get(group, id)))
            .await;
    // keep the reaction a group holds, passing on any error that isn't a miss
    let mut failure = None;
    for read in reads {
        match read {
            Ok(reaction) => return Ok(Some(reaction)),
            Err(error) if is_missing(&error) => (),
            Err(error) => failure = failure.or(Some(error)),
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(None),
    }
}

/// Find a tool run's reaction in some groups, then in the rest of the caller's groups
///
/// # Arguments
///
/// * `thorium` - The client to read with
/// * `id` - The reaction's id
/// * `groups` - The groups to look in first
async fn find_reaction(thorium: &Thorium, id: Uuid, groups: &[String]) -> Result<Reaction, Error> {
    // look in the given groups first
    if let Some(reaction) = find_in(thorium, id, groups).await? {
        return Ok(reaction);
    }
    // then in every other group the caller is in
    let rest: Vec<String> = thorium
        .users
        .info()
        .await?
        .groups
        .into_iter()
        .filter(|group| !groups.contains(group))
        .collect();
    if let Some(reaction) = find_in(thorium, id, &rest).await? {
        return Ok(reaction);
    }
    // no group holds it, so it doesn't exist as far as the caller can tell
    Err(Error::new(format!(
        "{} tool run {id} doesn't exist or isn't in any of your groups",
        StatusCode::NOT_FOUND
    )))
}

/// Get the seconds since an instant, rounded down
///
/// # Arguments
///
/// * `since` - The instant to count from
fn elapsed_secs(since: Instant) -> u64 {
    since.elapsed().as_secs()
}

/// Describe a run that hasn't finished, with a hint of when to poll it again
///
/// # Arguments
///
/// * `reaction` - The run's reaction
/// * `elapsed` - The seconds since the run started
fn unfinished(reaction: &Reaction, elapsed: u64) -> ToolRunOutput {
    // the status, the id, and the polling hint
    ToolRunOutput {
        status: Some(reaction.status.clone()),
        reaction: Some(reaction.id),
        retry_after_secs: tool_run_retry_after(&reaction.status, elapsed),
        ..ToolRunOutput::default()
    }
}

/// The end of following a run
enum Waited {
    /// The run finished
    Finished(Reaction),
    /// The command stopped waiting first
    TimedOut(Reaction),
}

/// Poll a run with a growing backoff until it finishes or the timeout is reached
///
/// # Arguments
///
/// * `thorium` - The client to read the run with
/// * `reaction` - The run's reaction as last read
/// * `started` - When the run was started
/// * `opts` - How to follow the run
async fn wait(
    thorium: &Thorium,
    mut reaction: Reaction,
    started: Instant,
    opts: &ToolRunOptions,
) -> Result<Waited, Error> {
    // stop waiting once the timeout passes
    let deadline = opts.timeout.map(|timeout| started + timeout);
    let mut pause = opts.poll_start;
    loop {
        // a finished run needs no more polls
        if finished(&reaction.status) {
            return Ok(Waited::Finished(reaction));
        }
        // sleep until the next poll, or the deadline if it's sooner
        let now = Instant::now();
        let sleep = match deadline {
            Some(deadline) if now >= deadline => return Ok(Waited::TimedOut(reaction)),
            Some(deadline) => pause.min(deadline - now),
            None => pause,
        };
        tokio::time::sleep(sleep).await;
        // double the wait up to its cap
        pause = (pause * 2).min(opts.poll_max);
        // read the run again, naming it in any error
        let id = reaction.id;
        reaction = thorium
            .reactions
            .get(&reaction.group, id)
            .await
            .map_err(|error| Error::new(format!("Failed to read tool run {id}: {error}")))?;
    }
}

/// Start a tool run, wait for it unless told not to, and download its results
///
/// # Arguments
///
/// * `thorium` - The client to run the tool with
/// * `tool_instance` - The tool instance to run
/// * `groups` - The groups to look for the run's reaction in once it exists
/// * `req` - The tool run request
/// * `opts` - How to follow and collect the run
/// * `hooks` - Steps to take when the run starts and when it's collected
pub async fn run(
    thorium: &Thorium,
    tool_instance: Uuid,
    groups: &[String],
    req: &ToolRunRequest,
    opts: &ToolRunOptions,
    hooks: &mut dyn ToolRunHooks,
) -> ToolRunReport {
    // start the run
    let created = match thorium.reactions.run_tool(tool_instance, req).await {
        Ok(created) => created,
        Err(error) => return ToolRunReport::failed(error.to_string()),
    };
    let started = Instant::now();
    // find the run's reaction so it can be followed
    let reaction = match find_reaction(thorium, created.id, groups).await {
        Ok(reaction) => reaction,
        Err(error) => {
            return ToolRunReport {
                output: ToolRunOutput {
                    reaction: Some(created.id),
                    error: Some(error.to_string()),
                    ..ToolRunOutput::default()
                },
                exit: ToolRunExit::Failed,
            };
        }
    };
    // let the caller record the run
    if let Err(error) = hooks.started(&reaction) {
        return ToolRunReport::failed_run(&reaction, error.to_string());
    }
    // without waiting, report the run as it is now
    if !opts.wait {
        return ToolRunReport {
            output: unfinished(&reaction, elapsed_secs(started)),
            exit: ToolRunExit::Completed,
        };
    }
    // follow the run until it finishes or the timeout is reached
    match wait(thorium, reaction, started, opts).await {
        Ok(Waited::Finished(reaction)) => finish(thorium, &reaction, opts, hooks).await,
        Ok(Waited::TimedOut(reaction)) => {
            // the run keeps going and can be picked up later
            let mut output = unfinished(&reaction, elapsed_secs(started));
            output.error = Some(format!(
                "tool run {} timed out after {}s; it's still running",
                reaction.id,
                opts.timeout.unwrap_or_default().as_secs()
            ));
            ToolRunReport {
                output,
                exit: ToolRunExit::TimedOut,
            }
        }
        Err(error) => ToolRunReport {
            output: ToolRunOutput {
                reaction: Some(created.id),
                error: Some(error.to_string()),
                ..ToolRunOutput::default()
            },
            exit: ToolRunExit::Failed,
        },
    }
}

/// Report a finished run, downloading its results when it completed
///
/// # Arguments
///
/// * `thorium` - The client to download results with
/// * `reaction` - The finished run's reaction
/// * `opts` - Where to download the results and how much of the result to return
/// * `hooks` - Steps to take once the run's results are collected
async fn finish(
    thorium: &Thorium,
    reaction: &Reaction,
    opts: &ToolRunOptions,
    hooks: &mut dyn ToolRunHooks,
) -> ToolRunReport {
    // a failed run has no results to download
    if reaction.status == ReactionStatus::Failed {
        return ToolRunReport::failed_run(reaction, format!("tool run {} failed", reaction.id));
    }
    // download the completed run's results
    match collect(thorium, reaction, opts).await {
        Ok((result, files)) => {
            // let the caller mark the run collected
            if let Err(error) = hooks.collected(reaction) {
                return ToolRunReport::failed_run(reaction, error.to_string());
            }
            ToolRunReport {
                output: ToolRunOutput {
                    status: Some(ReactionStatus::Completed),
                    reaction: Some(reaction.id),
                    result: Some(truncate(result, opts.max_result_bytes)),
                    files,
                    error: None,
                    retry_after_secs: None,
                },
                exit: ToolRunExit::Completed,
            }
        }
        Err(error) => ToolRunReport::failed_run(reaction, error.to_string()),
    }
}

/// Cut a result down to a number of bytes of its text (0 = unlimited)
///
/// A result that fits is returned as is; a longer one becomes the first bytes of its text,
/// ending on a character boundary.
///
/// # Arguments
///
/// * `value` - The result
/// * `max` - The most bytes to return
fn truncate(value: Value, max: u64) -> Value {
    // zero means unlimited
    if max == 0 {
        return value;
    }
    let max = usize::try_from(max).unwrap_or(usize::MAX);
    // measure the result's text
    let text = match &value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    if text.len() <= max {
        return value;
    }
    // keep the longest prefix that ends on a character boundary
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Value::String(text[..end].to_owned())
}

/// Join a result file name the API returned onto a directory, refusing names that leave it
///
/// # Arguments
///
/// * `dir` - The directory to write into
/// * `name` - The result file's name
fn safe_join(dir: &Path, name: &str) -> Result<PathBuf, Error> {
    // every part of the name must be a plain file or directory name
    let path = Path::new(name);
    let plain = path.components().count() > 0
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        && !name.contains('\\');
    if !plain {
        return Err(Error::new(format!(
            "result file {name} would be written outside of {}",
            dir.display()
        )));
    }
    Ok(dir.join(path))
}

/// Turn an input's key into a directory name
///
/// # Arguments
///
/// * `key` - A file's SHA256 or a repo's url
fn input_dir(key: &str) -> String {
    // keep letters, digits, dots, and dashes
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// An input a tool run ran on
enum Input<'a> {
    /// A file, by SHA256
    Sample(&'a str),
    /// A repo, by url
    Repo(&'a str),
}

impl Input<'_> {
    /// Get the key this input's results are stored under
    fn key(&self) -> &str {
        match self {
            Input::Sample(key) | Input::Repo(key) => key,
        }
    }
}

/// Download a completed run's results: its newest result on each input and each result file
///
/// One input's result is returned as is and several inputs' results as an object keyed by
/// input; with several inputs each input's files go in a directory named after it.
///
/// # Arguments
///
/// * `thorium` - The client to download with
/// * `reaction` - The completed run's reaction
/// * `opts` - Where to download the results
async fn collect(
    thorium: &Thorium,
    reaction: &Reaction,
    opts: &ToolRunOptions,
) -> Result<(Value, Vec<String>), Error> {
    // results are stored on the run's inputs under its key
    let Some(tool) = reaction.result_key() else {
        return Err(Error::new(format!(
            "reaction {} is a pipeline reaction, not a tool run",
            reaction.id
        )));
    };
    let params = ResultGetParams::default().tool(tool);
    let inputs: Vec<Input> = reaction
        .samples
        .iter()
        .map(|sample| Input::Sample(sample))
        .chain(reaction.repos.iter().map(|repo| Input::Repo(&repo.url)))
        .collect();
    let several = inputs.len() > 1;
    let mut results = serde_json::Map::new();
    let mut files = Vec::new();
    for input in &inputs {
        // read this input's results under the run's key
        let found = match input {
            Input::Sample(sha256) => thorium.files.get_results(sha256, &params).await?,
            Input::Repo(url) => thorium.repos.get_results(url, &params).await?,
        };
        let newest: Option<&Output> = found.results.get(tool).and_then(|outputs| outputs.first());
        // download the newest result's files
        if let Some(output) = newest {
            let dir = if several {
                opts.output.join(input_dir(input.key()))
            } else {
                opts.output.clone()
            };
            for name in &output.files {
                // check where the file lands before downloading it
                let target = safe_join(&dir, name)?;
                let attachment = match input {
                    Input::Sample(sha256) => {
                        thorium
                            .files
                            .download_result_file(sha256, tool, &output.id, name)
                            .await?
                    }
                    Input::Repo(url) => {
                        thorium
                            .repos
                            .download_result_file(url, tool, &output.id, name)
                            .await?
                    }
                };
                if let Some(parent) = target.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                let mut file = tokio::fs::File::create(&target).await?;
                file.write_all(&attachment.data).await?;
                files.push(target.to_string_lossy().into_owned());
            }
        }
        let value = newest.map_or(Value::Null, |output| output.result.clone());
        results.insert(input.key().to_owned(), value);
    }
    // one input's result is returned as is
    let result = if several {
        Value::Object(results)
    } else {
        results
            .into_iter()
            .next()
            .map_or(Value::Null, |(_, value)| value)
    };
    Ok((result, files))
}

/// Get the polling hint for an unfinished run from its first status update
///
/// # Arguments
///
/// * `thorium` - The client to read the run's status updates with
/// * `reaction` - The run's reaction
async fn hint(thorium: &Thorium, reaction: &Reaction) -> Result<Option<u64>, Error> {
    // a finished run is never polled again
    if finished(&reaction.status) {
        return Ok(None);
    }
    // the run started when its reaction was created, its first status update
    let logs = thorium
        .reactions
        .status_logs(&reaction.group, &reaction.id)
        .await?;
    let now = chrono::Utc::now();
    let start = logs.iter().map(|log| log.timestamp).min().unwrap_or(now);
    let elapsed = u64::try_from((now - start).num_seconds()).unwrap_or(0);
    Ok(tool_run_retry_after(&reaction.status, elapsed))
}

/// Get a tool run's status without waiting
///
/// # Arguments
///
/// * `thorium` - The client to read the run with
/// * `reaction` - The run's reaction id
/// * `group` - The group the run is in (searched for in the caller's groups when unset)
pub async fn status(thorium: &Thorium, reaction: Uuid, group: Option<&str>) -> ToolRunReport {
    // find the run's reaction
    let groups: Vec<String> = group.map(str::to_owned).into_iter().collect();
    let reaction = match find_reaction(thorium, reaction, &groups).await {
        Ok(reaction) => reaction,
        Err(error) => return ToolRunReport::failed(error.to_string()),
    };
    // hint when to poll again while it runs
    let retry_after_secs = match hint(thorium, &reaction).await {
        Ok(hint) => hint,
        Err(error) => return ToolRunReport::failed_run(&reaction, error.to_string()),
    };
    // a failed run exits 1
    let (exit, error) = if reaction.status == ReactionStatus::Failed {
        (
            ToolRunExit::Failed,
            Some(format!("tool run {} failed", reaction.id)),
        )
    } else {
        (ToolRunExit::Completed, None)
    };
    ToolRunReport {
        output: ToolRunOutput {
            status: Some(reaction.status.clone()),
            reaction: Some(reaction.id),
            error,
            retry_after_secs,
            ..ToolRunOutput::default()
        },
        exit,
    }
}

/// Download a finished tool run's results
///
/// # Arguments
///
/// * `thorium` - The client to read the run with
/// * `reaction` - The run's reaction id
/// * `group` - The group the run is in (searched for in the caller's groups when unset)
/// * `opts` - Where to download the results and how much of the result to return
/// * `hooks` - Steps to take once the run's results are collected
pub async fn results(
    thorium: &Thorium,
    reaction: Uuid,
    group: Option<&str>,
    opts: &ToolRunOptions,
    hooks: &mut dyn ToolRunHooks,
) -> ToolRunReport {
    // find the run's reaction
    let groups: Vec<String> = group.map(str::to_owned).into_iter().collect();
    let reaction = match find_reaction(thorium, reaction, &groups).await {
        Ok(reaction) => reaction,
        Err(error) => return ToolRunReport::failed(error.to_string()),
    };
    // a finished run is reported with its results
    if finished(&reaction.status) {
        return finish(thorium, &reaction, opts, hooks).await;
    }
    // an unfinished run has no results yet
    let retry_after_secs = match hint(thorium, &reaction).await {
        Ok(hint) => hint,
        Err(error) => return ToolRunReport::failed_run(&reaction, error.to_string()),
    };
    ToolRunReport {
        output: ToolRunOutput {
            status: Some(reaction.status.clone()),
            reaction: Some(reaction.id),
            error: Some(format!(
                "tool run {} hasn't finished yet ({})",
                reaction.id, reaction.status
            )),
            retry_after_secs,
            ..ToolRunOutput::default()
        },
        exit: ToolRunExit::TimedOut,
    }
}

/// Why a call of a Thorium MCP tool failed
enum McpFailure {
    /// The tool ran and returned an error
    Tool(String),
    /// The call never got a tool result
    Call(String),
}

/// Send one JSON-RPC message to Thorium's MCP server
///
/// Returns the session the server named, if any, and the JSON-RPC response, which is `null` for
/// a notification. Responses may come as JSON or as server-sent events.
///
/// # Arguments
///
/// * `client` - The HTTP client to send with
/// * `url` - The MCP server's url
/// * `token` - The token to authenticate with
/// * `session` - The session to send in, once the server named one
/// * `message` - The JSON-RPC message
async fn mcp_send(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    session: Option<&str>,
    message: &Value,
) -> Result<(Option<String>, Value), McpFailure> {
    // send the message with the token as a bearer token
    let mut req = client
        .post(url)
        .header("authorization", format!("Bearer {token}"))
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
        .json(message);
    if let Some(session) = session {
        req = req.header(MCP_SESSION_HEADER, session);
    }
    let resp = req
        .send()
        .await
        .map_err(|error| McpFailure::Call(format!("unable to reach the MCP server: {error}")))?;
    // a refused message ends the call
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(McpFailure::Call(format!(
            "the MCP server returned {status}: {body}"
        )));
    }
    // keep the session the server named
    let named = resp
        .headers()
        .get(MCP_SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let streamed = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|kind| kind.starts_with("text/event-stream"));
    let body = resp.text().await.map_err(|error| {
        McpFailure::Call(format!("unable to read the MCP server's reply: {error}"))
    })?;
    // read the response out of the body
    let response = if streamed {
        // the response is the event data that carries a JSON-RPC id
        body.lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
            .find(|value| value.get("id").is_some())
            .unwrap_or(Value::Null)
    } else if body.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&body).map_err(|error| {
            McpFailure::Call(format!("the MCP server's reply isn't JSON: {error}"))
        })?
    };
    Ok((named.or_else(|| session.map(str::to_owned)), response))
}

/// Call a Thorium MCP tool and get its result
///
/// # Arguments
///
/// * `url` - The MCP server's url
/// * `token` - The token to authenticate with
/// * `tool` - The name of the MCP tool
/// * `input` - The tool call's input
/// * `timeout` - The longest to wait for the call
async fn mcp_call(
    url: &str,
    token: &str,
    tool: &str,
    input: &serde_json::Map<String, Value>,
    timeout: Option<Duration>,
) -> Result<Value, McpFailure> {
    // a client that gives up at the timeout
    let mut builder = reqwest::Client::builder();
    if let Some(timeout) = timeout {
        builder = builder.timeout(timeout);
    }
    let client = builder
        .build()
        .map_err(|error| McpFailure::Call(format!("unable to build an HTTP client: {error}")))?;
    // start a session
    let init = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "thorium-tool-run", "version": env!("CARGO_PKG_VERSION")},
        },
    });
    let (session, _) = mcp_send(&client, url, token, None, &init).await?;
    let initialized = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    mcp_send(&client, url, token, session.as_deref(), &initialized).await?;
    // call the tool
    let call = json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": tool, "arguments": input},
    });
    let (session, response) = mcp_send(&client, url, token, session.as_deref(), &call).await?;
    // end the session; the result doesn't depend on it
    if let Some(session) = &session {
        let _ = client
            .delete(url)
            .header("authorization", format!("Bearer {token}"))
            .header(MCP_SESSION_HEADER, session)
            .send()
            .await;
    }
    // a protocol error never reached the tool
    if let Some(error) = response.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .map_or_else(|| error.to_string(), str::to_owned);
        return Err(McpFailure::Call(format!(
            "the MCP tool {tool} was refused: {message}"
        )));
    }
    let Some(result) = response.get("result") else {
        return Err(McpFailure::Call(format!(
            "the MCP server sent no result for {tool}"
        )));
    };
    // join the text content, which carries a tool error or an unstructured result
    let text: String =
        result
            .get("content")
            .and_then(Value::as_array)
            .map_or_else(String::new, |content| {
                content
                    .iter()
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .collect::<Vec<&str>>()
                    .join("\n")
            });
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(McpFailure::Tool(format!(
            "the MCP tool {tool} failed: {text}"
        )));
    }
    // prefer the structured result, then text that is JSON, then the text itself
    if let Some(structured) = result.get("structuredContent") {
        return Ok(structured.clone());
    }
    Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

/// Call a tool of Thorium's MCP server and report its result
///
/// # Arguments
///
/// * `mcp_url` - The url of Thorium's MCP server
/// * `token` - The token to authenticate with
/// * `tool` - The name of the MCP tool
/// * `input` - The tool call's input
/// * `opts` - How long to wait and how much of the result to return
pub async fn call_mcp(
    mcp_url: &str,
    token: &str,
    tool: &str,
    input: &serde_json::Map<String, Value>,
    opts: &ToolRunOptions,
) -> ToolRunReport {
    // a call of an MCP tool has no reaction
    match mcp_call(mcp_url, token, tool, input, opts.timeout).await {
        Ok(result) => ToolRunReport {
            output: ToolRunOutput {
                status: Some(ReactionStatus::Completed),
                result: Some(truncate(result, opts.max_result_bytes)),
                ..ToolRunOutput::default()
            },
            exit: ToolRunExit::Completed,
        },
        Err(McpFailure::Tool(error)) => ToolRunReport {
            output: ToolRunOutput {
                status: Some(ReactionStatus::Failed),
                error: Some(error),
                ..ToolRunOutput::default()
            },
            exit: ToolRunExit::Failed,
        },
        Err(McpFailure::Call(error)) => ToolRunReport::failed(error),
    }
}

#[cfg(test)]
mod tests;
