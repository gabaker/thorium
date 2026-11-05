//! A minimal Thorium harness built on `ThorPal`
//!
//! Reads the files the thorium-agent writes for a workflow run, gives `ThorPal`'s main agent the
//! run's model and the entry agent's tools, and writes the answer, conversation, traces, and
//! status back where the thorium-agent collects them. It reads no model settings from its
//! environment: the model, its endpoint, and its key come only from the models file. Its HTTP
//! clients take the proxy and CA settings from the standard env vars, which it logs at start.

use std::sync::{Arc, Mutex};
use std::time::Duration;
use thorpal::{GhidraSettings, ThorPal, ThorPalEvent};
use tokio::sync::mpsc::UnboundedReceiver;

mod dispatch;
mod interface;
mod traces;

use dispatch::Dispatcher;
use interface::{
    DEFAULT_TIMEOUT_SECS, HarnessPaths, HarnessStatus, NetworkSettings, RunFiles, RunPlan,
    StatusTool, TIMEOUT_ENV, write_file,
};
use traces::Recorder;

/// The exit code of a run that finished with an answer
const EXIT_OK: i32 = 0;

/// The exit code of a run that failed after it started
const EXIT_FAILED: i32 = 1;

/// The exit code of a run refused before its first model call
const EXIT_REFUSED: i32 = 2;

/// The exit code of a run that hit the run timeout
const EXIT_TIMED_OUT: i32 = 3;

/// How a run ended
struct Ending {
    /// The answer, if the run produced one
    answer: Option<String>,
    /// Why the run failed, if it did
    reason: Option<String>,
    /// The process exit code
    code: i32,
}

/// Write the status file, logging a failure to write it
///
/// # Arguments
///
/// * `paths` - Where the status file goes
/// * `status` - The status to write
fn write_status(paths: &HarnessPaths, status: &HarnessStatus) {
    // the status is always JSON
    let body = serde_json::to_vec_pretty(status).unwrap_or_default();
    if let Err(error) = write_file(&paths.outputs.status, &body) {
        eprintln!("{error}");
    }
}

/// Refuse a run before its first model call, writing the reason to the status file
///
/// # Arguments
///
/// * `paths` - Where the status file goes
/// * `reason` - Why the run is refused
fn refuse(paths: &HarnessPaths, reason: &str) -> i32 {
    // log the reason and record it in the status file
    eprintln!("thorpal-harness refused the run: {reason}");
    write_status(paths, &HarnessStatus::refused(reason));
    EXIT_REFUSED
}

/// Read the run timeout from the harness image's environment
fn run_timeout() -> Duration {
    // an unset or unreadable value means the default
    let secs = std::env::var(TIMEOUT_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// Record every `ThorPal` event until the run's event channel closes
///
/// # Arguments
///
/// * `events` - The run's events
/// * `recorder` - Where the events are recorded
async fn record(mut events: UnboundedReceiver<ThorPalEvent>, recorder: Arc<Mutex<Recorder>>) {
    // record each event in the order ThorPal emitted it
    while let Some(event) = events.recv().await {
        if let Ok(mut recorder) = recorder.lock() {
            recorder.record(&event);
        }
    }
}

/// Build `ThorPal` for a run and prompt its main agent once
///
/// # Arguments
///
/// * `files` - The run's files
/// * `plan` - What the run gives `ThorPal`
/// * `recorder` - Where the run's events are recorded
async fn session(
    files: &RunFiles,
    plan: &RunPlan,
    recorder: &Arc<Mutex<Recorder>>,
) -> Result<String, String> {
    // render the entry agent's tools and bind them to Thorium
    let dispatcher = Arc::new(Dispatcher::new(files.keys.clone(), files.run.reaction));
    let config = &files.agent.agent.config;
    let rendered = dispatch::render(
        &plan.tools,
        &plan.model,
        &config.tool_choice,
        config.parallel_tool_calls,
        &dispatcher,
    )
    .map_err(|error| error.to_string())?;
    // log once what the run's tools lose on the way to the model
    for warning in &rendered.warnings {
        eprintln!("thorpal-harness: {warning}");
    }
    // ThorPal gets the sample, and Ghidra stays unused since no agent of the run needs it
    let projects = std::env::temp_dir().join("thorpal-harness");
    let ghidra = GhidraSettings::from_binary(&plan.binary, projects, None);
    // record the run's events as ThorPal emits them
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let recording = tokio::spawn(record(receiver, Arc::clone(recorder)));
    // build the main agent with the entry agent's system prompt and tools
    let mut builder = ThorPal::builder(plan.provider.clone(), ghidra).events(sender);
    if let Some(preamble) = &plan.preamble {
        builder = builder.preamble(preamble);
    }
    for tool in rendered.tools {
        builder = builder.dynamic_tool(tool);
    }
    let thorpal = builder.build().await.map_err(|error| error.to_string())?;
    // prompt the main agent, which calls tools until it answers
    let answer = thorpal
        .prompt(&plan.prompt)
        .await
        .map_err(|error| format!("the model call failed: {error}"));
    // let the recorder drain the run's last events
    drop(thorpal);
    let _ = recording.await;
    answer
}

/// Run a workflow run's entry agent with `ThorPal`
///
/// # Arguments
///
/// * `args` - The command line arguments after the program name
async fn run(args: &[String]) -> i32 {
    // print the declared capabilities when asked
    if args.first().map(String::as_str) == Some("--capabilities") {
        println!(
            "{}",
            serde_json::to_string_pretty(&interface::capabilities()).unwrap_or_default()
        );
        return EXIT_OK;
    }
    // read where the run's files are, then the files themselves
    let paths = match HarnessPaths::from_args(args) {
        Ok(paths) => paths,
        Err(error) => return refuse(&HarnessPaths::default(), &error),
    };
    // log the proxies and CA certificates model and Thorium calls use, without proxy user info
    let network = NetworkSettings::from_env();
    eprintln!("thorpal-harness: {network}");
    for warning in network.ca_dir_warnings() {
        eprintln!("thorpal-harness: {warning}");
    }
    // CA settings that load no certificates would stop rig from building the model client
    if let Err(error) = interface::probe_model_client() {
        return refuse(&paths, &network.client_problem(&error));
    }
    let files = match RunFiles::load(&paths.inputs) {
        Ok(files) => files,
        Err(error) => return refuse(&paths, &error),
    };
    // check everything ThorPal needs before the first model call
    let plan = match interface::plan(&files, &paths.samples) {
        Ok(plan) => plan,
        Err(error) => return refuse(&paths, &error),
    };
    // log the tools mapped into the run so the workflow's mapping shows in the stage logs
    let tools = plan.status_tools();
    for tool in &tools {
        eprintln!("thorpal-harness: {tool}");
    }
    // log the samples ThorPal isn't given
    for skipped in &plan.skipped {
        eprintln!("thorpal-harness: ThorPal analyzes one sample, so sample {skipped} is skipped");
    }
    // run the session under the run timeout
    let recorder = Arc::new(Mutex::new(Recorder::default()));
    let timeout = run_timeout();
    let ending = match tokio::time::timeout(timeout, session(&files, &plan, &recorder)).await {
        Ok(Ok(answer)) => Ending {
            answer: Some(answer),
            reason: None,
            code: EXIT_OK,
        },
        Ok(Err(reason)) => Ending {
            answer: None,
            reason: Some(reason),
            code: EXIT_FAILED,
        },
        Err(_) => Ending {
            answer: None,
            reason: Some(format!(
                "the run timed out after {} seconds",
                timeout.as_secs()
            )),
            code: EXIT_TIMED_OUT,
        },
    };
    finish(&paths, &recorder, tools, ending)
}

/// Write a run's outputs and pick its exit code
///
/// # Arguments
///
/// * `paths` - Where the outputs go
/// * `recorder` - The run's recorded events
/// * `tools` - The tools mapped into the run
/// * `ending` - How the run ended
fn finish(
    paths: &HarnessPaths,
    recorder: &Arc<Mutex<Recorder>>,
    tools: Vec<StatusTool>,
    ending: Ending,
) -> i32 {
    // take what was recorded, even from a run that timed out
    let recorder = match recorder.lock() {
        Ok(recorder) => recorder.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    let mut code = ending.code;
    // write the conversation and traces as JSON lines
    let mut writes = vec![
        write_file(
            paths.result_files.join("conversation.jsonl"),
            recorder.conversation().as_bytes(),
        ),
        write_file(
            paths.result_files.join("traces.jsonl"),
            recorder.traces().as_bytes(),
        ),
    ];
    // write the answer of a run that has one
    if let Some(answer) = &ending.answer {
        writes.push(write_file(&paths.outputs.answer, answer.as_bytes()));
    }
    // a run whose outputs can't be written failed
    let mut reason = ending.reason;
    for error in writes.into_iter().filter_map(Result::err) {
        eprintln!("{error}");
        reason.get_or_insert(error);
        code = EXIT_FAILED;
    }
    // record how the run ended in the status file
    if let Some(reason) = &reason {
        eprintln!("thorpal-harness: the run failed: {reason}");
    }
    let mut status = recorder.status();
    status.success = code == EXIT_OK;
    status.reason = reason;
    status.tools = tools;
    write_status(paths, &status);
    code
}

/// Run the harness and exit with its code
#[tokio::main]
async fn main() {
    // the arguments after the program name pick the files
    let args: Vec<String> = std::env::args().skip(1).collect();
    // run the harness on the heap, since its future is too large to keep on the stack
    let code = Box::pin(run(&args)).await;
    std::process::exit(code);
}
