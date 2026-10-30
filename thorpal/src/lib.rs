//! An agent with binary analysis powers.
// the agent/sub-agent tool futures nest deeply enough that computing their async
// state-machine layout overflows the default query depth limit; raise it
#![recursion_limit = "256"]

mod thorpal;

pub use thorpal::analyst::{AnalystAnswer, AnalystLink, PendingAsk};
pub use thorpal::anonymize::stage_anonymized_sample;
pub use thorpal::detect::detections::{ArtifactClass, HitSource};
pub use thorpal::events::{ExtensionTrigger, ThorPalEvent, ThorPalEventSender};
pub use thorpal::gate::{CredentialRefresher, PauseController, PauseKind, PauseStatus};
pub use thorpal::mcp::{McpServerConfig, ThoriumMcpAuth, ThoriumMcpResolver};
pub use thorpal::resume::{
    ChatHistory, ForumSnapshot, SessionSnapshot, SideHistory, TranscriptEntry, UsageTotals,
};
pub use thorpal::scrub::{defang, refang};
pub use thorpal::seed::SEED_AUTHOR as DETECTOR_AUTHOR;
pub use thorpal::session::{RunOutcome, Session, SessionConfig};
pub use thorpal::store::library::{LibraryEntry, ScriptLibrary, ScriptMeta};
pub use thorpal::store::redact::{LibraryRedactor, parse_token_file};
pub use thorpal::store::{
    AgentRecord, ExtraSampleMeta, SessionMeta, SessionStore, validate_btw_name,
    validate_session_name,
};
pub use thorpal::tool_logs::{SEED_ANALYSIS_TOOL, ToolLogLayer, ToolLogRouter};
pub use thorpal::tools::forum::{
    AnalystAsk, AnswerSource, AskOption, Comment, CommentNode, DEFAULT_FORUM_LINK_DEPTH,
    FindingProvenance, ForumContextMode, ForumState, ForumView, Kind, NodeProvenance, ParentRef,
    Post, PostWithThread, QuestionStatus, ReviewFlag, SearchHit, Stance, Task, TaskCompletion,
    USER_AUTHOR, match_ranges, search_terms,
};
pub use thorpal::tools::ghidra::{
    FunctionEdges, GhidraEdge, GhidraSession, GhidraSettings, default_ghidra_projects_dir,
    validate_projects_dir,
};
pub use thorpal::tools::registry;
pub use thorpal::transcript::{DetectorRunLine, TranscriptItem, TranscriptLine};
pub use thorpal::{
    AgentConfig, AiClient, AiProviderConfig, ThorPal, ThorPalBuilder, Workflow,
    is_interrupted_error,
};
