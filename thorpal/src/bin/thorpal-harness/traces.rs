//! Records a `ThorPal` run's events as the conversation, traces, and counts the harness writes

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt::Write;
use thorpal::{ThorPalEvent, TranscriptItem, TranscriptLine};

use crate::interface::HarnessStatus;

/// The OTLP status code of a span that succeeded
const STATUS_OK: u8 = 1;

/// The OTLP status code of a span that failed
const STATUS_ERROR: u8 = 2;

/// Get the current time in nanoseconds since the Unix epoch
fn now_nanos() -> u64 {
    // a clock before 1970 or past 2554 reads as zero
    chrono::Utc::now()
        .timestamp_nanos_opt()
        .and_then(|nanos| u64::try_from(nanos).ok())
        .unwrap_or_default()
}

/// Get a random id of a number of bytes as lowercase hex
///
/// # Arguments
///
/// * `bytes` - How many random bytes the id has
fn random_id(bytes: usize) -> String {
    // two hex digits per random byte
    (0..bytes).fold(String::with_capacity(bytes * 2), |mut id, _| {
        // writing into a String never fails
        let _ = write!(id, "{:02x}", rand::random::<u8>());
        id
    })
}

/// Join some lines, ending each in a newline
///
/// # Arguments
///
/// * `lines` - The lines to join
fn lines(lines: &[String]) -> String {
    // each line is followed by a newline
    lines
        .iter()
        .flat_map(|line| [line.as_str(), "\n"])
        .collect()
}

/// A tool call that started and hasn't finished
#[derive(Debug, Clone)]
struct OpenCall {
    /// The agent that made the call
    agent: String,
    /// The tool that was called
    tool: String,
    /// When the call started
    start: u64,
}

/// Everything recorded about a run so far
#[derive(Debug, Clone)]
pub struct Recorder {
    /// The trace every span of the run belongs to
    trace_id: String,
    /// The conversation's lines
    conversation: Vec<String>,
    /// The trace's spans
    spans: Vec<String>,
    /// The tool calls that haven't finished, by call id
    open: BTreeMap<String, OpenCall>,
    /// When the last model call ended, or the run started
    mark: u64,
    /// The counts written to the status file
    status: HarnessStatus,
}

impl Default for Recorder {
    /// A recorder for a run that just started
    fn default() -> Self {
        Recorder {
            trace_id: random_id(16),
            conversation: Vec::new(),
            spans: Vec::new(),
            open: BTreeMap::new(),
            mark: now_nanos(),
            status: HarnessStatus::default(),
        }
    }
}

impl Recorder {
    /// Add one span to the trace
    ///
    /// # Arguments
    ///
    /// * `name` - The span's name
    /// * `start` - When the span started
    /// * `attributes` - The span's attributes, by key
    /// * `ok` - Whether the span succeeded
    fn span(&mut self, name: &str, start: u64, attributes: &[(&str, Value)], ok: bool) {
        // OTLP-JSON attributes are typed key/value pairs
        let attributes: Vec<Value> = attributes
            .iter()
            .map(|(key, value)| {
                // numbers, flags, and strings keep their OTLP types
                let value = match value {
                    Value::Number(number) => json!({"intValue": number.to_string()}),
                    Value::Bool(flag) => json!({"boolValue": flag}),
                    Value::String(text) => json!({"stringValue": text}),
                    other => json!({"stringValue": other.to_string()}),
                };
                json!({"key": key, "value": value})
            })
            .collect();
        // the span ends now
        let span = json!({
            "traceId": self.trace_id,
            "spanId": random_id(8),
            "name": name,
            "kind": 1,
            "startTimeUnixNano": start.to_string(),
            "endTimeUnixNano": now_nanos().to_string(),
            "attributes": attributes,
            "status": {"code": if ok { STATUS_OK } else { STATUS_ERROR }},
        });
        self.spans.push(span.to_string());
    }

    /// Record one event of the run
    ///
    /// # Arguments
    ///
    /// * `event` - The event `ThorPal` emitted
    pub fn record(&mut self, event: &ThorPalEvent) {
        // every displayable event is a line of the conversation
        if let Some(item) = TranscriptItem::from_event(event)
            && let Ok(line) = serde_json::to_string(&TranscriptLine::stamped(item))
        {
            self.conversation.push(line);
        }
        // model calls, tool calls, and failures are spans
        match event {
            ThorPalEvent::UsageUpdated {
                agent,
                input,
                output,
                ..
            } => {
                // each usage update ends one model call, which started when the last one ended
                self.status.model_calls += 1;
                self.status.input_tokens += input;
                self.status.output_tokens += output;
                let start = self.mark;
                self.span(
                    "model_call",
                    start,
                    &[
                        ("thorpal.agent", json!(agent)),
                        ("gen_ai.usage.input_tokens", json!(input)),
                        ("gen_ai.usage.output_tokens", json!(output)),
                    ],
                    true,
                );
                self.mark = now_nanos();
            }
            ThorPalEvent::ToolCallStarted {
                agent,
                call_id,
                tool,
                ..
            } => {
                // a tool call's span ends when the call finishes
                self.status.tool_calls += 1;
                self.open.insert(
                    call_id.clone(),
                    OpenCall {
                        agent: agent.clone(),
                        tool: tool.clone(),
                        start: now_nanos(),
                    },
                );
            }
            ThorPalEvent::ToolCallFinished {
                call_id, success, ..
            } => {
                // close the span the call opened
                if let Some(call) = self.open.remove(call_id) {
                    self.span(
                        "tool_call",
                        call.start,
                        &[
                            ("thorpal.agent", json!(call.agent)),
                            ("gen_ai.tool.name", json!(call.tool)),
                            ("gen_ai.tool.call.id", json!(call_id)),
                        ],
                        *success,
                    );
                }
            }
            ThorPalEvent::TurnFailed { agent, error, .. } => {
                // a failed model call is a span with an error status
                let start = self.mark;
                self.span(
                    "model_call",
                    start,
                    &[("thorpal.agent", json!(agent)), ("error", json!(error))],
                    false,
                );
            }
            // every other event is only part of the conversation
            _ => (),
        }
    }

    /// Get the conversation as JSON lines
    #[must_use]
    pub fn conversation(&self) -> String {
        // one line per item, each ending in a newline
        lines(&self.conversation)
    }

    /// Get the trace as JSON lines of OTLP-JSON spans
    #[must_use]
    pub fn traces(&self) -> String {
        // one span per line, each ending in a newline
        lines(&self.spans)
    }

    /// Get the counts recorded so far
    #[must_use]
    pub fn status(&self) -> HarnessStatus {
        // the counts are kept as a status with no outcome yet
        self.status.clone()
    }
}
