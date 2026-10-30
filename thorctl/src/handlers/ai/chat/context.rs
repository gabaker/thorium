//! The shared chat history the ThorPal chat tui renders

use std::sync::{Arc, Mutex, MutexGuard};

use uuid::Uuid;

/// A single entry in a chat's history
#[derive(Debug, Clone)]
pub enum ChatEntry {
    /// A message from the user
    User(String),
    /// A message from the AI
    Ai {
        /// The display name of the agent this message came from, if known
        ///
        /// `None` for unattributed messages (e.g. progress notes); the workers
        /// fill it with the agent's name (e.g. `"main"` or
        /// `"ReverseEngineer(Elara)"`) so the UI can show who produced it.
        agent: Option<String>,
        /// The message text
        text: String,
    },
    /// A failure surfaced to the user with click-to-expand detail
    Failure {
        /// The id a click uses to toggle this failure's detail rows
        id: Uuid,
        /// The display name of the agent that hit the failure, if known
        agent: Option<String>,
        /// The compact one-line summary shown in the log
        summary: String,
        /// The provider's request id for the failed request, when preserved
        request_id: Option<String>,
        /// The full error text behind the summary, when it says more
        detail: Option<String>,
    },
    /// A tool call made by an agent
    ToolCall {
        /// The id used to match this call with its result
        id: Uuid,
        /// The display name of the agent that made this call, if known
        agent: Option<String>,
        /// The name of the tool that was called
        name: String,
        /// The arguments the tool was called with if they were a JSON object
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    },
    /// A captured log line streamed by a running tool call
    ToolLog {
        /// The id of the tool call this line belongs to
        id: Uuid,
        /// The captured single log line
        line: String,
    },
    /// The result of a tool call
    ToolResult {
        /// The id of the tool call this is a result for
        id: Uuid,
        /// The result of the tool call
        result: String,
        /// Whether the tool call succeeded, if known
        ///
        /// Filled from the run event so the UI can flag a failed call (e.g. a
        /// red status marker).
        success: Option<bool>,
    },
}

/// A chat history shared between the tui and the workers that drive ThorPal
#[derive(Debug, Clone, Default)]
pub struct ChatContext(Arc<Mutex<Vec<ChatEntry>>>);

impl ChatContext {
    /// Add an entry to this chat's history
    ///
    /// # Arguments
    ///
    /// * `entry` - The entry to add
    fn push(&self, entry: ChatEntry) {
        // get a guard to our shared history and add this entry
        self.access().push(entry);
    }

    /// Add a message from the user
    ///
    /// # Arguments
    ///
    /// * `msg` - The message the user sent
    pub fn add_user(&self, msg: impl Into<String>) {
        // add this user message to our history
        self.push(ChatEntry::User(msg.into()));
    }

    /// Add a message from the AI with no agent attribution
    ///
    /// # Arguments
    ///
    /// * `msg` - The message the AI sent
    pub fn add_ai(&self, msg: impl Into<String>) {
        // add this AI message to our history unattributed
        self.add_ai_from(msg, None);
    }

    /// Add a message from the AI attributed to a specific agent
    ///
    /// Like [`add_ai`](Self::add_ai) but records which agent produced the
    /// message so the UI can label it with the main agent or sub-agent name.
    ///
    /// # Arguments
    ///
    /// * `msg` - The message the AI sent
    /// * `agent` - The display name of the agent that produced this message
    pub fn add_ai_from(&self, msg: impl Into<String>, agent: Option<String>) {
        // add this AI message to our history, tagged with the producing agent
        self.push(ChatEntry::Ai {
            agent,
            text: msg.into(),
        });
    }

    /// Add a failure with click-to-expand detail to the chat log
    ///
    /// The summary renders as a compact error line; the request id and full
    /// error text (when present) stay behind a click so provider bodies never
    /// flood the log.
    ///
    /// # Arguments
    ///
    /// * `summary` - The compact one-line summary of the failure
    /// * `agent` - The display name of the agent that hit the failure, if known
    /// * `request_id` - The provider's request id, when the error preserved one
    /// * `detail` - The full error text, when it says more than the summary
    pub fn add_failure(
        &self,
        summary: impl Into<String>,
        agent: Option<String>,
        request_id: Option<String>,
        detail: Option<String>,
    ) {
        // failures mint their own id so clicks can toggle their detail rows
        self.push(ChatEntry::Failure {
            id: Uuid::new_v4(),
            agent,
            summary: summary.into(),
            request_id,
            detail,
        });
    }

    /// Add a tool call made by an agent
    ///
    /// # Arguments
    ///
    /// * `id` - The id used to match this call with its result
    /// * `agent` - The display name of the agent that made the call, if known
    /// * `name` - The name of the tool that was called
    /// * `arguments` - The arguments the tool was called with if they were a JSON object
    pub fn add_tool_call(
        &self,
        id: Uuid,
        agent: Option<String>,
        name: impl Into<String>,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) {
        // add this tool call to our history, tagged with the calling agent
        self.push(ChatEntry::ToolCall {
            id,
            agent,
            name: name.into(),
            arguments,
        });
    }

    /// Add a log line streamed by a running tool call
    ///
    /// Log entries stay in the shared history for the life of the chat because
    /// the message log folds entries incrementally by index; the per-tool
    /// render buffer is capped, so the cost here is one small `String` per
    /// captured line (bounded in practice by the capture layer's target
    /// filter). Flagged for a future compaction pass if runs get chattier.
    ///
    /// # Arguments
    ///
    /// * `id` - The id of the tool call this line belongs to
    /// * `line` - The captured single log line
    pub fn add_tool_log(&self, id: Uuid, line: impl Into<String>) {
        // add this log line to our history under its tool call's id
        self.push(ChatEntry::ToolLog {
            id,
            line: line.into(),
        });
    }

    /// Add the result of a tool call
    ///
    /// # Arguments
    ///
    /// * `id` - The id of the tool call this is a result for
    /// * `result` - The result of the tool call
    /// * `success` - Whether the tool call succeeded, if known
    pub fn add_tool_result(&self, id: Uuid, result: impl Into<String>, success: Option<bool>) {
        // add this tool result to our history with its observed status
        self.push(ChatEntry::ToolResult {
            id,
            result: result.into(),
            success,
        });
    }

    /// Clear this chat's history
    pub fn clear(&self) {
        // get a guard to our shared history and drop every entry
        self.access().clear();
    }

    /// Get a guard to this chat's history
    pub fn access(&self) -> MutexGuard<'_, Vec<ChatEntry>> {
        // recover the history even if a writer panicked while holding the lock
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
