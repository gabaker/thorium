//! The telemetry bundle: the serializable document an export writes.
//!
//! These are deliberate serde twins of the usage TUI's view models (which
//! carry no serde by design): every field is `#[serde(default)]`-tolerant so
//! receivers parse bundles from both older and newer exporters, and
//! `bundle_version` is the compatibility contract a reader gates on. The
//! privacy posture is structural: no field in this file is ever populated
//! from message text, tool arguments or results, forum prose, file names, or
//! paths -- the only strings are tool names, role names, enum vocabulary,
//! technique ids, version ids, timestamps, hashes, and (at level 1 only) the
//! normalized failure signatures of [`SignatureCluster`].

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use thorium::Error;
use thorpal::UsageTotals;

/// The bundle format version this build writes
///
/// Receivers refuse bundles from the future by name; additive field changes
/// do not bump this (every field tolerates absence), structural changes do.
pub const BUNDLE_VERSION: u32 = 1;

/// The failure-signature recipe this build normalizes with
///
/// Receivers cluster signatures on exact string equality, so signatures only
/// cluster within one recipe; bundles without a stamp carry recipe 1 (the
/// original single-line, 90-character form).
pub const SIGNATURE_RECIPE: u32 = 2;

/// One exported telemetry bundle: the exporter's stamp plus its sessions
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Bundle {
    /// The bundle format version, the compatibility contract readers gate on
    #[serde(default)]
    pub bundle_version: u32,
    /// The level this bundle was exported at: 0 content-free, 1 signatures
    #[serde(default)]
    pub level: u8,
    /// When this bundle was exported
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    /// The thorctl version that exported this bundle
    #[serde(default)]
    pub tool_version: String,
    /// The failure-signature recipe the exporter normalized with (0 means a
    /// recipe-1 exporter that predates the stamp; see [`Bundle::recipe`])
    #[serde(default)]
    pub signature_recipe: u32,
    /// The per-session telemetry rows
    #[serde(default)]
    pub sessions: Vec<SessionTelemetry>,
}

/// One session's telemetry: counts, totals, and tallies, never content
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionTelemetry {
    /// The hex SHA-256 of the session's name (names can be descriptive, so
    /// the name itself never ships)
    #[serde(default)]
    pub session_id: String,
    /// The analyzed sample's hex SHA-256, when the session staged an
    /// anonymized copy (hand-named projects cannot name their sample safely,
    /// so they ship no identity at all)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_sha256: Option<String>,
    /// Whether the session was built with anonymize-at-rest
    #[serde(default)]
    pub anonymized: bool,
    /// The model id the session ran against, when known
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The per-session harness version, from the newest tools-offered
    /// manifest line, when the session store carried one
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_version: Option<String>,
    /// When the session was created, from its metadata when readable
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    /// Wall-clock span between the first and last stamped transcript events,
    /// in milliseconds (absent for transcripts with no stamped lines)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// The merged busy time across the whole session, in milliseconds
    #[serde(default)]
    pub active_ms: u64,
    /// The session's effective token totals summed across every agent
    #[serde(default)]
    pub totals: UsageTotals,
    /// Per-role rollups keyed by role name (instances grouped offline)
    #[serde(default)]
    pub agents: BTreeMap<String, AgentRoleTelemetry>,
    /// Per-tool call, failure, and duration telemetry keyed by tool name
    #[serde(default)]
    pub tools: BTreeMap<String, ToolTelemetry>,
    /// The offered-tool roster per role, unioned across the session's
    /// tools-offered manifest lines (empty for stores that predate them)
    #[serde(default)]
    pub tools_offered: BTreeMap<String, Vec<String>>,
    /// The decision points the harness hit (question 7's tallies)
    #[serde(default)]
    pub decision_points: DecisionPoints,
    /// Provider-stall telemetry: retries, backoff time, and gate pauses
    #[serde(default)]
    pub provider_stalls: ProviderStalls,
    /// The forum's end-state counts
    #[serde(default)]
    pub forum: ForumTelemetry,
    /// Detector-lead outcomes per technique id (question 8's tally)
    #[serde(default)]
    pub detectors: BTreeMap<String, DetectorTally>,
    /// Detector-seeded question resolutions by status
    #[serde(default)]
    pub detector_questions: DetectorQuestions,
    /// One row per seeded detector lead, strongest confidence first (empty
    /// for bundles from exporters that predate per-lead rows)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub detector_leads: Vec<DetectorLead>,
    /// How many leads the per-session cap dropped from `detector_leads`
    #[serde(default, skip_serializing_if = "is_zero")]
    pub detector_leads_dropped: u64,
    /// The native detector roster's fleet tallies, keyed by registry name
    /// (empty for stores that predate `detectors_run` lines)
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub detectors_run: BTreeMap<String, DetectorRunTally>,
    /// Token totals bucketed by local hour, keyed `YYYY-MM-DDTHH`
    #[serde(default)]
    pub hours: BTreeMap<String, UsageTotals>,
    /// The normalized failure-signature clusters (level 1 only; level 0
    /// bundles always serialize this empty)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signatures: Vec<SignatureCluster>,
}

/// One role's rollup inside a session
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentRoleTelemetry {
    /// How many agent instances ran under this role
    #[serde(default)]
    pub instances: u64,
    /// The role's effective token total summed across its instances
    #[serde(default)]
    pub total_tokens: u64,
    /// How many tool calls the role's instances made
    #[serde(default)]
    pub tool_calls: u64,
}

/// One tool's call, failure, and duration telemetry inside a session
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolTelemetry {
    /// How many times the tool was called
    #[serde(default)]
    pub calls: u64,
    /// How many of those calls reported failure
    #[serde(default)]
    pub fails: u64,
    /// The median wall-clock call duration in milliseconds, when any call
    /// closed between stamped lines (legacy ts-less transcripts degrade to
    /// counts only)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50_ms: Option<u64>,
    /// The 95th-percentile wall-clock call duration in milliseconds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p95_ms: Option<u64>,
}

/// The decision points a session's harness hit, as countable tallies
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DecisionPoints {
    /// Analyst asks by answer source (`analyst`, `analyst_other`,
    /// `auto_default`, `declined`)
    #[serde(default)]
    pub asks: BTreeMap<String, u64>,
    /// Findings flagged for analyst review
    #[serde(default)]
    pub review_flags: u64,
    /// Turns abandoned after exhausting their retry budget
    #[serde(default)]
    pub turn_failed: u64,
    /// Sub-agent deaths by class (`max_turns`, `context`, `cancelled`,
    /// `other`)
    #[serde(default)]
    pub terminations: BTreeMap<String, u64>,
    /// Context-pressure warnings (level 1 events)
    #[serde(default)]
    pub context_pressure_warn: u64,
    /// Context-pressure criticals (level 2 events)
    #[serde(default)]
    pub context_pressure_critical: u64,
    /// Turn-budget extensions by trigger (`requested`, `reserved`,
    /// `reservation_applied`, `auto`)
    #[serde(default)]
    pub extensions: BTreeMap<String, u64>,
    /// Content-filter kills answered with a scrubbed retry
    #[serde(default)]
    pub llm_scrubbed: u64,
    /// Scrubbed requests whose retry episode ultimately succeeded
    #[serde(default)]
    pub scrub_rescued: u64,
    /// Report-generation ladder degradations
    #[serde(default)]
    pub report_degraded: u64,
    /// Resume barriers that continued an interrupted turn
    #[serde(default)]
    pub interrupted_resumes: u64,
    /// Resume barriers over a cleanly settled session
    #[serde(default)]
    pub clean_resumes: u64,
    /// Swarms that ran to completion
    #[serde(default)]
    pub swarms_finished: u64,
    /// Swarm members that succeeded, summed across swarms
    #[serde(default)]
    pub swarm_members_succeeded: u64,
    /// Swarm members that failed, summed across swarms
    #[serde(default)]
    pub swarm_members_failed: u64,
}

/// A session's provider-stall telemetry: retries, backoff, and gate pauses
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderStalls {
    /// Distinct retry episodes (requests that failed at least once)
    #[serde(default)]
    pub retry_episodes: u64,
    /// Failed transient attempts summed across episodes
    #[serde(default)]
    pub retry_attempts: u64,
    /// Total backoff time spent between attempts, in milliseconds
    #[serde(default)]
    pub retry_backoff_ms: u64,
    /// Episodes that ended without success (budget spent or fatal)
    #[serde(default)]
    pub retries_exhausted: u64,
    /// Gate pauses keyed by pause kind (`auth`, `retry_budget_exhausted`)
    #[serde(default)]
    pub pauses: BTreeMap<String, PauseTally>,
    /// Whether the transcript ends inside a pause (the process died paused)
    #[serde(default)]
    pub open_pause: bool,
}

/// One pause kind's tally inside a session
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PauseTally {
    /// How many pauses of this kind the session hit
    #[serde(default)]
    pub count: u64,
    /// Total time paused, in milliseconds, summed over resumed pauses
    #[serde(default)]
    pub total_paused_ms: u64,
    /// Automatic probes that failed, summed over resumed pauses
    #[serde(default)]
    pub probe_attempts: u64,
    /// Pauses the operator aborted instead of the provider recovering
    #[serde(default)]
    pub aborted: u64,
}

/// Whether a counter is zero, for skipping empty fields in the encoding
///
/// # Arguments
///
/// * `n` - The counter to test
fn is_zero(n: &u64) -> bool {
    // serde's skip_serializing_if needs a function, not a closure
    *n == 0
}

/// What kind of tracked forum entry a telemetry row describes
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// A tracked question (a post or a comment)
    #[default]
    Question,
    /// A completion-gate task
    Task,
    /// A kind this reader does not know (a newer exporter wrote it)
    #[serde(other)]
    Unknown,
}

/// A tracked entry's resolution status
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryStatus {
    /// A question resolved as solved
    Solved,
    /// A question left unsolved
    #[default]
    Unsolved,
    /// A question parked as needing more evidence
    NeedsEvidence,
    /// A question explicitly marked unanswerable with current tools
    Blocked,
    /// A task completed before the run ended
    Met,
    /// A task left open when the run ended
    Unmet,
    /// A status this reader does not know (a newer exporter wrote it)
    #[serde(other)]
    Unknown,
}

/// Who authored a tracked forum entry, as a class
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorClass {
    /// The reserved detector author (seeded entries)
    Detector,
    /// Any agent display name
    #[default]
    Agent,
    /// The reserved user author (analyst-planted entries)
    User,
    /// A class this reader does not know (a newer exporter wrote it)
    #[serde(other)]
    Unknown,
}

/// Whether a tracked entry hangs off a post or a comment
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// The entry's parent is a post
    #[default]
    Post,
    /// The entry's parent is a comment
    Comment,
    /// A kind this reader does not know (a newer exporter wrote it)
    #[serde(other)]
    Unknown,
}

/// One tracked question or task, in closed vocabulary only
///
/// Never a body, a description, or a non-detector post name; the one prose
/// field is `title`, which ships only at level 1 (redacted and individually
/// review-droppable) and is cleared outright at level 0.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForumEntry {
    /// Whether this entry is a question or a task
    #[serde(default)]
    pub kind: EntryKind,
    /// The entry's resolution status when the store was exported
    #[serde(default)]
    pub status: EntryStatus,
    /// Who authored the entry, as a class
    #[serde(default)]
    pub author: AuthorClass,
    /// The authoring agent's role (via the usage views' role grouping),
    /// agent-authored entries only
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The detector post-name class (`fn`, `file_level`, `runtime_noise`,
    /// a native question name, ...), detector-authored entries only
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_class: Option<String>,
    /// The sibling index of an `s<N>-` scoped name in a multi-sample session
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample: Option<u32>,
    /// The technique ids a detector-authored entry names
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub techniques: Vec<String>,
    /// The forum revision the entry last changed at (0 predates tracking)
    #[serde(default)]
    pub updated: u64,
    /// What the entry hangs off, for comment-questions and tasks
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<NodeKind>,
    /// Whether a detector-seeded entry was never engaged after seeding
    /// (`None` when the store's revisions predate tracking)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub untouched: Option<bool>,
    /// How many completion-gate tasks gate this question
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tasks: u64,
    /// How many of those tasks were left unmet
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tasks_unmet: u64,
    /// How many non-detector comments sit in the entry's subtree
    #[serde(default, skip_serializing_if = "is_zero")]
    pub replies: u64,
    /// The question's title (a task's description) -- level 1 only, redacted
    /// and review-droppable; always absent at level 0
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// A session's forum end-state, as counts and statuses only
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ForumTelemetry {
    /// How many posts the forum holds
    #[serde(default)]
    pub posts: u64,
    /// How many comments the forum holds
    #[serde(default)]
    pub comments: u64,
    /// How many posts and comments are questions
    #[serde(default)]
    pub questions: u64,
    /// Questions resolved as solved
    #[serde(default)]
    pub solved: u64,
    /// Questions left unsolved
    #[serde(default)]
    pub unsolved: u64,
    /// Questions parked as needing more evidence
    #[serde(default)]
    pub needs_evidence: u64,
    /// Questions explicitly marked unanswerable with current tools
    #[serde(default)]
    pub blocked: u64,
    /// Completion-gate tasks the forum tracked
    #[serde(default)]
    pub tasks: u64,
    /// Completion-gate tasks that never cleared
    #[serde(default)]
    pub tasks_unmet: u64,
    /// One row per tracked question and task, unresolved first (empty for
    /// bundles from exporters that predate per-entry rows)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<ForumEntry>,
    /// How many entries the per-session cap dropped from `entries`
    #[serde(default, skip_serializing_if = "is_zero")]
    pub entries_dropped: u64,
}

/// The seeding shape a detector lead arrived as
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeadShape {
    /// A per-finding comment under a seeded post
    #[default]
    Finding,
    /// A line of the suppressed runtime-noise rollup
    Rollup,
    /// A shape this reader does not know (a newer exporter wrote it)
    #[serde(other)]
    Unknown,
}

/// A lead's seeded confidence label
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeadConfidence {
    /// The detector asserted a fact
    Fact,
    /// More likely true than not
    Likely,
    /// Even odds
    Unsure,
    /// Needs validation before trusting
    Untrusted,
    /// A label this reader does not know (a newer exporter wrote it)
    #[serde(other)]
    Unknown,
}

/// How a lead's thread resolved
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeadVerdict {
    /// Supporting evidence landed on the lead's technique
    Confirmed,
    /// Disproving evidence landed (the false-positive signal)
    Disputed,
    /// The thread was engaged without a decisive stance
    Neutral,
    /// Zero engagement: the lead cost seeding tokens and bought nothing
    #[default]
    Ignored,
    /// A verdict this reader does not know (a newer exporter wrote it)
    #[serde(other)]
    Unknown,
}

/// One seeded detector lead's structured facts and outcome
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DetectorLead {
    /// The technique id the lead evidences
    #[serde(default)]
    pub technique: String,
    /// The seeded post's closed name class (`fn`, `file_level`, ...)
    #[serde(default)]
    pub post_class: String,
    /// The sibling index of an `s<N>-` scoped post in a multi-sample session
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample: Option<u32>,
    /// Whether the lead arrived as a finding comment or a rollup line
    #[serde(default)]
    pub shape: LeadShape,
    /// The seeded confidence label, when one was recorded or parsed
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<LeadConfidence>,
    /// The seeded suspicion score, when one was recorded or parsed
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspicion: Option<i64>,
    /// How many backing sites the finding listed, when known
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sites: Option<u64>,
    /// The recovered artifact's class key -- never its value
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    /// The producing analysis (`native`, `capa`, ...), structured sessions only
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The producing detection's registry name, native sources only (script
    /// names are analyst-influenced and stay local)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detection: Option<String>,
    /// How the lead's thread resolved for its technique
    #[serde(default)]
    pub verdict: LeadVerdict,
    /// How many non-detector comments engaged the lead's node
    #[serde(default, skip_serializing_if = "is_zero")]
    pub replies: u64,
    /// Whether the lead was never engaged after seeding (`None` when the
    /// store's revisions predate tracking)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub untouched: Option<bool>,
    /// Whether the facts came from structured provenance rather than parsed
    /// template text
    #[serde(default)]
    pub exact: bool,
}

/// One registry detection's fleet tally from the `detectors_run` roster lines
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DetectorRunTally {
    /// The ATT&CK technique id the detection evidences
    #[serde(default)]
    pub technique: String,
    /// How many seeding passes ran the detection
    #[serde(default)]
    pub runs: u64,
    /// How many hits the detection produced across those passes
    #[serde(default)]
    pub hits: u64,
    /// How many passes the detection failed in
    #[serde(default, skip_serializing_if = "is_zero")]
    pub failed: u64,
}

/// One technique id's detector-lead outcomes across a session's forum
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DetectorTally {
    /// Threads where supporting evidence landed on the lead
    #[serde(default)]
    pub confirmed: u64,
    /// Threads where disproving evidence landed (the false-positive signal)
    #[serde(default)]
    pub disputed: u64,
    /// Threads engaged without a supporting or disproving stance
    #[serde(default)]
    pub neutral: u64,
    /// Threads with zero engagement (seeding tokens bought nothing)
    #[serde(default)]
    pub ignored: u64,
    /// How many individual leads carried this technique
    #[serde(default, skip_serializing_if = "is_zero")]
    pub leads: u64,
    /// The leads' confidence histogram, keyed `fact`/`likely`/`unsure`/
    /// `untrusted`/`unrated`
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub confidence: BTreeMap<String, u64>,
}

/// Detector-seeded question resolutions, tallied by status
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DetectorQuestions {
    /// Detector questions resolved as solved
    #[serde(default)]
    pub solved: u64,
    /// Detector questions explicitly marked unanswerable
    #[serde(default)]
    pub blocked: u64,
    /// Detector questions left unsolved
    #[serde(default)]
    pub unsolved: u64,
    /// Detector questions parked as needing more evidence
    #[serde(default)]
    pub needs_evidence: u64,
}

/// One normalized failure-signature cluster (level 1 only)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SignatureCluster {
    /// The tool whose failing results normalized to this signature
    #[serde(default)]
    pub tool: String,
    /// The normalized signature (see [`super::sig::normalize_signature`])
    #[serde(default)]
    pub sig: String,
    /// How many failing results normalized to this signature
    #[serde(default)]
    pub count: u64,
    /// Whether one agent hit this signature three-plus times consecutively
    /// on one tool -- the stuck-tool-loop flag
    #[serde(default, rename = "loop")]
    pub looped: bool,
}

impl Bundle {
    /// The signature recipe this bundle's signatures were normalized with
    ///
    /// Unstamped bundles predate the stamp and carry recipe 1.
    pub fn recipe(&self) -> u32 {
        // the stamp field defaults to 0 on legacy bundles
        if self.signature_recipe == 0 {
            1
        } else {
            self.signature_recipe
        }
    }

    /// Render this bundle as the exact pretty-printed bytes a file holds
    ///
    /// The preview-is-the-bundle contract hangs off this: the preview screen
    /// renders these bytes, and the writer writes these bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>, Error> {
        // pretty-print so the preview and the file stay human-readable
        let mut encoded = serde_json::to_vec_pretty(self)
            .map_err(|err| Error::new(format!("failed to encode the bundle: {err}")))?;
        // end the document with a newline like every other text artifact
        encoded.push(b'\n');
        Ok(encoded)
    }

    /// Write already-rendered bundle bytes to a path, gzipping on `.gz`
    ///
    /// Takes the bytes rather than re-encoding so the written file is
    /// byte-identical to the confirmed preview.
    ///
    /// # Arguments
    ///
    /// * `bytes` - The exact rendered bundle bytes to write
    /// * `path` - The path to write to (a `.gz` suffix gzips the same bytes)
    pub fn write_bytes(bytes: &[u8], path: &Path) -> Result<(), Error> {
        // gzip when the target name opts in, otherwise write the bytes as-is
        let payload = if path.extension().is_some_and(|ext| ext == "gz") {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder
                .write_all(bytes)
                .and_then(|()| encoder.finish())
                .map_err(|err| Error::new(format!("failed to gzip the bundle: {err}")))?
        } else {
            bytes.to_vec()
        };
        // write the whole document in one call
        std::fs::write(path, payload)
            .map_err(|err| Error::new(format!("failed to write {}: {err}", path.display())))
    }

    /// Read a bundle file back, sniffing gzip by its magic bytes
    ///
    /// # Arguments
    ///
    /// * `path` - The bundle file to read
    pub fn read(path: &Path) -> Result<Bundle, Error> {
        // read the raw file once
        let raw = std::fs::read(path)
            .map_err(|err| Error::new(format!("failed to read {}: {err}", path.display())))?;
        // gunzip when the magic bytes say so, whatever the file is named
        let decoded = if raw.starts_with(&[0x1f, 0x8b]) {
            let mut decoder = flate2::read::GzDecoder::new(raw.as_slice());
            let mut plain = Vec::new();
            std::io::Read::read_to_end(&mut decoder, &mut plain)
                .map_err(|err| Error::new(format!("failed to gunzip {}: {err}", path.display())))?;
            plain
        } else {
            raw
        };
        // parse and gate on the version contract
        let bundle: Bundle = serde_json::from_slice(&decoded)
            .map_err(|err| Error::new(format!("failed to parse {}: {err}", path.display())))?;
        if bundle.bundle_version > BUNDLE_VERSION {
            return Err(Error::new(format!(
                "{} is bundle_version {} but this thorctl reads up to {BUNDLE_VERSION}; \
                 upgrade thorctl to explore it",
                path.display(),
                bundle.bundle_version
            )));
        }
        Ok(bundle)
    }
}

/// One merged session row plus the harness version that produced it
///
/// The explorer splits fleets by version: the per-session manifest's
/// `harness_version` is authoritative when the store carried one, and the
/// exporting thorctl's `tool_version` stands in for older sessions.
#[derive(Debug, Clone)]
pub struct MergedSession {
    /// The session's telemetry row
    pub row: SessionTelemetry,
    /// The version this row's fleet splits group under
    pub version: String,
    /// The signature recipe the row's bundle was normalized with
    pub recipe: u32,
}

/// Merges bundles into deduplicated session rows
///
/// The same session can appear in several bundles (re-exports, overlapping
/// stores); rows deduplicate by `(session_id, created_at)` -- the name hash
/// alone would conflate two analysts' different sessions that merely share a
/// name -- keeping the row from the newest-exported bundle so corrections win.
///
/// # Arguments
///
/// * `bundles` - The parsed bundles to merge
pub fn merge(bundles: &[Bundle]) -> Vec<MergedSession> {
    // walk newest-exported first so the first row per key wins the dedupe
    let mut order: Vec<&Bundle> = bundles.iter().collect();
    order.sort_by_key(|bundle| std::cmp::Reverse(bundle.created_at));
    let mut seen: std::collections::BTreeSet<(String, Option<DateTime<Utc>>)> =
        std::collections::BTreeSet::new();
    let mut merged = Vec::new();
    for bundle in order {
        for row in &bundle.sessions {
            // the first (newest) occurrence of a session identity is the
            // keeper; the creation stamp splits same-named sessions apart
            if !seen.insert((row.session_id.clone(), row.created_at)) {
                continue;
            }
            // prefer the session's own harness version over the exporter's
            let version = row
                .harness_version
                .clone()
                .unwrap_or_else(|| bundle.tool_version.clone());
            merged.push(MergedSession {
                row: row.clone(),
                version,
                recipe: bundle.recipe(),
            });
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// Builds a small two-signature bundle for the round-trip tests
    fn sample_bundle() -> Bundle {
        // one session with a couple of populated corners is plenty
        let mut session = SessionTelemetry {
            session_id: "a".repeat(64),
            anonymized: true,
            model: Some("claude-opus-5-5".to_owned()),
            active_ms: 1_000,
            ..SessionTelemetry::default()
        };
        session.tools.insert(
            "run_script".to_owned(),
            ToolTelemetry {
                calls: 212,
                fails: 17,
                p50_ms: Some(1_410),
                p95_ms: Some(4_200),
            },
        );
        session.signatures.push(SignatureCluster {
            tool: "run_script".to_owned(),
            sig: "script error: line N: unknown fn 'N'".to_owned(),
            count: 11,
            looped: true,
        });
        Bundle {
            bundle_version: BUNDLE_VERSION,
            level: 1,
            created_at: None,
            tool_version: "test".to_owned(),
            signature_recipe: SIGNATURE_RECIPE,
            sessions: vec![session],
        }
    }

    /// A bundle round-trips through its own bytes, plain and gzipped alike
    #[test]
    fn bundles_round_trip_plain_and_gzipped() {
        let bundle = sample_bundle();
        let bytes = bundle.to_bytes().expect("bundle should encode");
        // the loop flag serializes under its reserved-word wire name
        let text = String::from_utf8(bytes.clone()).expect("utf8");
        assert!(text.contains("\"loop\": true"));
        // a plain write reads back intact
        let dir = tempfile::tempdir().expect("tempdir");
        let plain = dir.path().join("bundle.json");
        Bundle::write_bytes(&bytes, &plain).expect("plain write");
        let back = Bundle::read(&plain).expect("plain read");
        assert_eq!(back.sessions.len(), 1);
        assert_eq!(back.sessions[0].tools["run_script"].calls, 212);
        assert!(back.sessions[0].signatures[0].looped);
        // a .gz write gzips the same bytes and reads back identically
        let gz = dir.path().join("bundle.json.gz");
        Bundle::write_bytes(&bytes, &gz).expect("gz write");
        let raw = std::fs::read(&gz).expect("read gz");
        assert!(
            raw.starts_with(&[0x1f, 0x8b]),
            "the .gz file must be gzipped"
        );
        let back = Bundle::read(&gz).expect("gz read");
        assert_eq!(back.sessions[0].tools["run_script"].fails, 17);
    }

    /// A sparse bundle from a different exporter still parses via defaults
    #[test]
    fn sparse_bundles_parse_via_defaults() {
        // a minimal document missing almost every key
        let sparse = r#"{"bundle_version":1,"sessions":[{"session_id":"ff"}]}"#;
        let bundle: Bundle = serde_json::from_str(sparse).expect("sparse should parse");
        assert_eq!(bundle.level, 0);
        // an unstamped bundle reads as the original recipe
        assert_eq!(bundle.recipe(), 1);
        assert_eq!(bundle.sessions[0].session_id, "ff");
        assert!(bundle.sessions[0].signatures.is_empty());
    }

    /// Merging dedupes by session id, newest bundle first, versions split
    #[test]
    fn merge_dedupes_by_newest_and_prefers_harness_versions() {
        // the same session exported twice: the newer bundle's row must win
        let shared_id = "e".repeat(64);
        let created = Some(chrono::Utc.with_ymd_and_hms(2025, 12, 1, 0, 0, 0).unwrap());
        let mut old_row = SessionTelemetry {
            session_id: shared_id.clone(),
            created_at: created,
            ..SessionTelemetry::default()
        };
        old_row.totals.total_tokens = 1;
        let mut new_row = SessionTelemetry {
            session_id: shared_id.clone(),
            created_at: created,
            // the newer export's store carried a manifest version
            harness_version: Some("0.2.0".to_owned()),
            ..SessionTelemetry::default()
        };
        new_row.totals.total_tokens = 2;
        // a different analyst's session that merely shares the name hash: a
        // different creation stamp keeps it a distinct row, never deduped
        let twin = SessionTelemetry {
            session_id: shared_id.clone(),
            created_at: Some(chrono::Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap()),
            ..SessionTelemetry::default()
        };
        // a second session only the old bundle holds
        let lonely = SessionTelemetry {
            session_id: "f".repeat(64),
            ..SessionTelemetry::default()
        };
        let old_bundle = Bundle {
            bundle_version: BUNDLE_VERSION,
            level: 0,
            created_at: Some(chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()),
            tool_version: "0.1.0".to_owned(),
            signature_recipe: SIGNATURE_RECIPE,
            sessions: vec![old_row, lonely, twin],
        };
        let new_bundle = Bundle {
            bundle_version: BUNDLE_VERSION,
            level: 0,
            created_at: Some(chrono::Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap()),
            tool_version: "0.1.5".to_owned(),
            signature_recipe: SIGNATURE_RECIPE,
            sessions: vec![new_row],
        };
        // merge in either order: the newest-exported row wins the dedupe,
        // and the same-named twin survives on its distinct creation stamp
        let merged = merge(&[old_bundle, new_bundle]);
        assert_eq!(merged.len(), 3);
        let shared = merged
            .iter()
            .find(|session| {
                session.row.session_id == shared_id && session.row.created_at == created
            })
            .expect("the shared session survives");
        assert_eq!(shared.row.totals.total_tokens, 2, "the newer row wins");
        // the manifest version outranks the exporter's tool version
        assert_eq!(shared.version, "0.2.0");
        // a manifest-less row falls back to its bundle's exporter version
        let lonely = merged
            .iter()
            .find(|session| session.row.session_id.starts_with('f'))
            .expect("the lonely session survives");
        assert_eq!(lonely.version, "0.1.0");
    }

    /// Future bundle versions are refused by name instead of misread
    #[test]
    fn future_bundle_versions_are_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("future.json");
        std::fs::write(
            &path,
            format!(r#"{{"bundle_version":{}}}"#, BUNDLE_VERSION + 1),
        )
        .expect("write");
        let err = Bundle::read(&path).expect_err("a future bundle must be refused");
        assert!(
            err.to_string()
                .contains(&format!("bundle_version {}", BUNDLE_VERSION + 1))
        );
    }
}
