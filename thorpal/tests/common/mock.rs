//! A recording HTTP server that stands in for an Anthropic endpoint and Thorium's MCP server

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// One request the server received
#[derive(Debug, Clone)]
pub struct Recorded {
    /// The request method
    pub method: String,
    /// The request path
    pub path: String,
    /// The request headers, by lowercase name
    pub headers: BTreeMap<String, String>,
    /// The request body as JSON (`null` when it isn't JSON)
    pub body: Value,
}

/// A reply the server sends
#[derive(Debug, Clone)]
pub struct Reply {
    /// The status code
    pub status: u16,
    /// The content type
    pub content_type: String,
    /// Extra headers
    pub headers: Vec<(String, String)>,
    /// The body
    pub body: String,
}

impl Reply {
    /// Build a JSON reply
    ///
    /// # Arguments
    ///
    /// * `status` - The status code
    /// * `body` - The JSON body
    pub fn json(status: u16, body: &Value) -> Self {
        // a JSON body with no extra headers
        Reply {
            status,
            content_type: "application/json".to_owned(),
            headers: Vec::new(),
            body: body.to_string(),
        }
    }
}

/// Picks the reply to a request from the request and how many requests came before it
pub type Responder = Arc<dyn Fn(&Recorded, usize) -> Reply + Send + Sync>;

/// A running recording server
pub struct MockServer {
    /// The server's base url
    pub url: String,
    /// Every request the server received, in order
    pub requests: Arc<Mutex<Vec<Recorded>>>,
}

impl MockServer {
    /// Start a server on a free local port
    ///
    /// # Arguments
    ///
    /// * `responder` - Picks each reply
    pub async fn start(responder: Responder) -> Self {
        // listen on any free port
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        // serve each connection on its own task
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let recorded = Arc::clone(&recorded);
                let responder = Arc::clone(&responder);
                tokio::spawn(async move {
                    let _ = serve(stream, recorded, responder).await;
                });
            }
        });
        MockServer { url, requests }
    }

    /// Get the requests received so far on one path
    ///
    /// # Arguments
    ///
    /// * `path` - The path to filter on
    pub fn on(&self, path: &str) -> Vec<Recorded> {
        // keep the requests to that path in order
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.path == path)
            .cloned()
            .collect()
    }

    /// Get how many requests were received in total
    pub fn count(&self) -> usize {
        // count every request on any path
        self.requests.lock().unwrap().len()
    }
}

/// Read one request from a connection, record it, and send the reply
///
/// # Arguments
///
/// * `stream` - The connection
/// * `recorded` - Where requests are recorded
/// * `responder` - Picks the reply
async fn serve(
    mut stream: TcpStream,
    recorded: Arc<Mutex<Vec<Recorded>>>,
    responder: Responder,
) -> std::io::Result<()> {
    // read until the end of the headers
    let mut raw = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        raw.extend_from_slice(&chunk[..read]);
        if let Some(end) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    // parse the request line and headers
    let head = String::from_utf8_lossy(&raw[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split(' ');
    let method = request_line.next().unwrap_or_default().to_owned();
    let path = request_line.next().unwrap_or_default().to_owned();
    let headers: BTreeMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    // read the rest of the body
    let length: usize = headers
        .get("content-length")
        .and_then(|length| length.parse().ok())
        .unwrap_or(0);
    while raw.len() < header_end + length {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..read]);
    }
    let body = serde_json::from_slice(&raw[header_end..]).unwrap_or(Value::Null);
    let request = Recorded {
        method,
        path,
        headers,
        body,
    };
    // record the request, then reply
    let index = {
        let mut recorded = recorded.lock().unwrap();
        recorded.push(request.clone());
        recorded.len() - 1
    };
    let reply = responder(&request, index);
    let mut response = format!(
        "HTTP/1.1 {} Mock\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reply.content_type,
        reply.body.len()
    );
    for (name, value) in &reply.headers {
        // writing into a String never fails
        let _ = write!(response, "{name}: {value}\r\n");
    }
    response.push_str("\r\n");
    response.push_str(&reply.body);
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

/// Build an Anthropic Messages reply with some content, streamed when the request asks for it
///
/// # Arguments
///
/// * `request` - The request being answered
/// * `content` - The message's content blocks
/// * `stop_reason` - Why the message stopped
pub fn anthropic(request: &Recorded, content: &[Value], stop_reason: &str) -> Reply {
    // the reply names the model the request asked for
    let model = request.body["model"].clone();
    // a request that doesn't stream gets the whole message
    if request.body["stream"] != Value::Bool(true) {
        return Reply::json(
            200,
            &json!({
                "id": "msg_mock",
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": content,
                "stop_reason": stop_reason,
                "stop_sequence": null,
                "usage": {"input_tokens": 11, "output_tokens": 7},
            }),
        );
    }
    // a streaming request gets the message as server-sent events
    let mut events = vec![(
        "message_start",
        json!({"type": "message_start", "message": {
            "id": "msg_mock", "type": "message", "role": "assistant", "model": model,
            "content": [], "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": 11, "output_tokens": 1},
        }}),
    )];
    for (index, block) in content.iter().enumerate() {
        // each block opens empty and arrives in deltas, the way the Messages API streams it
        let (start, deltas) = match block["type"].as_str() {
            Some("text") => (
                json!({"type": "text", "text": ""}),
                vec![json!({"type": "text_delta", "text": block["text"]})],
            ),
            // a thinking block streams its text, if any, then its whole signature
            Some("thinking") => {
                let mut deltas = Vec::new();
                if block["thinking"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty())
                {
                    deltas.push(json!({"type": "thinking_delta", "thinking": block["thinking"]}));
                }
                deltas.push(json!({"type": "signature_delta", "signature": block["signature"]}));
                (json!({"type": "thinking", "thinking": ""}), deltas)
            }
            // a redacted thinking block arrives whole in its start event
            Some("redacted_thinking") => (block.clone(), Vec::new()),
            _ => (
                json!({"type": "tool_use", "id": block["id"], "name": block["name"], "input": {}}),
                vec![
                    json!({"type": "input_json_delta", "partial_json": block["input"].to_string()}),
                ],
            ),
        };
        events.push((
            "content_block_start",
            json!({"type": "content_block_start", "index": index, "content_block": start}),
        ));
        for delta in deltas {
            events.push((
                "content_block_delta",
                json!({"type": "content_block_delta", "index": index, "delta": delta}),
            ));
        }
        events.push((
            "content_block_stop",
            json!({"type": "content_block_stop", "index": index}),
        ));
    }
    events.push((
        "message_delta",
        json!({"type": "message_delta", "delta": {"stop_reason": stop_reason, "stop_sequence": null},
               "usage": {"output_tokens": 7}}),
    ));
    events.push(("message_stop", json!({"type": "message_stop"})));
    let body = events
        .into_iter()
        .fold(String::new(), |mut body, (name, data)| {
            // writing into a String never fails
            let _ = write!(body, "event: {name}\ndata: {data}\n\n");
            body
        });
    Reply {
        status: 200,
        content_type: "text/event-stream".to_owned(),
        headers: Vec::new(),
        body,
    }
}

/// Build an Anthropic Messages reply that answers with text
///
/// # Arguments
///
/// * `request` - The request being answered
/// * `text` - The answer
pub fn anthropic_text(request: &Recorded, text: &str) -> Reply {
    // one text block that ends the turn
    anthropic(
        request,
        &[json!({"type": "text", "text": text})],
        "end_turn",
    )
}

/// Build an Anthropic Messages reply that calls a tool
///
/// # Arguments
///
/// * `request` - The request being answered
/// * `name` - The tool to call
/// * `input` - The tool call's input
pub fn anthropic_tool_use(request: &Recorded, name: &str, input: &Value) -> Reply {
    // one tool use block that asks for the tool's result
    anthropic(
        request,
        &[json!({"type": "tool_use", "id": "toolu_mock", "name": name, "input": input})],
        "tool_use",
    )
}

/// Answer one message of the MCP handshake and tool call the way Thorium's MCP server does
///
/// # Arguments
///
/// * `request` - The MCP request
/// * `result` - The structured result of every tool call
pub fn mcp(request: &Recorded, result: &Value) -> Reply {
    // a session ends with a DELETE
    if request.method == "DELETE" {
        return Reply::json(200, &json!({}));
    }
    let id = request.body["id"].clone();
    match request.body["method"].as_str() {
        // the handshake names a session
        Some("initialize") => {
            let mut reply = Reply::json(
                200,
                &json!({"jsonrpc": "2.0", "id": id, "result": {
                    "protocolVersion": "2025-03-26", "capabilities": {"tools": {}},
                    "serverInfo": {"name": "mock-thorium", "version": "1"},
                }}),
            );
            reply
                .headers
                .push(("mcp-session-id".to_owned(), "session-mock".to_owned()));
            reply
        }
        // a tool call returns the structured result with its text
        Some("tools/call") => Reply::json(
            200,
            &json!({"jsonrpc": "2.0", "id": id, "result": {
                "content": [{"type": "text", "text": result.to_string()}],
                "structuredContent": result,
            }}),
        ),
        // a notification gets no body
        _ => Reply {
            status: 202,
            content_type: "application/json".to_owned(),
            headers: Vec::new(),
            body: String::new(),
        },
    }
}
