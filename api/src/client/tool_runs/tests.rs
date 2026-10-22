//! Tests for the shared tool-run helper against a mock Thorium API

use chrono::Utc;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

use super::{
    NoHooks, ToolRunExit, ToolRunHooks, ToolRunKind, ToolRunOptions, ToolRunOutput, call_mcp, plan,
    results, run, status,
};
use crate::Thorium;
use crate::models::{
    AgenticTool, AgenticToolBacking, ArgTarget, ContainerToolBacking, Entity, EntityKinds,
    EntityMetadata, Output, OutputDisplayType, OutputMap, PlannedStage, Reaction, ReactionPlan,
    ReactionSource, ReactionStatus, RunInputs, ToolInputMapping, ToolInstanceEntity,
    ToolOutputDelivery, ToolRunRequest, ToolSpec, ToolVersion, WorkloadSpec, tool_run_retry_after,
};

/// The answer to one request: a status code, extra headers, and a body
type Answer = (u16, Vec<(String, String)>, String);

/// One request a mock API received
#[derive(Debug, Clone)]
struct Received {
    /// The request line (`METHOD /path HTTP/1.1`)
    line: String,
    /// The whole request, head and body
    text: String,
    /// When the request arrived
    at: Instant,
}

/// A running mock Thorium API that records every request
struct Mock {
    /// The url the mock API listens at
    host: String,
    /// Every request the mock API received, in order
    received: Arc<Mutex<Vec<Received>>>,
}

impl Mock {
    /// Start a mock API that answers each request with its router
    ///
    /// # Arguments
    ///
    /// * `router` - Picks the answer to a request from its request line and body
    async fn start<F>(router: F) -> Self
    where
        F: Fn(&str, &str) -> Answer + Send + Sync + 'static,
    {
        // bind to a random local port
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let inner = received.clone();
        let router = Arc::new(router);
        // answer requests in the background
        tokio::spawn(async move {
            loop {
                // accept the next connection
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                // read the request head and its whole body
                let text = read_request(&mut stream).await;
                let line = text.lines().next().unwrap_or_default().to_owned();
                let body = text
                    .split_once("\r\n\r\n")
                    .map_or_else(String::new, |(_, body)| body.to_owned());
                inner.lock().unwrap().push(Received {
                    line: line.clone(),
                    text,
                    at: Instant::now(),
                });
                // answer and close the connection
                let (code, headers, body) = router(&line, &body);
                let extra = headers
                    .iter()
                    .fold(String::new(), |mut extra, (name, value)| {
                        let _ = write!(extra, "{name}: {value}\r\n");
                        extra
                    });
                let resp = format!(
                    "HTTP/1.1 {code} Mock\r\ncontent-type: application/json\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        Mock {
            host: format!("http://{addr}"),
            received,
        }
    }

    /// Get the request lines this mock API received, in order
    fn lines(&self) -> Vec<String> {
        self.received
            .lock()
            .unwrap()
            .iter()
            .map(|req| req.line.clone())
            .collect()
    }

    /// Get every request whose line contains a pattern
    ///
    /// # Arguments
    ///
    /// * `pattern` - The text the request line must contain
    fn matching(&self, pattern: &str) -> Vec<Received> {
        self.received
            .lock()
            .unwrap()
            .iter()
            .filter(|req| req.line.contains(pattern))
            .cloned()
            .collect()
    }

    /// Build a client for this mock API
    async fn client(&self) -> Thorium {
        Thorium::build(&self.host)
            .token("secret-token")
            .build()
            .await
            .unwrap()
    }
}

/// Read one whole request from a connection
///
/// # Arguments
///
/// * `stream` - The connection to read from
async fn read_request(stream: &mut tokio::net::TcpStream) -> String {
    // read until the head and the body it announces have arrived
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
            return text;
        }
    }
}

/// Answer with a json body and no extra headers
///
/// # Arguments
///
/// * `code` - The status code
/// * `body` - The body
fn answer(code: u16, body: impl Into<String>) -> Answer {
    (code, Vec::new(), body.into())
}

/// Build a tool run's reaction in the corn group
///
/// # Arguments
///
/// * `id` - The reaction's id
/// * `status` - The reaction's status
/// * `samples` - The reaction's input files
fn tool_reaction(id: Uuid, status: ReactionStatus, samples: &[&str]) -> Reaction {
    // a direct tool run of the harvester tool
    let tool_instance = Uuid::new_v4();
    Reaction {
        id,
        group: "corn".to_owned(),
        creator: "mcarson".to_owned(),
        pipeline: "harvester".to_owned(),
        status,
        current_stage: 0,
        current_stage_progress: 0,
        current_stage_length: 1,
        args: HashMap::default(),
        sla: Utc::now(),
        jobs: Vec::new(),
        tags: Vec::new(),
        parent: None,
        sub_reactions: 0,
        completed_sub_reactions: 0,
        generators: Vec::new(),
        samples: samples.iter().map(|sample| (*sample).to_owned()).collect(),
        ephemeral: Vec::new(),
        parent_ephemeral: HashMap::default(),
        repos: Vec::new(),
        trigger_depth: None,
        has_cache: false,
        source: ReactionSource::Tool {
            tool_instance,
            provenance: crate::models::RunProvenance::default(),
        },
        plan: ReactionPlan {
            stages: vec![vec![PlannedStage {
                name: "harvester".to_owned(),
                tool_instance,
            }]],
        },
        run_token: None,
        run: None,
    }
}

/// Build the results stored on a file under the harvester tool
///
/// # Arguments
///
/// * `result` - The result value
/// * `files` - The names of the result files
fn output_map(result: &Value, files: &[&str]) -> (Uuid, String) {
    // one newest result with some files
    let id = Uuid::new_v4();
    let output = Output {
        id,
        groups: vec!["corn".to_owned()],
        tool_version: Some(ToolVersion::from("1.2.0")),
        cmd: None,
        uploaded: Utc::now(),
        deserialization_error: None,
        result: result.clone(),
        files: files.iter().map(|file| (*file).to_owned()).collect(),
        entities: HashMap::default(),
        display_type: OutputDisplayType::Json,
        children: HashMap::default(),
    };
    let mut results = HashMap::new();
    results.insert("harvester".to_owned(), vec![output]);
    (id, serde_json::to_string(&OutputMap { results }).unwrap())
}

/// The SHA256 of the file the test runs are given
const SHA256: &str = "63b9c2cbd4f4f1f6c2a0c2a1d7e0d2e6c2b6a8a3b1c4d5e6f7a8b9c0d1e2f3a4";

/// Build fast polling options that download into a fresh directory
///
/// # Arguments
///
/// * `start` - The first wait between polls
fn fast_opts(start: Duration) -> ToolRunOptions {
    // a fresh output directory and a fast backoff
    let dir = std::env::temp_dir().join(format!("thorium-tool-runs-{}", Uuid::new_v4()));
    let mut opts = ToolRunOptions::new(dir);
    opts.poll_start = start;
    opts.poll_max = start * 8;
    opts
}

/// Mock the API of a tool run that is started, then completes on a later poll
///
/// Returns the mock and the reaction's id.
///
/// # Arguments
///
/// * `tool_instance` - The tool instance the run runs
/// * `started_polls` - How many reads return the run as `Started` before it's `Completed`
/// * `result` - The result stored on the input file
async fn completing_run(tool_instance: Uuid, started_polls: usize, result: Value) -> (Mock, Uuid) {
    // the run's reaction, its result, and its result file
    let id = Uuid::new_v4();
    let started =
        serde_json::to_string(&tool_reaction(id, ReactionStatus::Started, &[SHA256])).unwrap();
    let completed =
        serde_json::to_string(&tool_reaction(id, ReactionStatus::Completed, &[SHA256])).unwrap();
    let (output_id, outputs) = output_map(&result, &["out/report.txt"]);
    let reads = Arc::new(Mutex::new(0_usize));
    let runs_path = format!("POST /api/tools/instances/{tool_instance}/runs ");
    let reaction_path = format!("GET /api/reactions/corn/{id} ");
    let results_path = format!("GET /api/files/results/{SHA256}");
    let file_path = format!("GET /api/files/result-files/{SHA256}/harvester/{output_id}");
    let mock = Mock::start(move |line, _| {
        if line.starts_with(&runs_path) {
            answer(200, json!({"id": id}).to_string())
        } else if line.starts_with(&reaction_path) {
            // the run is started for a few reads, then completed
            let mut count = reads.lock().unwrap();
            *count += 1;
            if *count <= started_polls {
                answer(200, started.clone())
            } else {
                answer(200, completed.clone())
            }
        } else if line.starts_with(&results_path) {
            answer(200, outputs.clone())
        } else if line.starts_with(&file_path) {
            answer(200, "decompiled code")
        } else {
            answer(404, r#"{"error":"not found"}"#)
        }
    })
    .await;
    (mock, id)
}

/// Result file and result entity downloads percent-encode the tool name in the url path, so a
/// workflow instance name like `harvester#r1` reaches the API whole instead of ending the path
#[tokio::test]
async fn result_downloads_encode_the_tool_name() {
    // a mock API that answers every request with an empty list
    let mock = Mock::start(|_, _| answer(200, "[]")).await;
    let thorium = mock.client().await;
    let result_id = Uuid::new_v4();
    // download a file's result file, a repo's result file, and a file's result entities
    crate::client::ResultsClient::download_result_file(
        &thorium.files,
        SHA256,
        "harvester#r1",
        &result_id,
        "out/report.txt",
    )
    .await
    .unwrap();
    crate::client::ResultsClient::download_result_file(
        &thorium.repos,
        "github.com/jupiter/io",
        "harvester#r1",
        &result_id,
        "out/report.txt",
    )
    .await
    .unwrap();
    crate::client::ResultsClient::download_result_entities(
        &thorium.files,
        SHA256,
        "harvester#r1",
        result_id,
        EntityKinds::Device,
    )
    .await
    .unwrap();
    // every request path names the whole tool, with its `#` encoded
    let lines = mock.lines();
    assert_eq!(lines.len(), 3, "{lines:?}");
    let expected = [
        format!("GET /api/files/result-files/{SHA256}/harvester%23r1/{result_id}?"),
        format!("GET /api/repos/result-files/github.com/jupiter/io/harvester%23r1/{result_id}?"),
        format!("GET /api/files/result-files/{SHA256}/harvester%23r1/{result_id}/"),
    ];
    for (line, expected) in lines.iter().zip(expected) {
        assert!(line.starts_with(&expected), "{line} != {expected}");
    }
}

/// The helper starts the run, polls it with growing intervals until it completes, downloads each
/// result file into the output directory, and returns the CLI wrapper's JSON object
#[tokio::test]
async fn run_polls_and_downloads_results() {
    // a run that completes on its fourth read
    let tool_instance = Uuid::new_v4();
    let (mock, id) = completing_run(tool_instance, 3, json!({"functions": 412})).await;
    let thorium = mock.client().await;
    let opts = fast_opts(Duration::from_millis(60));
    let req = ToolRunRequest::new(RunInputs::default().sample(SHA256));
    // run it and wait for it
    let report = run(
        &thorium,
        tool_instance,
        &["corn".to_owned()],
        &req,
        &opts,
        &mut NoHooks,
    )
    .await;
    // the run completed and its result file was downloaded into the output directory
    let file = opts.output.join("out/report.txt");
    assert_eq!(report.exit, ToolRunExit::Completed, "{report:?}");
    assert_eq!(
        report.output,
        ToolRunOutput {
            status: Some(ReactionStatus::Completed),
            reaction: Some(id),
            result: Some(json!({"functions": 412})),
            files: vec![file.to_string_lossy().into_owned()],
            error: None,
            retry_after_secs: None,
        }
    );
    assert_eq!(
        tokio::fs::read_to_string(&file).await.unwrap(),
        "decompiled code"
    );
    tokio::fs::remove_dir_all(&opts.output).await.unwrap();
    // the run was started once and read until it completed, with growing waits between reads
    let lines = mock.lines();
    assert_eq!(
        lines.iter().filter(|line| line.contains("/runs ")).count(),
        1
    );
    let reads = mock.matching(&format!("GET /api/reactions/corn/{id} "));
    assert_eq!(reads.len(), 4, "{lines:?}");
    let gaps: Vec<Duration> = reads
        .windows(2)
        .map(|pair| pair[1].at - pair[0].at)
        .collect();
    assert!(gaps[0] >= Duration::from_millis(50), "{gaps:?}");
    assert!(gaps[1] > gaps[0] + Duration::from_millis(30), "{gaps:?}");
    assert!(gaps[2] > gaps[1] + Duration::from_millis(60), "{gaps:?}");
    // the results were read under the run's tool name
    let results = mock.matching("/api/files/results/");
    assert!(
        results[0].line.contains("tools%5B%5D=harvester"),
        "{results:?}"
    );
}

/// A failed run returns the object with status `Failed` and downloads nothing
#[tokio::test]
async fn failed_run_downloads_nothing() {
    // a run whose reaction failed
    let tool_instance = Uuid::new_v4();
    let id = Uuid::new_v4();
    let failed =
        serde_json::to_string(&tool_reaction(id, ReactionStatus::Failed, &[SHA256])).unwrap();
    let runs_path = format!("POST /api/tools/instances/{tool_instance}/runs ");
    let mock = Mock::start(move |line, _| {
        if line.starts_with(&runs_path) {
            answer(200, json!({"id": id}).to_string())
        } else if line.starts_with("GET /api/reactions/corn/") {
            answer(200, failed.clone())
        } else {
            answer(500, "{}")
        }
    })
    .await;
    let thorium = mock.client().await;
    let opts = fast_opts(Duration::from_millis(10));
    let req = ToolRunRequest::new(RunInputs::default().sample(SHA256));
    let report = run(
        &thorium,
        tool_instance,
        &["corn".to_owned()],
        &req,
        &opts,
        &mut NoHooks,
    )
    .await;
    // the run failed with an error, no result, and no files, and no results were read
    assert_eq!(report.exit, ToolRunExit::Failed);
    assert_eq!(report.exit.code(), 1);
    assert_eq!(report.output.status, Some(ReactionStatus::Failed));
    assert_eq!(report.output.reaction, Some(id));
    assert_eq!(report.output.result, None);
    assert_eq!(report.output.files, Vec::<String>::new());
    assert!(report.output.error.is_some());
    assert!(mock.matching("/results/").is_empty(), "{:?}", mock.lines());
    assert!(!opts.output.exists());
}

/// A `404` while polling ends the run with an error naming the reaction
#[tokio::test]
async fn poll_not_found_names_the_reaction() {
    // a run whose reaction is readable once, then gone
    let tool_instance = Uuid::new_v4();
    let id = Uuid::new_v4();
    let started =
        serde_json::to_string(&tool_reaction(id, ReactionStatus::Started, &[SHA256])).unwrap();
    let reads = Arc::new(Mutex::new(0_usize));
    let runs_path = format!("POST /api/tools/instances/{tool_instance}/runs ");
    let mock = Mock::start(move |line, _| {
        if line.starts_with(&runs_path) {
            answer(200, json!({"id": id}).to_string())
        } else if line.starts_with("GET /api/reactions/corn/") {
            let mut count = reads.lock().unwrap();
            *count += 1;
            if *count == 1 {
                answer(200, started.clone())
            } else {
                answer(404, r#"{"error":"reaction not found"}"#)
            }
        } else {
            answer(500, "{}")
        }
    })
    .await;
    let thorium = mock.client().await;
    let opts = fast_opts(Duration::from_millis(10));
    let req = ToolRunRequest::new(RunInputs::default().sample(SHA256));
    let report = run(
        &thorium,
        tool_instance,
        &["corn".to_owned()],
        &req,
        &opts,
        &mut NoHooks,
    )
    .await;
    // the run ended in an error that names the reaction
    assert_eq!(report.exit, ToolRunExit::Failed);
    let error = report.output.error.clone().unwrap_or_default();
    assert!(error.contains(&id.to_string()), "{error}");
    assert!(error.contains("404"), "{error}");
}

/// Reaching the timeout prints the status and reaction and exits 3, and not waiting returns the
/// reaction at once
#[tokio::test]
async fn timeout_and_no_wait() {
    // a run that never finishes
    let tool_instance = Uuid::new_v4();
    let (mock, id) = completing_run(tool_instance, usize::MAX, json!({})).await;
    let thorium = mock.client().await;
    let req = ToolRunRequest::new(RunInputs::default().sample(SHA256));
    // wait at most a short time
    let mut opts = fast_opts(Duration::from_millis(20));
    opts.timeout = Some(Duration::from_millis(150));
    let report = run(
        &thorium,
        tool_instance,
        &["corn".to_owned()],
        &req,
        &opts,
        &mut NoHooks,
    )
    .await;
    assert_eq!(report.exit, ToolRunExit::TimedOut);
    assert_eq!(report.exit.code(), 3);
    assert_eq!(report.output.status, Some(ReactionStatus::Started));
    assert_eq!(report.output.reaction, Some(id));
    assert!(report.output.retry_after_secs.is_some());
    assert!(
        report
            .output
            .error
            .unwrap_or_default()
            .contains("timed out")
    );
    // don't wait at all
    let mut opts = fast_opts(Duration::from_millis(20));
    opts.wait = false;
    let report = run(
        &thorium,
        tool_instance,
        &["corn".to_owned()],
        &req,
        &opts,
        &mut NoHooks,
    )
    .await;
    assert_eq!(report.exit, ToolRunExit::Completed);
    assert_eq!(report.exit.code(), 0);
    assert_eq!(report.output.reaction, Some(id));
    assert_eq!(report.output.result, None);
    assert_eq!(report.output.retry_after_secs, Some(1));
}

/// Hooks hear the run start, then the results get collected
#[tokio::test]
async fn hooks_hear_start_and_collection() {
    /// Records each hook call
    #[derive(Default)]
    struct Recorder(Vec<String>);
    impl ToolRunHooks for Recorder {
        fn started(&mut self, reaction: &Reaction) -> Result<(), crate::Error> {
            self.0
                .push(format!("started {} {}", reaction.id, reaction.group));
            Ok(())
        }
        fn collected(&mut self, reaction: &Reaction) -> Result<(), crate::Error> {
            self.0.push(format!("collected {}", reaction.id));
            Ok(())
        }
    }
    // a run that completes on its second read
    let tool_instance = Uuid::new_v4();
    let (mock, id) = completing_run(tool_instance, 1, json!({"ok": true})).await;
    let thorium = mock.client().await;
    let opts = fast_opts(Duration::from_millis(10));
    let req = ToolRunRequest::new(RunInputs::default().sample(SHA256));
    let mut hooks = Recorder::default();
    let report = run(
        &thorium,
        tool_instance,
        &["corn".to_owned()],
        &req,
        &opts,
        &mut hooks,
    )
    .await;
    assert_eq!(report.exit, ToolRunExit::Completed, "{report:?}");
    assert_eq!(
        hooks.0,
        vec![format!("started {id} corn"), format!("collected {id}")]
    );
    tokio::fs::remove_dir_all(&opts.output).await.unwrap();
}

/// `tool status` returns the polling hint from the run's start while it runs and none once it's
/// finished
#[tokio::test]
async fn status_hint_follows_backoff() {
    // a started run created five seconds ago, and a finished one
    let running = Uuid::new_v4();
    let finished = Uuid::new_v4();
    let failed = Uuid::new_v4();
    let started_json =
        serde_json::to_string(&tool_reaction(running, ReactionStatus::Started, &[SHA256])).unwrap();
    let finished_json = serde_json::to_string(&tool_reaction(
        finished,
        ReactionStatus::Completed,
        &[SHA256],
    ))
    .unwrap();
    let failed_json =
        serde_json::to_string(&tool_reaction(failed, ReactionStatus::Failed, &[SHA256])).unwrap();
    let logs = json!([{
        "group": "corn", "pipeline": "harvester", "reaction": running.to_string(),
        "action": "ReactionCreated", "timestamp": Utc::now() - chrono::Duration::seconds(5),
        "msg": null, "update": {},
    }])
    .to_string();
    let user = serde_json::to_string(&crate::models::ScrubbedUser {
        username: "mcarson".to_owned(),
        role: crate::models::UserRole::User,
        email: "mcarson@example.com".to_owned(),
        groups: vec!["corn".to_owned()],
        actual_groups: Vec::new(),
        token: "token".to_owned(),
        token_expiration: Utc::now(),
        unix: None,
        settings: crate::models::UserSettings::default(),
        local: true,
        verified: true,
        has_image: false,
    })
    .unwrap();
    let mock = Mock::start(move |line, _| {
        if line.starts_with(&format!("GET /api/reactions/corn/{running} ")) {
            answer(200, started_json.clone())
        } else if line.starts_with(&format!("GET /api/reactions/corn/{finished} ")) {
            answer(200, finished_json.clone())
        } else if line.starts_with(&format!("GET /api/reactions/corn/{failed} ")) {
            answer(200, failed_json.clone())
        } else if line.starts_with("GET /api/reactions/logs/") {
            answer(200, logs.clone())
        } else if line.starts_with("GET /api/users/whoami ") {
            answer(200, user.clone())
        } else {
            answer(404, r#"{"error":"not found"}"#)
        }
    })
    .await;
    let thorium = mock.client().await;
    // a running run's hint is the time left until the backoff's next poll
    let report = status(&thorium, running, Some("corn")).await;
    assert_eq!(report.exit, ToolRunExit::Completed);
    assert_eq!(report.output.status, Some(ReactionStatus::Started));
    let expected = tool_run_retry_after(&ReactionStatus::Started, 5);
    let hint = report.output.retry_after_secs;
    assert!(
        hint == expected || hint == tool_run_retry_after(&ReactionStatus::Started, 6),
        "{hint:?}"
    );
    // a finished run has no hint, found in the caller's groups
    let report = status(&thorium, finished, None).await;
    assert_eq!(report.output.status, Some(ReactionStatus::Completed));
    assert_eq!(report.output.retry_after_secs, None);
    assert_eq!(report.exit, ToolRunExit::Completed);
    // a failed run exits 1
    let report = status(&thorium, failed, None).await;
    assert_eq!(report.exit, ToolRunExit::Failed);
    // a run in none of the caller's groups is the API's 404
    let missing = Uuid::new_v4();
    let report = status(&thorium, missing, None).await;
    assert_eq!(report.exit, ToolRunExit::Failed);
    let error = report.output.error.unwrap_or_default();
    assert!(
        error.contains("404") && error.contains(&missing.to_string()),
        "{error}"
    );
}

/// The returned result is truncated to the effective result size, and 0 means unlimited
#[tokio::test]
async fn results_truncate_to_the_limit() {
    // a completed run with a long result
    let tool_instance = Uuid::new_v4();
    let result = json!({"functions": 412, "names": "abcdefghijklmnopqrstuvwxyz"});
    let (mock, id) = completing_run(tool_instance, 0, result.clone()).await;
    let thorium = mock.client().await;
    // a 10 byte limit keeps the first 10 bytes of the result's text
    let mut opts = fast_opts(Duration::from_millis(10));
    opts.max_result_bytes = 10;
    let report = results(&thorium, id, Some("corn"), &opts, &mut NoHooks).await;
    assert_eq!(report.exit, ToolRunExit::Completed, "{report:?}");
    let text = result.to_string();
    assert_eq!(
        report.output.result,
        Some(Value::String(text[..10].to_owned()))
    );
    tokio::fs::remove_dir_all(&opts.output).await.unwrap();
    // no limit returns the whole result
    let opts = fast_opts(Duration::from_millis(10));
    let report = results(&thorium, id, Some("corn"), &opts, &mut NoHooks).await;
    assert_eq!(report.output.result, Some(result));
    tokio::fs::remove_dir_all(&opts.output).await.unwrap();
}

/// A result file whose name would leave the output directory is refused and nothing is written
/// outside of it
#[tokio::test]
async fn results_refuse_escaping_file_names() {
    // a completed run whose result names a file outside the output directory
    let id = Uuid::new_v4();
    let completed =
        serde_json::to_string(&tool_reaction(id, ReactionStatus::Completed, &[SHA256])).unwrap();
    let (_, outputs) = output_map(&json!({}), &["../escaped.txt"]);
    let mock = Mock::start(move |line, _| {
        if line.starts_with("GET /api/reactions/corn/") {
            answer(200, completed.clone())
        } else if line.starts_with("GET /api/files/results/") {
            answer(200, outputs.clone())
        } else {
            answer(200, "escaped")
        }
    })
    .await;
    let thorium = mock.client().await;
    let mut opts = fast_opts(Duration::from_millis(10));
    opts.output = opts.output.join("out");
    let report = results(&thorium, id, Some("corn"), &opts, &mut NoHooks).await;
    assert_eq!(report.exit, ToolRunExit::Failed);
    assert!(
        report
            .output
            .error
            .unwrap_or_default()
            .contains("escaped.txt")
    );
    assert!(!opts.output.parent().unwrap().join("escaped.txt").exists());
    assert!(
        mock.matching("/result-files/").is_empty(),
        "{:?}",
        mock.lines()
    );
}

/// Build a tool instance entity in the corn group
///
/// # Arguments
///
/// * `spec` - The tool instance's spec
fn instance(spec: ToolSpec) -> Entity {
    // instances are named after their tool and version
    Entity {
        id: Uuid::new_v4(),
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
        tags: HashMap::default(),
        image: None,
        created: Utc::now(),
    }
}

/// Build an agentic tool with a backing
///
/// # Arguments
///
/// * `backing` - The tool's backing
fn agentic(backing: AgenticToolBacking) -> AgenticTool {
    // a tool that takes a depth and files
    AgenticTool {
        function_name: "run_ghidra".to_owned(),
        title: None,
        model_description: "Decompile a file".to_owned(),
        input_schema: serde_json::Map::new(),
        output_schema: None,
        capabilities: crate::models::ToolCapabilities::default(),
        backing,
        result_format: crate::models::ToolResultFormat::Json,
        requires_approval: false,
        timeout: Some(600),
        input_examples: Vec::new(),
        strict: crate::models::ToolStrictness::Auto,
        loading: crate::models::ToolLoading::Eager,
        icons: Vec::new(),
    }
}

/// A container tool maps its input onto its job and delivers its results to its output path, a
/// Thorium MCP tool is called through the MCP server, and other tool instances take no input
#[test]
fn plan_follows_the_backing() {
    // a container tool with a depth kwarg and a samples property
    let mut args = std::collections::BTreeMap::new();
    args.insert("depth".to_owned(), ArgTarget::Kwarg("--depth".to_owned()));
    let container = ContainerToolBacking {
        tool_instance: Uuid::new_v4(),
        inputs: ToolInputMapping {
            args,
            samples: Some("files".to_owned()),
        },
        outputs: ToolOutputDelivery {
            path: None,
            max_result_bytes: 300,
        },
    };
    let entity = instance(ToolSpec::Agentic(agentic(AgenticToolBacking::Container(
        container.clone(),
    ))));
    let input = json!({"depth": 2, "files": [SHA256]});
    let planned = plan(&entity, input.as_object().unwrap()).unwrap();
    let ToolRunKind::Job(mapped) = &planned.kind else {
        panic!("a container tool runs a job: {planned:?}");
    };
    assert_eq!(
        mapped.args.kwargs.get("--depth"),
        Some(&vec!["2".to_owned()])
    );
    assert_eq!(mapped.samples, Some(vec![SHA256.to_owned()]));
    assert_eq!(planned.timeout, Some(600));
    assert_eq!(planned.max_result_bytes, 300);
    assert_eq!(
        planned.output,
        std::path::PathBuf::from("/tmp/thorium/tool-results/run_ghidra")
    );
    // a set output path is where results go
    let mut delivered = container;
    delivered.outputs.path = Some("/work/out".to_owned());
    let entity = instance(ToolSpec::Agentic(agentic(AgenticToolBacking::Container(
        delivered,
    ))));
    let planned = plan(&entity, &serde_json::Map::new()).unwrap();
    assert_eq!(planned.output, std::path::PathBuf::from("/work/out"));
    // a Thorium MCP tool is called through the MCP server with the input as is
    let entity = instance(ToolSpec::Agentic(agentic(AgenticToolBacking::ThoriumMcp {
        tool: "get_sample".to_owned(),
    })));
    let planned = plan(&entity, input.as_object().unwrap()).unwrap();
    assert_eq!(
        planned.kind,
        ToolRunKind::Mcp {
            tool: "get_sample".to_owned(),
            input: input.as_object().unwrap().clone()
        }
    );
    // an Image tool instance takes no input and delivers under its tool's name
    let entity = instance(ToolSpec::Image(WorkloadSpec::default()));
    let planned = plan(&entity, &serde_json::Map::new()).unwrap();
    assert_eq!(
        planned.kind,
        ToolRunKind::Job(crate::models::MappedToolInput::default())
    );
    assert_eq!(
        planned.output,
        std::path::PathBuf::from("/tmp/thorium/tool-results/harvester")
    );
    assert!(plan(&entity, input.as_object().unwrap()).is_err());
    // a switch given a number is a bad input
    let mut args = std::collections::BTreeMap::new();
    args.insert("deep".to_owned(), ArgTarget::Switch("--deep".to_owned()));
    let backing = AgenticToolBacking::Container(ContainerToolBacking {
        tool_instance: Uuid::new_v4(),
        inputs: ToolInputMapping {
            args,
            samples: None,
        },
        outputs: ToolOutputDelivery::default(),
    });
    let entity = instance(ToolSpec::Agentic(agentic(backing)));
    let err = plan(&entity, json!({"deep": 3}).as_object().unwrap()).unwrap_err();
    assert!(err.contains("deep"), "{err}");
}

/// A Thorium MCP tool is called through the MCP server with the token as a bearer token, and
/// its structured result is returned with no reaction
#[tokio::test]
async fn mcp_call_returns_the_tool_result() {
    // an MCP server that starts a session and answers one tool call
    let mock = Mock::start(|line, body| {
        if !line.starts_with("POST /api/mcp ") {
            return answer(404, "{}");
        }
        let request: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        match request["method"].as_str() {
            Some("initialize") => (
                200,
                vec![("mcp-session-id".to_owned(), "session-1".to_owned())],
                json!({"jsonrpc": "2.0", "id": request["id"], "result": {
                    "protocolVersion": "2025-03-26", "capabilities": {},
                    "serverInfo": {"name": "thorium", "version": "1"}}})
                .to_string(),
            ),
            Some("notifications/initialized") => answer(202, ""),
            Some("tools/call") if request["params"]["name"] == "explode" => answer(
                200,
                json!({"jsonrpc": "2.0", "id": request["id"], "result": {
                    "content": [{"type": "text", "text": "boom"}], "isError": true}})
                .to_string(),
            ),
            Some("tools/call") => answer(
                200,
                json!({"jsonrpc": "2.0", "id": request["id"], "result": {
                    "content": [{"type": "text", "text": "{\"sha256\":\"abc\"}"}],
                    "structuredContent": {"sha256": "abc"}, "isError": false}})
                .to_string(),
            ),
            _ => answer(400, "{}"),
        }
    })
    .await;
    let opts = fast_opts(Duration::from_millis(10));
    let url = format!("{}/api/mcp", mock.host);
    let input = json!({"sha256": "abc"});
    let report = call_mcp(
        &url,
        "secret-token",
        "get_sample",
        input.as_object().unwrap(),
        &opts,
    )
    .await;
    // the tool's structured result is returned with no reaction
    assert_eq!(report.exit, ToolRunExit::Completed, "{report:?}");
    assert_eq!(report.output.reaction, None);
    assert_eq!(report.output.status, Some(ReactionStatus::Completed));
    assert_eq!(report.output.result, Some(json!({"sha256": "abc"})));
    // the call went to the tool with the input, in the session, with the bearer token
    let calls = mock.matching("POST /api/mcp ");
    assert_eq!(calls.len(), 3, "{:?}", mock.lines());
    let call = &calls[2].text;
    assert!(call.contains("\"tools/call\""), "{call}");
    assert!(call.contains("\"get_sample\""), "{call}");
    assert!(
        call.to_lowercase().contains("mcp-session-id: session-1"),
        "{call}"
    );
    assert!(call.contains("Bearer secret-token"), "{call}");
    // a tool error fails the call
    let report = call_mcp(
        &url,
        "secret-token",
        "explode",
        &serde_json::Map::new(),
        &opts,
    )
    .await;
    assert_eq!(report.exit, ToolRunExit::Failed);
    assert_eq!(report.output.status, Some(ReactionStatus::Failed));
    assert!(report.output.error.unwrap_or_default().contains("boom"));
}

/// Every command prints one JSON object with the same fields, and the exit codes are 0 to 3
#[test]
fn output_has_a_stable_schema() {
    // a refusal sets only the error
    let refused = ToolRunOutput::refused("bad input");
    let line = refused.to_line();
    assert!(!line.contains('\n'));
    let value: Value = serde_json::from_str(&line).unwrap();
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
        ]
    );
    assert_eq!(value["error"], "bad input");
    assert_eq!(value["status"], Value::Null);
    assert_eq!(value["files"], json!([]));
    // each ending has its exit code
    assert_eq!(ToolRunExit::Completed.code(), 0);
    assert_eq!(ToolRunExit::Failed.code(), 1);
    assert_eq!(ToolRunExit::Usage.code(), 2);
    assert_eq!(ToolRunExit::TimedOut.code(), 3);
    assert_eq!(super::ToolRunReport::usage("x").exit, ToolRunExit::Usage);
    assert_eq!(super::ToolRunReport::failed("x").exit, ToolRunExit::Failed);
}

/// The client fails a reaction through its handle route and passes on a conflict
#[tokio::test]
async fn reactions_fail_calls_the_handle_route() {
    // a reaction that fails once, then conflicts
    let id = Uuid::new_v4();
    let fails = Arc::new(Mutex::new(0_usize));
    let mock = Mock::start(move |line, _| {
        let mut count = fails.lock().unwrap();
        *count += 1;
        if *count == 1 {
            answer(202, r#"{"status":"Failed"}"#)
        } else {
            let _ = line;
            answer(409, r#"{"error":"already failed"}"#)
        }
    })
    .await;
    let thorium = mock.client().await;
    let handled = thorium.reactions.fail("corn", &id).await.unwrap();
    assert_eq!(handled.status, ReactionStatus::Failed);
    let err = thorium.reactions.fail("corn", &id).await.unwrap_err();
    assert_eq!(err.status(), Some(http::StatusCode::CONFLICT));
    assert_eq!(
        mock.lines()[0],
        format!("POST /api/reactions/handle/corn/{id}/fail HTTP/1.1")
    );
}
