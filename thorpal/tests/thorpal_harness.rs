//! Runs the thorpal-harness binary on fixture run files against a recording model endpoint and a
//! recording Thorium MCP server

use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use thorium::models::{AwsRoleSource, AzureClientAuth, CredentialKind, HarnessToolEntry, ModelApi};
use thorium::tools::render::{
    RenderOptions, ToolCallOutput, ToolCallResult, encode_result, render_tool,
};
use uuid::Uuid;

#[allow(dead_code)]
mod common {
    pub mod fixtures;
    pub mod mock;
}

use common::fixtures::{KEY, MODEL, RunFixture, TOKEN, TOOL};
use common::mock::{self, MockServer, Recorded, Reply};

/// The structured result the mock MCP server returns for every tool call
fn mcp_result() -> Value {
    // a small JSON object the model sees as the tool's result
    json!({"size": 42, "name": "io-sample.exe"})
}

/// Start a server that answers both the model and the MCP server: the model calls the fixture
/// tool once, then answers
async fn model_and_mcp() -> MockServer {
    // one server plays both the model endpoint and Thorium
    MockServer::start(Arc::new(|request: &Recorded, _| {
        // MCP traffic gets the MCP server's replies
        if request.path == "/api/mcp" {
            return mock::mcp(request, &mcp_result());
        }
        // the model calls the tool until a tool result comes back, then answers
        let answered = request.body["messages"].to_string().contains("tool_result");
        if answered {
            mock::anthropic_text(request, "final answer from io")
        } else {
            mock::anthropic_tool_use(request, TOOL, &json!({"sha256": "a".repeat(64)}))
        }
    }))
    .await
}

/// Run the harness binary with some arguments and env values, in a clean environment
///
/// # Arguments
///
/// * `args` - The harness arguments
/// * `env` - The env values to set
fn harness(args: &[String], env: &[(&str, String)]) -> i32 {
    // run it and keep only its exit code
    harness_output(args, env).0
}

/// Run the harness binary like `harness`, also returning everything it wrote to stdout and stderr
///
/// # Arguments
///
/// * `args` - The harness arguments
/// * `env` - The env values to set
fn harness_output(args: &[String], env: &[(&str, String)]) -> (i32, String) {
    // keep only PATH from the test's environment
    let mut command = Command::new(env!("CARGO_BIN_EXE_thorpal-harness"));
    command.env_clear().args(args);
    if let Ok(path) = std::env::var("PATH") {
        command.env("PATH", path);
    }
    // add the env values the test asks for
    for (name, value) in env {
        command.env(name, value);
    }
    // run it, showing its logs with the test's output
    let output = command.output().unwrap();
    let logs = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    eprintln!("{logs}");
    (output.status.code().unwrap_or(-1), logs)
}

/// Read a JSON file the harness wrote
///
/// # Arguments
///
/// * `path` - The file to read
fn read_json(path: &Path) -> Value {
    // a file the harness wrote must be JSON
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Get the plain text of a message's tool result content, which may be a string or text blocks
///
/// # Arguments
///
/// * `content` - The tool result's content
fn content_text(content: &Value) -> String {
    // join text blocks; a plain string is kept as is
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<&str>>()
            .join(""),
        other => other.to_string(),
    }
}

/// Find the tool result block in a request's messages
///
/// # Arguments
///
/// * `request` - The model request
fn tool_result(request: &Recorded) -> Value {
    // search every message's content blocks
    request.body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .find(|block| block["type"] == "tool_result")
        .cloned()
        .unwrap_or(Value::Null)
}

/// Poison values for every model env var a client library might read
///
/// # Arguments
///
/// * `url` - Where the poisoned base urls point
fn poisoned_env(url: &str) -> Vec<(&'static str, String)> {
    // every key, base url, and model a provider SDK reads
    vec![
        ("ANTHROPIC_API_KEY", "poison-anthropic-key".to_owned()),
        ("ANTHROPIC_BASE_URL", url.to_owned()),
        ("ANTHROPIC_MODEL", "poison-model".to_owned()),
        ("OPENAI_API_KEY", "poison-openai-key".to_owned()),
        ("OPENAI_BASE_URL", url.to_owned()),
        ("OPENAI_MODEL", "poison-model".to_owned()),
    ]
}

/// A run writes its answer, conversation, traces, and status and exits 0; the model endpoint in
/// the models file gets every model call with the fixture's key and model, the tools as the
/// renderers render them, and tool results as `encode_result` encodes them
#[tokio::test(flavor = "multi_thread")]
async fn run_calls_the_models_file_endpoint_and_writes_outputs() {
    // a fixture run pointed at the recording server for both the model and Thorium
    let server = model_and_mcp().await;
    let fixture = RunFixture::new(&server.url, &server.url);
    let dir = tempfile::tempdir().unwrap();
    let args = fixture.write(dir.path());
    // run the harness off the test's runtime
    let (code, logs) = tokio::task::spawn_blocking(move || harness_output(&args, &[]))
        .await
        .unwrap();
    assert_eq!(code, 0);
    // the answer and the status
    let answer = std::fs::read_to_string(dir.path().join("results")).unwrap();
    assert_eq!(answer, "final answer from io");
    let status = read_json(&dir.path().join("result-files/status.json"));
    assert_eq!(status["success"], Value::Bool(true), "{status}");
    assert_eq!(status["model_calls"], json!(2), "{status}");
    assert_eq!(status["tool_calls"], json!(1), "{status}");
    assert_eq!(status["input_tokens"], json!(22), "{status}");
    // the conversation holds the prompt and the answer
    let conversation =
        std::fs::read_to_string(dir.path().join("result-files/conversation.jsonl")).unwrap();
    let lines: Vec<Value> = conversation
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        conversation.contains("Describe the input file."),
        "{conversation}"
    );
    assert!(
        conversation.contains("final answer from io"),
        "{conversation}"
    );
    assert!(lines.len() >= 2, "{conversation}");
    // the traces are OTLP-JSON spans for both model calls and the tool call
    let traces = std::fs::read_to_string(dir.path().join("result-files/traces.jsonl")).unwrap();
    let spans: Vec<Value> = traces
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let names: Vec<&str> = spans
        .iter()
        .filter_map(|span| span["name"].as_str())
        .collect();
    assert_eq!(
        names.iter().filter(|name| **name == "model_call").count(),
        2,
        "{traces}"
    );
    assert_eq!(
        names.iter().filter(|name| **name == "tool_call").count(),
        1,
        "{traces}"
    );
    for span in &spans {
        assert_eq!(span["traceId"].as_str().map(str::len), Some(32), "{span}");
        assert_eq!(span["spanId"].as_str().map(str::len), Some(16), "{span}");
        assert!(span["startTimeUnixNano"].is_string(), "{span}");
    }
    // every model call went to the models file's endpoint with its key and model
    let calls = server.on("/v1/messages");
    assert_eq!(calls.len(), 2, "{:?}", server.requests);
    for call in &calls {
        assert_eq!(call.method, "POST");
        assert_eq!(call.headers.get("x-api-key").map(String::as_str), Some(KEY));
        assert_eq!(call.body["model"], Value::from(MODEL));
    }
    // the model key and the run token stay out of every output, log, and model request body
    for (name, text) in [
        ("answer", &answer),
        ("status.json", &status.to_string()),
        ("conversation.jsonl", &conversation),
        ("traces.jsonl", &traces),
        ("logs", &logs),
    ] {
        assert!(
            !text.contains(KEY),
            "the model key leaked into {name}: {text}"
        );
        assert!(
            !text.contains(TOKEN),
            "the run token leaked into {name}: {text}"
        );
    }
    for call in &calls {
        let body = call.body.to_string();
        assert!(
            !body.contains(KEY),
            "the model key leaked into a model request: {body}"
        );
        assert!(
            !body.contains(TOKEN),
            "the run token leaked into a model request: {body}"
        );
    }
    // the tool and its result went to the model as the renderers render them
    check_rendering(&fixture, &server, &calls);
}

/// Check that the fixture's tool was offered to the model as `render_tool` renders it, that its
/// call reached the MCP server with the run's token, and that its result came back to the model
/// as `encode_result` encodes it
///
/// # Arguments
///
/// * `fixture` - The run
/// * `server` - The server that recorded the run's requests
/// * `calls` - The model calls, in order
fn check_rendering(fixture: &RunFixture, server: &MockServer, calls: &[Recorded]) {
    // the tool was offered as the renderer renders it for the Anthropic API
    let entry: &HarnessToolEntry = fixture.tools.values().next().unwrap();
    let opts = RenderOptions {
        model_id: MODEL.to_owned(),
        ..RenderOptions::default()
    };
    let rendered = render_tool(entry, ModelApi::AnthropicMessages, &opts).unwrap();
    let offered = calls[0].body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == TOOL)
        .cloned()
        .unwrap();
    assert_eq!(offered["input_schema"], rendered.definition["input_schema"]);
    assert_eq!(offered["description"], rendered.definition["description"]);
    // the tool call reached the MCP server with the run's token, and its result came back to
    // the model as encode_result encodes it
    let mcp = server.on("/api/mcp");
    let call = mcp
        .iter()
        .find(|request| request.body["method"] == "tools/call")
        .unwrap();
    assert_eq!(call.body["params"]["name"], "get_file");
    assert_eq!(
        call.body["params"]["arguments"]["sha256"],
        Value::from("a".repeat(64))
    );
    assert_eq!(
        call.headers.get("authorization").map(String::as_str),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    // the result the model got is the encoded MCP result
    let result = ToolCallResult {
        call_id: "toolu_mock".to_owned(),
        function_name: TOOL.to_owned(),
        output: ToolCallOutput::Json(mcp_result()),
        is_error: false,
    };
    let encoded = encode_result(&result, entry, ModelApi::AnthropicMessages);
    let returned = tool_result(&calls[1]);
    assert_eq!(returned["tool_use_id"], "toolu_mock");
    assert_eq!(
        content_text(&returned["content"]),
        encoded["content"].as_str().unwrap()
    );
}

/// Model env vars in the harness's environment change nothing: the run still calls only the
/// models file's endpoint with its key and model, and nothing reaches the poisoned urls
#[tokio::test(flavor = "multi_thread")]
async fn model_env_vars_change_nothing() {
    // a server for the run and one that the poisoned env vars point at
    let server = model_and_mcp().await;
    let poison = MockServer::start(Arc::new(|request: &Recorded, _| {
        mock::anthropic_text(request, "poisoned answer")
    }))
    .await;
    let fixture = RunFixture::new(&server.url, &server.url);
    let dir = tempfile::tempdir().unwrap();
    let args = fixture.write(dir.path());
    let env = poisoned_env(&poison.url);
    // run the harness with the poisoned env
    let code = tokio::task::spawn_blocking(move || harness(&args, &env))
        .await
        .unwrap();
    assert_eq!(code, 0);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("results")).unwrap(),
        "final answer from io"
    );
    // only the models file's endpoint was called, with its key and model
    assert_eq!(poison.count(), 0, "{:?}", poison.requests);
    for call in server.on("/v1/messages") {
        assert_eq!(call.headers.get("x-api-key").map(String::as_str), Some(KEY));
        assert_eq!(call.body["model"], Value::from(MODEL));
    }
    // an unsupported model is still refused with the poisoned env
    let mut fixture = RunFixture::new(&server.url, &server.url);
    fixture.entry_model().api = ModelApi::OpenAiChat;
    let refused = tempfile::tempdir().unwrap();
    let args = fixture.write(refused.path());
    let env = poisoned_env(&poison.url);
    let code = tokio::task::spawn_blocking(move || harness(&args, &env))
        .await
        .unwrap();
    assert_ne!(code, 0);
    assert_eq!(poison.count(), 0, "{:?}", poison.requests);
}

/// Every model API and credential kind the harness doesn't declare exits non-zero before any
/// model call, with status.json naming it
#[tokio::test(flavor = "multi_thread")]
async fn unsupported_models_are_refused_before_any_model_call() {
    // a server that records anything the harness sends
    let server = model_and_mcp().await;
    let apis = [
        ModelApi::OpenAiChat,
        ModelApi::OpenAiResponses,
        ModelApi::BedrockConverse,
        ModelApi::BedrockInvoke,
        ModelApi::GeminiGenerateContent,
        ModelApi::GeminiInteractions,
        ModelApi::VertexRawPredict,
    ];
    let kinds = [
        CredentialKind::AwsAccessKey {
            access_key_id: "AKIAEXAMPLE".to_owned(),
        },
        CredentialKind::AwsAssumeRole {
            role_arn: "arn:aws:iam::123456789012:role/thorium".to_owned(),
            external_id: None,
            session_name: None,
            duration_seconds: None,
            source: AwsRoleSource::AccessKey(Uuid::nil()),
        },
        CredentialKind::AwsBearerToken,
        CredentialKind::GcpCredentialsJson {
            impersonate_service_account: None,
        },
        CredentialKind::AzureEntraClient {
            tenant_id: "tenant".to_owned(),
            client_id: "client".to_owned(),
            auth: AzureClientAuth::ClientSecret,
        },
    ];
    // build one refused run per undeclared API and per undeclared credential kind
    let mut cases: Vec<(RunFixture, String)> = Vec::new();
    for api in apis {
        let mut fixture = RunFixture::new(&server.url, &server.url);
        fixture.entry_model().api = api;
        cases.push((fixture, api.name()));
    }
    for kind in kinds {
        let mut fixture = RunFixture::new(&server.url, &server.url);
        let named = kind.cred_type().to_string();
        if let Some(credential) = fixture.entry_model().credential.as_mut() {
            credential.kind = kind;
        }
        cases.push((fixture, named));
    }
    for (fixture, named) in cases {
        let dir = tempfile::tempdir().unwrap();
        let args = fixture.write(dir.path());
        let code = tokio::task::spawn_blocking(move || harness(&args, &[]))
            .await
            .unwrap();
        // the harness exits non-zero and names the reason in status.json
        assert_ne!(code, 0, "{named}");
        let status = read_json(&dir.path().join("result-files/status.json"));
        assert_eq!(status["success"], Value::Bool(false), "{named}: {status}");
        let reason = status["reason"].as_str().unwrap_or_default();
        assert!(reason.contains(&named), "{named}: {reason}");
        assert_eq!(status["model_calls"], json!(0), "{named}: {status}");
        assert!(!dir.path().join("results").exists(), "{named}");
    }
    // no model call was ever made
    assert_eq!(server.count(), 0, "{:?}", server.requests);
}

/// A model endpoint that keeps failing ends the run at the run timeout with the reason in
/// status.json and a non-zero exit
#[tokio::test(flavor = "multi_thread")]
async fn provider_errors_end_at_the_run_timeout() {
    // an endpoint that always fails
    let server = MockServer::start(Arc::new(|_: &Recorded, _| {
        Reply::json(
            500,
            &json!({"type": "error", "error": {"type": "api_error", "message": "down"}}),
        )
    }))
    .await;
    let fixture = RunFixture::new(&server.url, &server.url);
    let dir = tempfile::tempdir().unwrap();
    let args = fixture.write(dir.path());
    // run with a short run timeout
    let started = std::time::Instant::now();
    let code = tokio::task::spawn_blocking(move || {
        harness(&args, &[("THORPAL_HARNESS_TIMEOUT_SECS", "2".to_owned())])
    })
    .await
    .unwrap();
    assert_ne!(code, 0);
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
    // the status names the timeout and no answer was written
    let status = read_json(&dir.path().join("result-files/status.json"));
    assert_eq!(status["success"], Value::Bool(false), "{status}");
    let reason = status["reason"].as_str().unwrap_or_default();
    assert!(reason.contains("timed out after 2 seconds"), "{reason}");
    assert!(!dir.path().join("results").exists());
    // the model endpoint was tried
    assert!(!server.on("/v1/messages").is_empty());
}
