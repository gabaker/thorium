//! The telemetry fold: the counters a bundle ships, gathered in one pass.
//!
//! The usage scanner's `SessionFold` owns the single streaming pass over
//! `transcript.jsonl`; when a scan gathers telemetry it carries a
//! [`TelemetryFold`] alongside its usage counters and hands every line to
//! [`TelemetryFold::observe`]. A separate read-only pass over `forum.json`
//! ([`read_forum`]) tallies the discussion's end-state and the detector-lead
//! outcomes, and [`assemble_session`] joins both with the usage summary into
//! the bundle's [`SessionTelemetry`] row. Nothing here reads message text
//! into the output: failing tool results are reduced to normalized
//! signatures (held for level 1), and forum bodies are parsed only for
//! detector technique ids.

use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use thorpal::{
    ArtifactClass, Comment, DETECTOR_AUTHOR, ForumSnapshot, HitSource, Kind, NodeProvenance,
    ParentRef, PauseKind, Post, QuestionStatus, SessionMeta, Stance, TranscriptItem, USER_AUTHOR,
    UsageTotals,
};

use super::bundle::{
    AgentRoleTelemetry, AuthorClass, DecisionPoints, DetectorLead, DetectorQuestions,
    DetectorRunTally, DetectorTally, EntryKind, EntryStatus, ForumEntry, ForumTelemetry,
    LeadConfidence, LeadShape, LeadVerdict, NodeKind, PauseTally, ProviderStalls, SessionTelemetry,
    SignatureCluster, ToolTelemetry,
};
use super::sig::normalize_signature;
use crate::handlers::ai::usage::model::{SessionSummary, role_of, token_sum};

/// How many consecutive identical failure signatures flag a stuck tool loop
const LOOP_RUN: u32 = 3;

/// The native ATT&CK technique-id shape (`T1027`, `T1497.003`)
///
/// Ids outside this shape are minted by capa rules, YARA matches, or library
/// detector scripts, so they get the same export scrutiny as level-1 strings:
/// deny-token redaction plus per-id drops on the review screen.
static NATIVE_TECHNIQUE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^T\d{4}(\.\d{3})?$").expect("a valid technique regex"));

/// A detector comment's opening `[<id> <name>]` template line
static COMMENT_OPENER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[([A-Za-z0-9][A-Za-z0-9._\-]{1,31}) ").expect("a valid opener regex")
});

/// A detector rollup's `- <id> @ <addr>:` lines
static ROLLUP_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^-\s+([A-Za-z0-9][A-Za-z0-9._\-]{1,31}) @ ").expect("a valid rollup regex")
});

/// A detector post's `Techniques: a, b.` list line
static TECHNIQUES_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^Techniques:\s*(.+)$").expect("a valid list regex"));

/// A bare technique-id token inside a `Techniques:` list
static ID_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._\-]{1,31}$").expect("a valid token regex")
});

/// Whether a technique id is a native ATT&CK id (safe at level 0 as-is)
///
/// # Arguments
///
/// * `id` - The technique id to classify
pub fn is_native_technique(id: &str) -> bool {
    // native ids follow the fixed Txxxx(.yyy) shape exactly
    NATIVE_TECHNIQUE.is_match(id)
}

/// The telemetry-only counters gathered alongside the usage fold's pass
///
/// Allocated only when a scan gathers telemetry, so the usage TUI's folds pay
/// nothing. Signature text is held in memory only; whether it ships is the
/// export's level choice.
#[derive(Debug, Clone, Default)]
pub struct TelemetryFold {
    /// Per-tool call tallies
    calls: BTreeMap<String, u64>,
    /// Per-tool failure tallies
    fails: BTreeMap<String, u64>,
    /// Per-tool closed-call durations in milliseconds
    durations: BTreeMap<String, Vec<u64>>,
    /// In-flight calls' start stamps, keyed by call id
    pending: HashMap<String, Option<u64>>,
    /// Analyst asks by answer source
    asks: BTreeMap<String, u64>,
    /// Review flags raised
    review_flags: u64,
    /// Turns abandoned after their retry budget
    turn_failed: u64,
    /// Sub-agent deaths by class
    terminations: BTreeMap<String, u64>,
    /// Context-pressure warnings (level 1)
    pressure_warn: u64,
    /// Context-pressure criticals (level 2)
    pressure_critical: u64,
    /// Turn-budget extensions by trigger
    extensions: BTreeMap<String, u64>,
    /// Content-filter kills answered with a scrubbed retry
    scrubbed: u64,
    /// The retry episodes that carried a scrub
    scrub_episodes: BTreeSet<String>,
    /// Report-generation ladder degradations
    report_degraded: u64,
    /// Resume barriers that continued an interrupted turn
    interrupted_resumes: u64,
    /// Resume barriers over a cleanly settled session
    clean_resumes: u64,
    /// Swarms that ran to completion
    swarms_finished: u64,
    /// Swarm members that succeeded, summed across swarms
    swarm_ok: u64,
    /// Swarm members that failed, summed across swarms
    swarm_failed: u64,
    /// Every retry episode id seen on any retry, resolution, or scrub line
    episodes: BTreeSet<String>,
    /// Failed transient attempts summed across episodes
    retry_attempts: u64,
    /// Total backoff time between attempts, in milliseconds
    retry_backoff_ms: u64,
    /// Each resolved episode's outcome, keyed by episode id
    resolved: BTreeMap<String, bool>,
    /// Gate pauses by kind
    pauses: BTreeMap<String, PauseTally>,
    /// Pause-start lines seen, for the open-pause flag
    paused_lines: u64,
    /// Pause-end lines seen, for the open-pause flag
    resumed_lines: u64,
    /// The resume-segment counter scoping per-process retry episode ids
    ///
    /// Episode ids restart per process (`main#0` again after every resume),
    /// so every episode key is prefixed with the count of restore barriers
    /// seen so far -- otherwise a resumed session's episodes collide, folding
    /// two different outcomes under one key.
    segment: u64,
    /// The offered-tool roster per role, unioned across manifest lines
    offered: BTreeMap<String, BTreeSet<String>>,
    /// The newest manifest's harness version
    harness_version: Option<String>,
    /// The native detector roster's tallies from `detectors_run` lines
    detectors_run: BTreeMap<String, DetectorRunTally>,
    /// Signature clusters keyed by (tool, signature)
    clusters: BTreeMap<(String, String), ClusterFold>,
    /// Each (agent, tool)'s last signature and its consecutive run length
    last_sig: HashMap<(String, String), (String, u32)>,
}

/// One signature cluster's in-progress tally
#[derive(Debug, Clone, Default)]
struct ClusterFold {
    /// How many failing results normalized to this signature
    count: u64,
    /// Whether any agent hit the loop threshold on this signature
    looped: bool,
}

/// A pause kind's bundle key: its snake_case wire name
///
/// Reuses the serde encoding so the bundle's keys can never drift from the
/// transcript's own `pause` field values.
///
/// # Arguments
///
/// * `kind` - The pause kind to key
fn pause_key(kind: &PauseKind) -> String {
    // the serde rename is the single source of truth for the wire name
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

impl TelemetryFold {
    /// Folds one transcript line into the telemetry counters
    ///
    /// # Arguments
    ///
    /// * `item` - The transcript item to fold
    /// * `ts` - The line's sanitized stamp, when it carried a usable one
    pub fn observe(&mut self, item: &TranscriptItem, ts: Option<u64>) {
        // tally only what the bundle ships; everything else is a no-op here
        match item {
            // a tool call opens its duration interval and counts
            TranscriptItem::ToolCall { call_id, tool, .. } => {
                *self.calls.entry(tool.clone()).or_default() += 1;
                self.pending.insert(call_id.clone(), ts);
            }
            // a tool result closes the interval and clusters failures
            TranscriptItem::ToolResult {
                agent,
                call_id,
                tool,
                result,
                success,
            } => {
                // close the call's duration when both ends were stamped
                if let (Some(Some(start)), Some(end)) = (self.pending.remove(call_id), ts) {
                    self.durations
                        .entry(tool.clone())
                        .or_default()
                        .push(end.saturating_sub(start));
                }
                if !success {
                    *self.fails.entry(tool.clone()).or_default() += 1;
                    // reduce the failing result to its cluster signature; the
                    // result text itself is never retained
                    let sig = normalize_signature(result);
                    let cluster = self
                        .clusters
                        .entry((tool.clone(), sig.clone()))
                        .or_default();
                    cluster.count += 1;
                    // extend or reset this agent+tool's consecutive run
                    let run = self
                        .last_sig
                        .entry((agent.clone(), tool.clone()))
                        .and_modify(|(last, run)| {
                            if *last == sig {
                                *run += 1;
                            } else {
                                *last = sig.clone();
                                *run = 1;
                            }
                        })
                        .or_insert_with(|| (sig.clone(), 1))
                        .1;
                    if run >= LOOP_RUN {
                        cluster.looped = true;
                    }
                } else {
                    // a success breaks any failure run on this agent+tool
                    self.last_sig.remove(&(agent.clone(), tool.clone()));
                }
            }
            // the decision points, each a straight tally
            TranscriptItem::AnalystAsk { source, .. } => {
                *self.asks.entry(source.clone()).or_default() += 1;
            }
            TranscriptItem::ReviewFlag { .. } => self.review_flags += 1,
            TranscriptItem::TurnFailed { .. } => self.turn_failed += 1,
            TranscriptItem::SubAgentTerminated { class, .. } => {
                *self.terminations.entry(class.clone()).or_default() += 1;
            }
            TranscriptItem::ContextPressure { level, .. } => {
                // level 1 warns, level 2 (or any future higher level) is critical
                if *level >= 2 {
                    self.pressure_critical += 1;
                } else {
                    self.pressure_warn += 1;
                }
            }
            TranscriptItem::SubAgentExtended { trigger, .. } => {
                // key extensions by the trigger's snake_case wire name
                let key = serde_json::to_value(trigger)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_else(|| "unknown".to_owned());
                *self.extensions.entry(key).or_default() += 1;
            }
            TranscriptItem::LlmScrubbed { retry_id, .. } => {
                self.scrubbed += 1;
                // the segment-scoped episode key joins this scrub to its
                // retry outcome
                if let Some(retry_id) = retry_id {
                    let key = self.episode_key(retry_id);
                    self.scrub_episodes.insert(key.clone());
                    self.episodes.insert(key);
                }
            }
            TranscriptItem::ReportDegraded { .. } => self.report_degraded += 1,
            TranscriptItem::SessionResumed { interrupted } => {
                if *interrupted {
                    self.interrupted_resumes += 1;
                } else {
                    self.clean_resumes += 1;
                }
                // a restore barrier starts a fresh process: its episode ids
                // restart, and any pause left open died with the old process
                // (the gate's pause state is in-memory), so dangling pauses
                // close here instead of poisoning the open-pause flag forever
                self.segment += 1;
                self.resumed_lines = self.paused_lines;
            }
            TranscriptItem::SwarmFinished {
                succeeded, failed, ..
            } => {
                self.swarms_finished += 1;
                self.swarm_ok += *succeeded as u64;
                self.swarm_failed += *failed as u64;
            }
            // the provider-stall lines
            TranscriptItem::LlmRetry {
                retry_id,
                backoff_ms,
                ..
            } => {
                let key = self.episode_key(retry_id);
                self.episodes.insert(key);
                self.retry_attempts += 1;
                self.retry_backoff_ms += backoff_ms;
            }
            TranscriptItem::LlmRetryResolved {
                retry_id,
                succeeded,
                ..
            } => {
                let key = self.episode_key(retry_id);
                self.episodes.insert(key.clone());
                self.resolved.insert(key, *succeeded);
            }
            TranscriptItem::ProviderPaused { pause, .. } => {
                self.pauses.entry(pause_key(pause)).or_default().count += 1;
                self.paused_lines += 1;
            }
            TranscriptItem::ProviderResumed {
                pause,
                recovered,
                paused_ms,
                probe_attempts,
            } => {
                let tally = self.pauses.entry(pause_key(pause)).or_default();
                tally.total_paused_ms += paused_ms;
                tally.probe_attempts += u64::from(*probe_attempts);
                if !recovered {
                    tally.aborted += 1;
                }
                self.resumed_lines += 1;
            }
            // each seeding pass's roster tallies per registry detection, so
            // "never fired" is answerable against what actually ran
            TranscriptItem::DetectorsRun { detectors, .. } => {
                for line in detectors {
                    // honest rosters only carry registry identifiers; a name
                    // without that shape is a forged transcript riding prose
                    if !valid_detection_name(&line.name) {
                        continue;
                    }
                    let tally = self.detectors_run.entry(line.name.clone()).or_default();
                    tally.technique = line.technique.clone();
                    tally.runs += 1;
                    tally.hits += u64::from(line.hits);
                    tally.failed += u64::from(line.failed);
                }
            }
            // the offered-tool manifest unions per role
            TranscriptItem::ToolsOffered {
                role,
                tools,
                version,
            } => {
                self.offered
                    .entry(role.clone())
                    .or_default()
                    .extend(tools.iter().cloned());
                // appends are chronological, so the last manifest is newest
                self.harness_version = Some(version.clone());
            }
            // everything else carries no telemetry beyond what the usage fold
            // already counts (text payloads are deliberately never read here)
            _ => {}
        }
    }

    /// A retry episode's fold key, scoped to the current resume segment
    ///
    /// # Arguments
    ///
    /// * `retry_id` - The transcript's per-process episode id
    fn episode_key(&self, retry_id: &str) -> String {
        // the segment prefix keeps per-process ids from colliding across runs
        format!("{}#{retry_id}", self.segment)
    }

    /// Reduces the folded counters to the summary a bundle row is built from
    ///
    /// # Arguments
    ///
    /// * `meta` - The session's metadata, when `meta.json` was readable
    pub fn summarize(&self, meta: Option<&SessionMeta>) -> TelemetrySummary {
        // join calls, failures, and duration percentiles per tool
        let mut tools: BTreeMap<String, ToolTelemetry> = BTreeMap::new();
        for (tool, calls) in &self.calls {
            let durations = self.durations.get(tool);
            tools.insert(
                tool.clone(),
                ToolTelemetry {
                    calls: *calls,
                    fails: self.fails.get(tool).copied().unwrap_or(0),
                    p50_ms: durations.and_then(|samples| percentile(samples, 0.50)),
                    p95_ms: durations.and_then(|samples| percentile(samples, 0.95)),
                },
            );
        }
        // a failing tool with zero recorded calls (torn call line) still shows
        for (tool, fails) in &self.fails {
            tools.entry(tool.clone()).or_default().fails = *fails;
        }
        // count the scrubs whose episode ultimately succeeded
        let scrub_rescued = self
            .scrub_episodes
            .iter()
            .filter(|id| self.resolved.get(*id).copied().unwrap_or(false))
            .count() as u64;
        let decision_points = DecisionPoints {
            asks: self.asks.clone(),
            review_flags: self.review_flags,
            turn_failed: self.turn_failed,
            terminations: self.terminations.clone(),
            context_pressure_warn: self.pressure_warn,
            context_pressure_critical: self.pressure_critical,
            extensions: self.extensions.clone(),
            llm_scrubbed: self.scrubbed,
            scrub_rescued,
            report_degraded: self.report_degraded,
            interrupted_resumes: self.interrupted_resumes,
            clean_resumes: self.clean_resumes,
            swarms_finished: self.swarms_finished,
            swarm_members_succeeded: self.swarm_ok,
            swarm_members_failed: self.swarm_failed,
        };
        let provider_stalls = ProviderStalls {
            retry_episodes: self.episodes.len() as u64,
            retry_attempts: self.retry_attempts,
            retry_backoff_ms: self.retry_backoff_ms,
            retries_exhausted: self.resolved.values().filter(|ok| !**ok).count() as u64,
            pauses: self.pauses.clone(),
            open_pause: self.paused_lines > self.resumed_lines,
        };
        // order signature clusters by weight so previews read heaviest-first
        let mut signatures: Vec<SignatureCluster> = self
            .clusters
            .iter()
            .map(|((tool, sig), cluster)| SignatureCluster {
                tool: tool.clone(),
                sig: sig.clone(),
                count: cluster.count,
                looped: cluster.looped,
            })
            .collect();
        signatures.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| a.tool.cmp(&b.tool))
                .then_with(|| a.sig.cmp(&b.sig))
        });
        // only an anonymized session's bare 64-hex project name is the staged
        // sample's own identity; a hand-named project that merely looks like a
        // digest is still a file name, and file names never ship
        let binary_sha256 = meta
            .filter(|meta| meta.anonymized)
            .map(|meta| meta.project_name.as_str())
            .filter(|name| name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit()))
            .map(str::to_owned);
        TelemetrySummary {
            anonymized: meta.is_some_and(|meta| meta.anonymized),
            binary_sha256,
            harness_version: self.harness_version.clone(),
            tools,
            decision_points,
            provider_stalls,
            tools_offered: self
                .offered
                .iter()
                .map(|(role, roster)| (role.clone(), roster.iter().cloned().collect()))
                .collect(),
            detectors_run: self.detectors_run.clone(),
            signatures,
        }
    }
}

/// The per-session telemetry counters the usage scanner hands to the export
///
/// Carried on [`SessionSummary::telemetry`] when a scan gathers telemetry, so
/// the export's assembly step is a join rather than a re-read.
#[derive(Debug, Clone, Default)]
pub struct TelemetrySummary {
    /// Whether the session staged an anonymized copy of its sample
    pub anonymized: bool,
    /// The staged sample's hex SHA-256, when the project name is one
    pub binary_sha256: Option<String>,
    /// The newest tools-offered manifest's harness version
    pub harness_version: Option<String>,
    /// Per-tool call, failure, and duration telemetry
    pub tools: BTreeMap<String, ToolTelemetry>,
    /// The decision-point tallies
    pub decision_points: DecisionPoints,
    /// The provider-stall tallies
    pub provider_stalls: ProviderStalls,
    /// The offered-tool roster per role, sorted
    pub tools_offered: BTreeMap<String, Vec<String>>,
    /// The native detector roster's tallies, keyed by registry name
    pub detectors_run: BTreeMap<String, DetectorRunTally>,
    /// The failure-signature clusters, heaviest first
    pub signatures: Vec<SignatureCluster>,
}

/// Nearest-rank percentile over an unsorted sample set, in milliseconds
///
/// # Arguments
///
/// * `samples` - The duration samples to rank
/// * `quantile` - The quantile to pick (0.50 for the median)
pub(super) fn percentile(samples: &[u64], quantile: f64) -> Option<u64> {
    // no stamped pairs means no duration claim at all
    if samples.is_empty() {
        return None;
    }
    // rank over a sorted copy; samples-per-tool stay small
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let rank = ((sorted.len() - 1) as f64 * quantile).round() as usize;
    Some(sorted[rank.min(sorted.len() - 1)])
}

/// How many forum entries one session's bundle row lists at most
const MAX_FORUM_ENTRIES: usize = 256;

/// How many detector leads one session's bundle row lists at most
const MAX_DETECTOR_LEADS: usize = 128;

/// The seeded finding line: `Site: <addr> | confidence: <label> | suspicion: <score>`
///
/// Production shape pinned by a thorpal seed test; the LAST match wins since
/// hostile text embedded in the finding's content usually lands before the
/// template's own line. The defense is best-effort only: the template quotes
/// a recovered artifact value AFTER these lines, so sample content there can
/// still fake a later match (enums and numbers only) -- structured provenance
/// outranks this parse path entirely on current stores.
static SITE_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)^Site: \S+ \| confidence: (fact|likely|unsure|untrusted) \| suspicion: (-?\d+)$",
    )
    .expect("a valid site-line regex")
});

/// The seeded backing-sites line: `Sites (<total>): ...`
static SITES_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^Sites \((\d+)\):").expect("a valid sites-line regex"));

/// The seeded artifact line: `Artifact (<class label>): ...`
static ARTIFACT_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^Artifact \(([^)]+)\):").expect("a valid artifact-line regex")
});

/// Whether a stored artifact-class key is one the closed list defines
///
/// A key outside [`ArtifactClass::ALL`] (hostile or future) gates to no
/// artifact at all.
///
/// # Arguments
///
/// * `key` - The provenance-stored class key to check
fn valid_artifact_key(key: &str) -> bool {
    // the exact path stores keys, so match on the key side of thorpal's list
    ArtifactClass::ALL.iter().any(|class| class.key() == key)
}

/// Whether a stored finding source is one of the closed source names
///
/// # Arguments
///
/// * `source` - The provenance-stored source to check
fn valid_source(source: &str) -> bool {
    // gate against thorpal's own source vocabulary so the two crates can
    // never drift apart (a hand-copied list once silently dropped emulation)
    HitSource::ALL.iter().any(|known| known.key() == source)
}

/// Whether a stored detection name has the registry's identifier shape
///
/// Registry detection names (and library script names) are short `snake_case`
/// identifiers; anything else is a forged store trying to ride prose through
/// a name field, so it gates to no detection at all.
///
/// # Arguments
///
/// * `name` - The provenance-stored detection name to check
fn valid_detection_name(name: &str) -> bool {
    // short, lowercase, identifier-only: enough shape to carry no prose
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// Maps a parsed artifact label onto its class key, closed-list gated
///
/// # Arguments
///
/// * `label` - The label parsed out of an `Artifact (...)` line
fn artifact_key(label: &str) -> Option<&'static str> {
    // the label must match a known class exactly; the value is never read
    ArtifactClass::ALL
        .iter()
        .find(|class| class.label() == label)
        .map(|class| class.key())
}

/// Maps a confidence label onto its lead enum
///
/// # Arguments
///
/// * `label` - The seeded confidence label
fn lead_confidence(label: &str) -> Option<LeadConfidence> {
    // the four seeded labels are the whole vocabulary
    match label {
        "fact" => Some(LeadConfidence::Fact),
        "likely" => Some(LeadConfidence::Likely),
        "unsure" => Some(LeadConfidence::Unsure),
        "untrusted" => Some(LeadConfidence::Untrusted),
        _ => None,
    }
}

/// The histogram key a lead's confidence tallies under
///
/// # Arguments
///
/// * `confidence` - The lead's confidence, when any was recorded
fn confidence_key(confidence: Option<LeadConfidence>) -> &'static str {
    // unrated leads (rollup lines, unparseable bodies) get their own bucket
    match confidence {
        Some(LeadConfidence::Fact) => "fact",
        Some(LeadConfidence::Likely) => "likely",
        Some(LeadConfidence::Unsure) => "unsure",
        Some(LeadConfidence::Untrusted) => "untrusted",
        _ => "unrated",
    }
}

/// A lead's cap-ordering rank: strongest confidence first
///
/// # Arguments
///
/// * `lead` - The lead to rank
fn lead_rank(lead: &DetectorLead) -> u8 {
    // fact outranks likely outranks unsure outranks untrusted and unrated
    match lead.confidence {
        Some(LeadConfidence::Fact) => 0,
        Some(LeadConfidence::Likely) => 1,
        Some(LeadConfidence::Unsure) => 2,
        Some(LeadConfidence::Untrusted) => 3,
        _ => 4,
    }
}

/// The native detector-derived question names, as a closed vocabulary
///
/// A `q-*` name outside this list is a promoted library script's question and
/// degrades to the `script_question` class; when stage-1 provenance is
/// present its `question_origin` is authoritative instead.
const NATIVE_QUESTIONS: &[&str] = &[
    "q-unpack",
    "q-prior-toolkit",
    "q-decoded-config",
    "q-crypto-keys",
    "q-c2-channel",
    "q-ransomware",
    "q-evasion-gauntlet",
    "q-maldoc-macros",
    "q-container-payload",
    "q-script-triage",
    "q-rtf-embedded",
    "q-host-gate",
];

/// Classifies a detector post name into its closed vocabulary
///
/// Returns the `s<N>-` sibling index, when the name carries one, plus a
/// `&'static str` class -- static strings guarantee no analyst-influenced
/// text can ride a name class into a bundle.
///
/// # Arguments
///
/// * `name` - The detector post's reference name
fn name_class(name: &str) -> (Option<u32>, &'static str) {
    // strip a multi-sample scope prefix first and remember its index
    let (sample, bare) = match name
        .strip_prefix('s')
        .and_then(|rest| rest.split_once('-'))
        .and_then(|(digits, rest)| digits.parse::<u32>().ok().map(|index| (index, rest)))
    {
        Some((index, rest)) => (Some(index), rest),
        None => (None, name),
    };
    // match the fixed names, then the shaped families
    let class = match bare {
        "file-level" => "file_level",
        "file-identity" => "file_identity",
        "runtime-noise" => "runtime_noise",
        "primary-not-loaded" => "primary_not_loaded",
        "sample-set" => "sample_set",
        _ => {
            if bare.starts_with("fn-")
                && bare.len() > 3
                && bare[3..].bytes().all(|b| b.is_ascii_hexdigit())
            {
                // the address itself never ships, only the class
                "fn"
            } else if let Some(known) = NATIVE_QUESTIONS.iter().find(|known| **known == bare) {
                // an exact native question name is its own class
                known
            } else if bare.starts_with("q-host-gate-") {
                // host-gate questions mint a per-gate hex suffix; the dashed
                // prefix keeps a script's `q-host-gateway...` from classifying
                // as the native family
                "q-host-gate"
            } else if bare.starts_with("q-") {
                // a promoted library script's question name is analyst prose
                "script_question"
            } else {
                "other"
            }
        }
    };
    (sample, class)
}

/// Classifies a forum author into its bundle class
///
/// # Arguments
///
/// * `author` - The node's author string
fn author_class(author: &str) -> AuthorClass {
    // the two reserved authors are exact; everything else is an agent
    if author == DETECTOR_AUTHOR {
        AuthorClass::Detector
    } else if author == USER_AUTHOR {
        AuthorClass::User
    } else {
        AuthorClass::Agent
    }
}

/// A bundle-safe role for an agent-authored entry, shape-gated
///
/// Honest stores only ever record harness-minted authors (`main`,
/// `Role(Moon)`, `btw:` scopes), which reduce to short identifier-ish role
/// labels; anything longer or odder is a forged store trying to ride prose
/// through the role field, so it gates to no role at all.
///
/// # Arguments
///
/// * `author` - The entry's author string
fn entry_role(author: &str) -> Option<String> {
    // reduce to the role label the usage views group under
    let role = role_of(author);
    // gate the shape: short and identifier-ish (the shared "side chats"
    // label keeps its space)
    let valid = !role.is_empty()
        && role.chars().count() <= 32
        && role
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ' ');
    valid.then_some(role)
}

/// Local-only forum prose for the explorer's pop-outs
///
/// Deliberately NOT `Serialize`: it can never become a field of a bundle
/// type, so titles browsed locally can never ride a bundle off the machine.
#[derive(Debug, Clone, Default)]
pub struct LocalForumDetail {
    /// One record per [`ForumTelemetry::entries`] row, index-aligned
    pub entries: Vec<EntryDetail>,
    /// One record per `SessionTelemetry::detector_leads` row, index-aligned
    pub leads: Vec<LeadDetail>,
}

/// One detector lead's local-only prose
#[derive(Debug, Clone, Default)]
pub struct LeadDetail {
    /// The seeded post's reference name
    pub post_name: String,
    /// The seeded post's title
    pub post_title: String,
    /// The finding's first content line (after the technique opener)
    pub finding: String,
}

/// One tracked entry's local-only prose
#[derive(Debug, Clone, Default)]
pub struct EntryDetail {
    /// The forum node's reference name, when the entry is a post
    pub name: Option<String>,
    /// The question's title, a comment-question's first body line, or a
    /// task's description
    pub title: String,
}

/// A forum pass's output: end-state counts plus detector-lead outcomes
#[derive(Debug, Clone, Default)]
pub struct ForumOutcomes {
    /// The forum's end-state counts
    pub forum: ForumTelemetry,
    /// Detector-lead outcomes per technique id
    pub detectors: BTreeMap<String, DetectorTally>,
    /// Detector-seeded question resolutions by status
    pub detector_questions: DetectorQuestions,
    /// One row per seeded detector lead, strongest first (capped)
    pub detector_leads: Vec<DetectorLead>,
    /// How many leads the per-session cap dropped
    pub detector_leads_dropped: u64,
}

/// The technique id a finding comment's opening `[<id> ...]` line names
///
/// Only the opener is read: a finding comment carries exactly one technique,
/// and its later lines embed sample-derived text (decoded strings, artifact
/// values) that must never read as ids.
///
/// # Arguments
///
/// * `body` - The detector-authored comment body to read
fn opener_id(body: &str) -> Option<String> {
    // the opener is template-written as the body's very first characters
    COMMENT_OPENER.captures(body).map(|caps| caps[1].to_owned())
}

/// The technique ids a bucket post's `Techniques:` list lines name
///
/// # Arguments
///
/// * `body` - The detector-authored bucket post body to read
fn techniques_list_ids(body: &str) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    // list lines name the techniques of a whole seeded post
    for caps in TECHNIQUES_LINE.captures_iter(body) {
        for token in caps[1].split(',') {
            let token = token.trim().trim_end_matches('.');
            if ID_TOKEN.is_match(token) {
                ids.insert(token.to_owned());
            }
        }
    }
    ids
}

/// The technique ids a rollup post's per-site lines name
///
/// # Arguments
///
/// * `body` - The detector-authored rollup post body to read
fn rollup_ids(body: &str) -> BTreeSet<String> {
    // rollup lines name one technique per listed site
    ROLLUP_LINE
        .captures_iter(body)
        .map(|caps| caps[1].to_owned())
        .collect()
}

/// A question post's title as the bundle may carry it
///
/// Detector-seeded questions scoped to a sibling sample get a ` — <file
/// name>` suffix appended by the harness; file names must never ship, so the
/// title is cut at the first ` — ` (losing, at worst, a same-shaped tail of
/// the base title -- the sample index already says which sibling). Cutting at
/// the first match is the privacy-safe side: a file name containing ` — `
/// itself must never leave a leading fragment behind. All other titles pass
/// through whole.
///
/// # Arguments
///
/// * `title` - The post's local title
/// * `author` - The entry's author class
/// * `sample` - The entry's sample index, when the post name carried one
fn export_title(title: &str, author: AuthorClass, sample: Option<u32>) -> String {
    // only sampled detector questions carry the harness-appended suffix
    if matches!(author, AuthorClass::Detector) && sample.is_some() {
        if let Some(cut) = title.find(" — ") {
            return title[..cut].to_owned();
        }
    }
    title.to_owned()
}

/// How many characters a bundle entry's title may carry at most
const MAX_TITLE_CHARS: usize = 100;

/// Flattens a title (or a task description) to one bounded line for a bundle
///
/// Titles are analyst- and model-written prose, so a bundle row's copy
/// flattens every whitespace run (newlines included) to one space and caps
/// the length -- keeping every non-signature bundle string single-line and
/// short. The local explorer's prose twin keeps the full text.
///
/// # Arguments
///
/// * `title` - The raw title or description to flatten
fn clamp_title(title: &str) -> String {
    // flatten every whitespace run, newlines included, to a single space
    let flat = title.split_whitespace().collect::<Vec<_>>().join(" ");
    // cap at a character boundary, never splitting a code point
    if flat.chars().count() > MAX_TITLE_CHARS {
        flat.chars().take(MAX_TITLE_CHARS).collect()
    } else {
        flat
    }
}

/// Tallies a question status into the detector-question counters
///
/// # Arguments
///
/// * `questions` - The counters to tally into
/// * `status` - The question's resolution status
fn tally_detector_question(questions: &mut DetectorQuestions, status: QuestionStatus) {
    // one bucket per resolution status
    match status {
        QuestionStatus::Solved => questions.solved += 1,
        QuestionStatus::Blocked => questions.blocked += 1,
        QuestionStatus::Unsolved => questions.unsolved += 1,
        QuestionStatus::NeedsEvidence => questions.needs_evidence += 1,
    }
}

/// Folds a forum snapshot into its end-state and detector outcomes
///
/// The entry rows carry question titles and task descriptions -- prose that
/// ships only at level 1, where `finalize` redacts it and the review screen
/// can drop it (level 0 clears every title outright). Everything else is
/// counts, statuses, stances, closed-vocabulary classes, and detector
/// technique ids.
///
/// # Arguments
///
/// * `snapshot` - The forum snapshot to fold
pub fn fold_forum(snapshot: &ForumSnapshot) -> ForumOutcomes {
    // the export path discards the local-only prose records
    fold_forum_inner(snapshot).0
}

/// Folds a forum snapshot, also returning the explorer's local-only prose
///
/// # Arguments
///
/// * `snapshot` - The forum snapshot to fold
pub fn fold_forum_local(snapshot: &ForumSnapshot) -> (ForumOutcomes, LocalForumDetail) {
    // the local explorer wants the index-aligned titles too
    fold_forum_inner(snapshot)
}

/// The shared fold behind [`fold_forum`] and [`fold_forum_local`]
///
/// The prose twins fall out of the same row walk the entries need, so the
/// fold always builds them and [`fold_forum`] simply drops them.
///
/// # Arguments
///
/// * `snapshot` - The forum snapshot to fold
fn fold_forum_inner(snapshot: &ForumSnapshot) -> (ForumOutcomes, LocalForumDetail) {
    let mut outcomes = ForumOutcomes::default();
    // the end-state counts cover every author
    outcomes.forum.posts = snapshot.posts.len() as u64;
    outcomes.forum.comments = snapshot.comments.len() as u64;
    outcomes.forum.tasks = snapshot.tasks.len() as u64;
    outcomes.forum.tasks_unmet = snapshot
        .tasks
        .iter()
        .filter(|task| task.completed.is_none())
        .count() as u64;
    // tally every tracked question by its resolution status
    let kinds = snapshot
        .posts
        .iter()
        .map(|post| (&post.author, &post.kind))
        .chain(
            snapshot
                .comments
                .iter()
                .map(|comment| (&comment.author, &comment.kind)),
        );
    for (author, kind) in kinds {
        if let Kind::Question(status) = kind {
            outcomes.forum.questions += 1;
            match status {
                QuestionStatus::Solved => outcomes.forum.solved += 1,
                QuestionStatus::Unsolved => outcomes.forum.unsolved += 1,
                QuestionStatus::NeedsEvidence => outcomes.forum.needs_evidence += 1,
                QuestionStatus::Blocked => outcomes.forum.blocked += 1,
            }
            // detector-seeded questions tally separately for question 8
            if author == DETECTOR_AUTHOR {
                tally_detector_question(&mut outcomes.detector_questions, *status);
            }
        }
    }
    // index the forum for the thread walks below
    let posts: HashMap<&str, &Post> = snapshot
        .posts
        .iter()
        .map(|post| (post.name.as_str(), post))
        .collect();
    let comments: HashMap<usize, &Comment> = snapshot
        .comments
        .iter()
        .map(|comment| (comment.id, comment))
        .collect();
    // resolve a comment's root post by walking its parent chain (cycle-safe)
    let root_of = |comment: &Comment| -> Option<&Post> {
        let mut current = &comment.parent;
        let mut hops = 0;
        loop {
            match current {
                ParentRef::Post(name) => return posts.get(name.as_str()).copied(),
                ParentRef::Comment(id) => {
                    current = &comments.get(id)?.parent;
                    // a corrupt snapshot could cycle; bail instead of spinning
                    hops += 1;
                    if hops > snapshot.comments.len() {
                        return None;
                    }
                }
            }
        }
    };
    // gather each detector thread's technique ids: per node and unioned
    let mut thread_ids: HashMap<&str, BTreeSet<String>> = HashMap::new();
    let mut node_ids: HashMap<ParentRef, BTreeSet<String>> = HashMap::new();
    for post in &snapshot.posts {
        if post.author == DETECTOR_AUTHOR {
            // ids parse only from the template surface each class writes:
            // bucket bodies carry a `Techniques:` list and rollup bodies the
            // per-site lines; question and annotation bodies embed
            // sample-derived text (archive entry names, file identities)
            // that must never read as technique ids
            let (_, class) = name_class(&post.name);
            let ids = match class {
                "fn" | "file_level" => techniques_list_ids(&post.body),
                "runtime_noise" => rollup_ids(&post.body),
                _ => BTreeSet::new(),
            };
            thread_ids
                .entry(post.name.as_str())
                .or_default()
                .extend(ids.iter().cloned());
            node_ids.insert(ParentRef::Post(post.name.clone()), ids);
        }
    }
    for comment in &snapshot.comments {
        if comment.author == DETECTOR_AUTHOR {
            let Some(root) = root_of(comment) else {
                continue;
            };
            // detector comments only seed under detector posts, but guard
            // anyway: an agent thread never gains detector ids
            if root.author != DETECTOR_AUTHOR {
                continue;
            }
            // a finding comment names exactly one technique in its opener
            let ids: BTreeSet<String> = opener_id(&comment.body).into_iter().collect();
            thread_ids
                .entry(root.name.as_str())
                .or_default()
                .extend(ids.iter().cloned());
            node_ids.insert(ParentRef::Comment(comment.id), ids);
        }
    }
    // count each node's non-detector subtree replies in one chain walk, so
    // entries can report engagement and the untouched test has its evidence
    let mut subtree_replies: HashMap<ParentRef, u64> = HashMap::new();
    for comment in &snapshot.comments {
        if comment.author == DETECTOR_AUTHOR {
            continue;
        }
        // credit every ancestor on the chain, cycle-capped like root_of
        let mut current = Some(&comment.parent);
        let mut hops = 0;
        while let Some(parent) = current {
            *subtree_replies.entry(parent.clone()).or_default() += 1;
            current = match parent {
                ParentRef::Post(_) => None,
                ParentRef::Comment(id) => comments.get(id).map(|parent| &parent.parent),
            };
            hops += 1;
            if hops > snapshot.comments.len() {
                break;
            }
        }
    }
    // the seeding watermark: the earliest revision any non-detector node
    // carries; a detector node older than it was never touched after seeding
    let revisions_tracked = snapshot
        .posts
        .iter()
        .map(|post| post.updated)
        .chain(snapshot.comments.iter().map(|comment| comment.updated))
        .any(|updated| updated > 0);
    // the earliest detector-seeded revision anchors the watermark window:
    // non-detector activity recorded BEFORE seeding (a pre-planted analyst
    // question) must not read as post-seeding engagement
    let min_detector_rev = snapshot
        .posts
        .iter()
        .filter(|post| post.author == DETECTOR_AUTHOR)
        .map(|post| post.updated)
        .chain(
            snapshot
                .comments
                .iter()
                .filter(|comment| comment.author == DETECTOR_AUTHOR)
                .map(|comment| comment.updated),
        )
        .filter(|updated| *updated > 0)
        .min()
        .unwrap_or(0);
    let seed_rev = snapshot
        .posts
        .iter()
        .filter(|post| post.author != DETECTOR_AUTHOR)
        .map(|post| post.updated)
        .chain(
            snapshot
                .comments
                .iter()
                .filter(|comment| comment.author != DETECTOR_AUTHOR)
                .map(|comment| comment.updated),
        )
        .filter(|updated| *updated > 0 && *updated >= min_detector_rev)
        .min()
        .unwrap_or(u64::MAX);
    // whether a detector-seeded node went wholly unengaged after seeding
    let untouched_of = |author: &str, updated: u64, node: &ParentRef| -> Option<bool> {
        // only detector entries have a seeding moment to measure against
        if author != DETECTOR_AUTHOR || !revisions_tracked {
            return None;
        }
        let replies = subtree_replies.get(node).copied().unwrap_or(0);
        Some(updated < seed_rev && replies == 0)
    };
    // tasks group under their gating question for the per-entry tallies
    let mut tasks_by_parent: HashMap<&ParentRef, (u64, u64)> = HashMap::new();
    for task in &snapshot.tasks {
        let entry = tasks_by_parent.entry(&task.parent).or_default();
        entry.0 += 1;
        if task.completed.is_none() {
            entry.1 += 1;
        }
    }
    // index the structured provenance early: exact records outrank both the
    // lead parsing below and the name-class heuristic for question origins
    let provenance: HashMap<&ParentRef, &NodeProvenance> = snapshot
        .detector_provenance
        .iter()
        .map(|record| (&record.node, record))
        .collect();
    // build one row (plus its local-only prose twin) per tracked entry
    let mut rows: Vec<(ForumEntry, EntryDetail)> = Vec::new();
    for post in &snapshot.posts {
        let Kind::Question(status) = post.kind else {
            continue;
        };
        let node = ParentRef::Post(post.name.clone());
        // detector names reduce to their closed class; agent and user post
        // names are prose and never classify
        let author = author_class(&post.author);
        let (sample, class) = match author {
            AuthorClass::Detector => {
                let (sample, class) = name_class(&post.name);
                // stage-1 provenance is authoritative for question origins: a
                // promoted script's question may reuse a native-looking name
                let class = match provenance.get(&node) {
                    Some(record)
                        if record.shape == "question"
                            && record.question_origin.as_deref() == Some("script") =>
                    {
                        "script_question"
                    }
                    _ => class,
                };
                (sample, Some(class.to_owned()))
            }
            _ => (None, None),
        };
        let (tasks, tasks_unmet) = tasks_by_parent.get(&node).copied().unwrap_or_default();
        rows.push((
            ForumEntry {
                kind: EntryKind::Question,
                status: entry_status(status),
                author,
                role: matches!(author, AuthorClass::Agent)
                    .then(|| entry_role(&post.author))
                    .flatten(),
                name_class: class,
                sample,
                techniques: node_ids
                    .get(&node)
                    .map(|ids| ids.iter().cloned().collect())
                    .unwrap_or_default(),
                updated: post.updated,
                parent: None,
                untouched: untouched_of(&post.author, post.updated, &node),
                tasks,
                tasks_unmet,
                replies: subtree_replies.get(&node).copied().unwrap_or(0),
                // prose: gated to level 1 (and redaction) by finalize, with
                // any harness-appended file-name suffix cut first and the
                // result flattened to one bounded line
                title: Some(clamp_title(&export_title(&post.title, author, sample))),
            },
            EntryDetail {
                name: Some(post.name.clone()),
                title: post.title.clone(),
            },
        ));
    }
    for comment in &snapshot.comments {
        let Kind::Question(status) = comment.kind else {
            continue;
        };
        let node = ParentRef::Comment(comment.id);
        let author = author_class(&comment.author);
        let (tasks, tasks_unmet) = tasks_by_parent.get(&node).copied().unwrap_or_default();
        // a comment has no title; its first body line stands in as prose
        let first_line = comment.body.lines().next().unwrap_or_default().to_owned();
        rows.push((
            ForumEntry {
                kind: EntryKind::Question,
                status: entry_status(status),
                author,
                role: matches!(author, AuthorClass::Agent)
                    .then(|| entry_role(&comment.author))
                    .flatten(),
                name_class: None,
                sample: None,
                techniques: node_ids
                    .get(&node)
                    .map(|ids| ids.iter().cloned().collect())
                    .unwrap_or_default(),
                updated: comment.updated,
                parent: Some(match comment.parent {
                    ParentRef::Post(_) => NodeKind::Post,
                    ParentRef::Comment(_) => NodeKind::Comment,
                }),
                untouched: untouched_of(&comment.author, comment.updated, &node),
                tasks,
                tasks_unmet,
                replies: subtree_replies.get(&node).copied().unwrap_or(0),
                // a comment has no title and its body never ships: the first
                // line serves only the local explorer's pop-outs below
                title: None,
            },
            EntryDetail {
                name: None,
                title: first_line,
            },
        ));
    }
    for task in &snapshot.tasks {
        rows.push((
            ForumEntry {
                kind: EntryKind::Task,
                status: if task.completed.is_some() {
                    EntryStatus::Met
                } else {
                    EntryStatus::Unmet
                },
                author: author_class(&task.author),
                role: matches!(author_class(&task.author), AuthorClass::Agent)
                    .then(|| entry_role(&task.author))
                    .flatten(),
                name_class: None,
                sample: None,
                techniques: Vec::new(),
                updated: task.updated,
                parent: Some(match task.parent {
                    ParentRef::Post(_) => NodeKind::Post,
                    ParentRef::Comment(_) => NodeKind::Comment,
                }),
                untouched: None,
                tasks: 0,
                tasks_unmet: 0,
                replies: 0,
                // a description is prose like a title: one bounded line only
                title: Some(clamp_title(&task.description)),
            },
            EntryDetail {
                name: None,
                title: task.description.clone(),
            },
        ));
    }
    // unresolved entries sort first, newest activity next; the sort is
    // stable, so same-rank same-revision rows keep their build order
    rows.sort_by(|a, b| {
        entry_rank(&a.0)
            .cmp(&entry_rank(&b.0))
            .then_with(|| b.0.updated.cmp(&a.0.updated))
    });
    // cap the rows and record what the cap dropped
    if rows.len() > MAX_FORUM_ENTRIES {
        outcomes.forum.entries_dropped = (rows.len() - MAX_FORUM_ENTRIES) as u64;
        rows.truncate(MAX_FORUM_ENTRIES);
    }
    // unzip so the bundle rows and the local prose stay index-aligned
    let (entries, details): (Vec<ForumEntry>, Vec<EntryDetail>) = rows.into_iter().unzip();
    outcomes.forum.entries = entries;
    // the local prose record completes once the leads below are built too
    let entry_details = details;
    // attribute each agent comment's stance to the nearest detector ancestor
    // with ids (falling back to the root post's ids, then the thread union)
    let mut verdicts: HashMap<&str, BTreeMap<String, ThreadVerdict>> = HashMap::new();
    for comment in &snapshot.comments {
        if comment.author == DETECTOR_AUTHOR {
            continue;
        }
        let Some(root) = root_of(comment) else {
            continue;
        };
        // only detector-seeded threads carry leads to grade
        if root.author != DETECTOR_AUTHOR {
            continue;
        }
        // walk up to the nearest detector node that parsed any ids
        let mut target: Option<&BTreeSet<String>> = None;
        let mut current = Some(&comment.parent);
        let mut hops = 0;
        while let Some(parent) = current {
            if let Some(ids) = node_ids.get(parent).filter(|ids| !ids.is_empty()) {
                target = Some(ids);
                break;
            }
            current = match parent {
                ParentRef::Post(_) => None,
                ParentRef::Comment(id) => comments.get(id).map(|parent| &parent.parent),
            };
            hops += 1;
            if hops > snapshot.comments.len() {
                break;
            }
        }
        // fall back to the root's own ids, then the whole thread's union
        let root_ref = ParentRef::Post(root.name.clone());
        let fallback = node_ids.get(&root_ref).filter(|ids| !ids.is_empty());
        let union = thread_ids
            .get(root.name.as_str())
            .filter(|ids| !ids.is_empty());
        let Some(ids) = target.or(fallback).or(union) else {
            continue;
        };
        // the comment's stance lands on every technique it attributed to
        let thread = verdicts.entry(root.name.as_str()).or_default();
        for id in ids {
            let verdict = thread.entry(id.clone()).or_default();
            match comment.stance {
                Stance::Disproves => verdict.disproved = true,
                Stance::Supports => verdict.supported = true,
                Stance::Neutral => verdict.engaged = true,
            }
        }
    }
    // every seeded technique gets a verdict, engaged or not
    for (thread, ids) in &thread_ids {
        for id in ids {
            let verdict = verdicts
                .get(*thread)
                .and_then(|thread| thread.get(id))
                .copied()
                .unwrap_or_default();
            let tally = outcomes.detectors.entry(id.clone()).or_default();
            // disproving evidence outranks support, which outranks engagement
            if verdict.disproved {
                tally.disputed += 1;
            } else if verdict.supported {
                tally.confirmed += 1;
            } else if verdict.engaged {
                tally.neutral += 1;
            } else {
                tally.ignored += 1;
            }
        }
    }
    // the verdict a lead inherits from its thread for one technique
    let verdict_of = |root: &str, technique: &str| -> LeadVerdict {
        match verdicts.get(root).and_then(|thread| thread.get(technique)) {
            Some(verdict) if verdict.disproved => LeadVerdict::Disputed,
            Some(verdict) if verdict.supported => LeadVerdict::Confirmed,
            Some(verdict) if verdict.engaged => LeadVerdict::Neutral,
            _ => LeadVerdict::Ignored,
        }
    };
    // build one lead row (plus its local prose twin) per seeded finding
    let mut leads: Vec<(DetectorLead, LeadDetail)> = Vec::new();
    for comment in &snapshot.comments {
        if comment.author != DETECTOR_AUTHOR {
            continue;
        }
        let Some(root) = root_of(comment) else {
            continue;
        };
        if root.author != DETECTOR_AUTHOR {
            continue;
        }
        let node = ParentRef::Comment(comment.id);
        let (sample, post_class) = name_class(&root.name);
        let shared = DetectorLead {
            post_class: post_class.to_owned(),
            sample,
            shape: LeadShape::Finding,
            replies: subtree_replies.get(&node).copied().unwrap_or(0),
            untouched: untouched_of(&comment.author, comment.updated, &node),
            ..DetectorLead::default()
        };
        // the finding's first content line, for the local pop-outs only
        let finding_line = comment.body.lines().next().unwrap_or_default().to_owned();
        let detail_of = || LeadDetail {
            post_name: root.name.clone(),
            post_title: root.title.clone(),
            finding: finding_line.clone(),
        };
        if let Some(record) = provenance.get(&node) {
            // structured provenance outranks template parsing entirely
            for finding in &record.findings {
                // a thread verdict is engagement even when the finding's own
                // subtree is silent, so it overrides the untouched flag
                let verdict = verdict_of(&root.name, &finding.technique);
                leads.push((
                    DetectorLead {
                        technique: finding.technique.clone(),
                        confidence: lead_confidence(&finding.confidence),
                        suspicion: Some(finding.suspicion),
                        sites: Some(finding.sites),
                        // class fields ship only when they match the closed
                        // vocabularies, so a forged store cannot smuggle
                        // prose through the provenance block
                        artifact: finding
                            .artifact_class
                            .as_deref()
                            .filter(|key| valid_artifact_key(key))
                            .map(str::to_owned),
                        source: valid_source(&finding.source).then(|| finding.source.clone()),
                        // script names are analyst-influenced and stay local,
                        // and even a claimed-native name must keep the
                        // registry's identifier shape to ship
                        detection: (finding.source == "native")
                            .then(|| finding.detection.clone())
                            .flatten()
                            .filter(|name| valid_detection_name(name)),
                        untouched: shared
                            .untouched
                            .map(|flag| flag && matches!(verdict, LeadVerdict::Ignored)),
                        verdict,
                        exact: true,
                        ..shared.clone()
                    },
                    detail_of(),
                ));
            }
        } else if let Some(ids) = node_ids.get(&node).filter(|ids| !ids.is_empty()) {
            // parse the seeded template: the LAST site line wins, since a
            // hostile body can fake earlier ones but seeding appends last
            let site = SITE_LINE
                .captures_iter(&comment.body)
                .last()
                .map(|caps| (caps[1].to_owned(), caps[2].to_owned()));
            let sites = SITES_LINE
                .captures_iter(&comment.body)
                .last()
                .and_then(|caps| caps[1].parse::<u64>().ok());
            let artifact = ARTIFACT_LINE
                .captures_iter(&comment.body)
                .last()
                .and_then(|caps| artifact_key(&caps[1]))
                .map(str::to_owned);
            for technique in ids {
                // a thread verdict is engagement even when the finding's own
                // subtree is silent, so it overrides the untouched flag
                let verdict = verdict_of(&root.name, technique);
                leads.push((
                    DetectorLead {
                        technique: technique.clone(),
                        confidence: site.as_ref().and_then(|(label, _)| lead_confidence(label)),
                        suspicion: site
                            .as_ref()
                            .and_then(|(_, score)| score.parse::<i64>().ok()),
                        // the template writes a Sites line only when the
                        // list is non-empty, so no line means zero listed
                        // sites -- matching the provenance path's count
                        sites: sites.or(Some(0)),
                        artifact: artifact.clone(),
                        untouched: shared
                            .untouched
                            .map(|flag| flag && matches!(verdict, LeadVerdict::Ignored)),
                        verdict,
                        exact: false,
                        ..shared.clone()
                    },
                    detail_of(),
                ));
            }
        }
    }
    // rollup posts carry one unrated lead per suppressed line
    for post in &snapshot.posts {
        if post.author != DETECTOR_AUTHOR {
            continue;
        }
        let (sample, post_class) = name_class(&post.name);
        if post_class != "runtime_noise" {
            continue;
        }
        let node = ParentRef::Post(post.name.clone());
        // a rollup lead's engagement is its post's: replies in the post's
        // subtree, and the same seeding-watermark untouched test
        let replies = subtree_replies.get(&node).copied().unwrap_or(0);
        let untouched = untouched_of(&post.author, post.updated, &node);
        if let Some(record) = provenance.get(&node) {
            // structured rollup records carry the real facts
            for finding in &record.findings {
                // a thread verdict is engagement, so it overrides untouched
                let verdict = verdict_of(&post.name, &finding.technique);
                leads.push((
                    DetectorLead {
                        technique: finding.technique.clone(),
                        post_class: post_class.to_owned(),
                        sample,
                        shape: LeadShape::Rollup,
                        confidence: lead_confidence(&finding.confidence),
                        suspicion: Some(finding.suspicion),
                        sites: Some(finding.sites),
                        // the artifact class gates exactly like finding leads
                        artifact: finding
                            .artifact_class
                            .as_deref()
                            .filter(|key| valid_artifact_key(key))
                            .map(str::to_owned),
                        source: valid_source(&finding.source).then(|| finding.source.clone()),
                        detection: (finding.source == "native")
                            .then(|| finding.detection.clone())
                            .flatten()
                            .filter(|name| valid_detection_name(name)),
                        replies,
                        untouched: untouched
                            .map(|flag| flag && matches!(verdict, LeadVerdict::Ignored)),
                        verdict,
                        exact: true,
                        ..DetectorLead::default()
                    },
                    LeadDetail {
                        post_name: post.name.clone(),
                        post_title: post.title.clone(),
                        finding: String::new(),
                    },
                ));
            }
        } else {
            for caps in ROLLUP_LINE.captures_iter(&post.body) {
                let technique = caps[1].to_owned();
                // a thread verdict is engagement, so it overrides untouched
                let verdict = verdict_of(&post.name, &technique);
                leads.push((
                    DetectorLead {
                        technique,
                        post_class: post_class.to_owned(),
                        sample,
                        shape: LeadShape::Rollup,
                        replies,
                        untouched: untouched
                            .map(|flag| flag && matches!(verdict, LeadVerdict::Ignored)),
                        verdict,
                        ..DetectorLead::default()
                    },
                    LeadDetail {
                        post_name: post.name.clone(),
                        post_title: post.title.clone(),
                        finding: String::new(),
                    },
                ));
            }
        }
    }
    // fold each lead into its technique's histogram before the cap, so the
    // tallies stay complete however many rows the cap drops
    for (lead, _) in &leads {
        let tally = outcomes
            .detectors
            .entry(lead.technique.clone())
            .or_default();
        tally.leads += 1;
        *tally
            .confidence
            .entry(confidence_key(lead.confidence).to_owned())
            .or_default() += 1;
    }
    // strongest leads first, then most suspicious, then by technique
    leads.sort_by(|a, b| {
        lead_rank(&a.0)
            .cmp(&lead_rank(&b.0))
            .then_with(|| {
                b.0.suspicion
                    .unwrap_or(i64::MIN)
                    .cmp(&a.0.suspicion.unwrap_or(i64::MIN))
            })
            .then_with(|| a.0.technique.cmp(&b.0.technique))
    });
    if leads.len() > MAX_DETECTOR_LEADS {
        outcomes.detector_leads_dropped = (leads.len() - MAX_DETECTOR_LEADS) as u64;
        leads.truncate(MAX_DETECTOR_LEADS);
    }
    let (lead_rows, lead_details): (Vec<DetectorLead>, Vec<LeadDetail>) = leads.into_iter().unzip();
    outcomes.detector_leads = lead_rows;
    // hand back the prose twins beside the outcomes they index into
    let detail = LocalForumDetail {
        entries: entry_details,
        leads: lead_details,
    };
    (outcomes, detail)
}

/// Maps a question status onto its bundle entry status
///
/// # Arguments
///
/// * `status` - The forum's question status
fn entry_status(status: QuestionStatus) -> EntryStatus {
    // one bucket per resolution status
    match status {
        QuestionStatus::Solved => EntryStatus::Solved,
        QuestionStatus::Unsolved => EntryStatus::Unsolved,
        QuestionStatus::NeedsEvidence => EntryStatus::NeedsEvidence,
        QuestionStatus::Blocked => EntryStatus::Blocked,
    }
}

/// An entry's sort rank: unresolved work first
///
/// # Arguments
///
/// * `entry` - The entry to rank
fn entry_rank(entry: &ForumEntry) -> u8 {
    // unsolved, blocked, needs-evidence, unmet, then everything settled
    match entry.status {
        EntryStatus::Unsolved => 0,
        EntryStatus::Blocked => 1,
        EntryStatus::NeedsEvidence => 2,
        EntryStatus::Unmet => 3,
        _ => 4,
    }
}

/// One technique's stance accumulation inside one thread
#[derive(Debug, Clone, Copy, Default)]
struct ThreadVerdict {
    /// Whether any comment disproved the lead
    disproved: bool,
    /// Whether any comment supported the lead
    supported: bool,
    /// Whether any comment engaged without a decisive stance
    engaged: bool,
}

/// Reads and folds a session's `forum.json` for the local explorer
///
/// Like [`read_forum`] (missing means empty, corrupt logs at debug, never
/// `SessionStore::open`), but also returns the index-aligned local-only prose
/// the explorer's pop-outs show -- which therefore never exists on the export
/// path at all.
///
/// # Arguments
///
/// * `dir` - The session directory holding `forum.json`
pub async fn read_forum_local(dir: &Path) -> (ForumOutcomes, LocalForumDetail) {
    // read the raw file; a missing forum is just an empty one
    let raw = match tokio::fs::read(dir.join("forum.json")).await {
        Ok(raw) => raw,
        Err(_) => return (ForumOutcomes::default(), LocalForumDetail::default()),
    };
    // parse leniently: one corrupt forum must not fail the whole explorer
    match serde_json::from_slice::<ForumSnapshot>(&raw) {
        Ok(snapshot) => fold_forum_local(&snapshot),
        Err(err) => {
            tracing::debug!(?err, dir = %dir.display(), "skipping an unreadable forum.json");
            (ForumOutcomes::default(), LocalForumDetail::default())
        }
    }
}

/// Reads and folds a session's `forum.json`, best-effort
///
/// A missing file is an empty forum; a corrupt one logs at debug and counts
/// as empty too (the export must never fail on one damaged session). This
/// never goes through `SessionStore::open`, which would create directories in
/// a tree the exporter promises to leave untouched.
///
/// # Arguments
///
/// * `dir` - The session directory holding `forum.json`
pub async fn read_forum(dir: &Path) -> ForumOutcomes {
    // read the raw file; a missing forum is just an empty one
    let raw = match tokio::fs::read(dir.join("forum.json")).await {
        Ok(raw) => raw,
        Err(_) => return ForumOutcomes::default(),
    };
    // parse leniently: one corrupt forum must not fail a whole export
    match serde_json::from_slice::<ForumSnapshot>(&raw) {
        Ok(snapshot) => fold_forum(&snapshot),
        Err(err) => {
            tracing::debug!(?err, dir = %dir.display(), "skipping an unreadable forum.json");
            ForumOutcomes::default()
        }
    }
}

/// Joins a scanned session's summaries into its bundle row
///
/// # Arguments
///
/// * `summary` - The usage scanner's per-session summary (with telemetry)
/// * `outcomes` - The forum pass's end-state and detector outcomes
pub fn assemble_session(summary: &SessionSummary, outcomes: ForumOutcomes) -> SessionTelemetry {
    // the session name may be descriptive, so only its hash ever ships
    let session_id = format!("{:x}", Sha256::digest(summary.name.as_bytes()));
    // group the per-instance agent windows into per-role rollups
    let mut agents: BTreeMap<String, AgentRoleTelemetry> = BTreeMap::new();
    for (name, window) in &summary.agents {
        // prefer the window's authoritative role, else derive it from the name
        let role = if window.role.is_empty() {
            role_of(name)
        } else {
            window.role.clone()
        };
        let rollup = agents.entry(role).or_default();
        rollup.instances += 1;
        rollup.total_tokens += token_sum(&window.totals);
        rollup.tool_calls += window.tool_calls;
    }
    // flatten the local-hour buckets to integer token totals
    let hours = summary
        .hours
        .iter()
        .map(|((date, hour), bucket)| {
            let totals = UsageTotals {
                input_tokens: bucket.input.round() as u64,
                output_tokens: bucket.output.round() as u64,
                cached_input_tokens: bucket.cached.round() as u64,
                cache_creation_input_tokens: bucket.cache_creation.round() as u64,
                total_tokens: (bucket.input + bucket.output + bucket.cached + bucket.cache_creation)
                    .round() as u64,
                ..UsageTotals::default()
            };
            (format!("{date}T{hour:02}"), totals)
        })
        .collect();
    // the telemetry counters ride the summary when the scan gathered them
    let telemetry = summary.telemetry.clone().unwrap_or_default();
    SessionTelemetry {
        session_id,
        binary_sha256: telemetry.binary_sha256.clone(),
        anonymized: telemetry.anonymized,
        model: summary.model.clone(),
        harness_version: telemetry.harness_version.clone(),
        created_at: summary.created_at,
        duration_ms: match (summary.first_ts, summary.last_ts) {
            (Some(first), Some(last)) => Some(last.saturating_sub(first)),
            _ => None,
        },
        active_ms: summary.active_ms,
        totals: summary.totals,
        agents,
        tools: telemetry.tools.clone(),
        tools_offered: telemetry.tools_offered.clone(),
        decision_points: telemetry.decision_points.clone(),
        provider_stalls: telemetry.provider_stalls.clone(),
        forum: outcomes.forum,
        detectors: outcomes.detectors,
        detector_questions: outcomes.detector_questions,
        detector_leads: outcomes.detector_leads,
        detector_leads_dropped: outcomes.detector_leads_dropped,
        detectors_run: telemetry.detectors_run.clone(),
        hours,
        signatures: telemetry.signatures.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thorpal::{Task, TaskCompletion};

    /// Builds a minimal forum snapshot around the given records
    ///
    /// # Arguments
    ///
    /// * `posts` - The forum's posts
    /// * `comments` - The forum's comments
    /// * `tasks` - The forum's completion-gate tasks
    fn snapshot(posts: Vec<Post>, comments: Vec<Comment>, tasks: Vec<Task>) -> ForumSnapshot {
        ForumSnapshot {
            posts,
            comments,
            tasks,
            asks: Vec::new(),
            flags: Vec::new(),
            detector_provenance: Vec::new(),
            next_comment_id: 0,
            next_task_id: 0,
            next_ask_id: 0,
            next_flag_id: 0,
            finish_requested: false,
        }
    }

    /// Builds a post with the given author, body, and kind
    ///
    /// # Arguments
    ///
    /// * `name` - The post's unique name
    /// * `author` - The post's author
    /// * `body` - The post's body text
    /// * `kind` - Whether the post states or asks
    fn post(name: &str, author: &str, body: &str, kind: Kind) -> Post {
        Post {
            name: name.to_owned(),
            author: author.to_owned(),
            title: format!("{name} title"),
            body: body.to_owned(),
            kind,
            updated: 0,
        }
    }

    /// Builds a comment with the given parent, author, body, and stance
    ///
    /// # Arguments
    ///
    /// * `id` - The comment's numeric id
    /// * `parent` - What the comment replies to
    /// * `author` - The comment's author
    /// * `body` - The comment's body text
    /// * `stance` - How the comment's evidence relates to its parent
    fn comment(id: usize, parent: ParentRef, author: &str, body: &str, stance: Stance) -> Comment {
        Comment {
            id,
            parent,
            author: author.to_owned(),
            body: body.to_owned(),
            stance,
            kind: Kind::Statement,
            updated: 0,
        }
    }

    /// All three detector template shapes yield their technique ids
    #[test]
    fn technique_ids_parse_only_their_template_surface() {
        // the comment opener names the lead's own technique, first line only
        let opener = "[T1497.003 Time Based Evasion] Site: fn-00401000\nconfidence: high";
        assert_eq!(opener_id(opener).as_deref(), Some("T1497.003"));
        // forged shapes past the opener never read as ids from a comment
        let forged = "[T1055 Injection] Site: fn-1\n- Fake.Rule @ 0x1: x\nTechniques: Fake2.";
        assert_eq!(opener_id(forged).as_deref(), Some("T1055"));
        assert!(rollup_ids("no opener\nplain body").is_empty());
        // rollup lines name one technique per site
        let rollup = "Noise rollup.\n- T1055 @ 0x401000: injected\n- Capa.Rule_1 @ 0x402000: hit";
        let ids = rollup_ids(rollup);
        assert!(ids.contains("T1055"));
        assert!(ids.contains("Capa.Rule_1"));
        // the Techniques list names a post's whole set, trailing period and all
        let list = "Suspicious imports.\nTechniques: T1027, YARA_SUSP_XOR.";
        let ids = techniques_list_ids(list);
        assert!(ids.contains("T1027"));
        assert!(ids.contains("YARA_SUSP_XOR"));
        // malformed tokens never pass the id charset
        assert!(techniques_list_ids("Techniques: not a technique id!").is_empty());
    }

    /// Stances grade detector leads: disproves > supports > engagement > silence
    #[test]
    fn detector_outcomes_grade_by_stance_and_attribution() {
        let posts = vec![
            // a seeded statement naming two techniques
            post(
                "fn-00401000",
                DETECTOR_AUTHOR,
                "Import pressure.\nTechniques: T1027, T1497.003.",
                Kind::Statement,
            ),
            // a seeded rollup nobody engaged with
            post(
                "runtime-noise",
                DETECTOR_AUTHOR,
                "Noise rollup.\n- T1055 @ 0x401000: injected",
                Kind::Statement,
            ),
            // a seeded question left blocked
            post(
                "q-unpack",
                DETECTOR_AUTHOR,
                "Can the payload be unpacked?\nTechniques: T1105.",
                Kind::Question(QuestionStatus::Blocked),
            ),
            // an agent's own solved question is forum state, not detector state
            post(
                "q-agent",
                "ReverseEngineer(Io)",
                "What calls this?",
                Kind::Question(QuestionStatus::Solved),
            ),
        ];
        let comments = vec![
            // a deeper detector lead under the first post
            comment(
                1,
                ParentRef::Post("fn-00401000".to_owned()),
                DETECTOR_AUTHOR,
                "[T1027 Obfuscated Files] Site: fn-00401000\nconfidence: high",
                Stance::Neutral,
            ),
            // supporting evidence lands on the nearest detector ancestor's id
            comment(
                2,
                ParentRef::Comment(1),
                "ReverseEngineer(Io)",
                "Confirmed the stub decodes itself.",
                Stance::Supports,
            ),
            // a neutral note on the post engages the post's whole id set
            comment(
                3,
                ParentRef::Post("fn-00401000".to_owned()),
                "ReverseEngineer(Io)",
                "Looking into the timing checks.",
                Stance::Neutral,
            ),
        ];
        let tasks = vec![
            Task {
                id: 1,
                parent: ParentRef::Post("q-unpack".to_owned()),
                author: DETECTOR_AUTHOR.to_owned(),
                description: "identify the unpack routine".to_owned(),
                completed: None,
                updated: 0,
            },
            Task {
                id: 2,
                parent: ParentRef::Post("q-agent".to_owned()),
                author: "ReverseEngineer(Io)".to_owned(),
                description: "list the callers".to_owned(),
                completed: Some(TaskCompletion {
                    comment: 2,
                    author: "ReverseEngineer(Io)".to_owned(),
                }),
                updated: 0,
            },
        ];
        let outcomes = fold_forum(&snapshot(posts, comments, tasks));
        // the end-state counts cover every author
        assert_eq!(outcomes.forum.posts, 4);
        assert_eq!(outcomes.forum.comments, 3);
        assert_eq!(outcomes.forum.questions, 2);
        assert_eq!(outcomes.forum.solved, 1);
        assert_eq!(outcomes.forum.blocked, 1);
        assert_eq!(outcomes.forum.tasks, 2);
        assert_eq!(outcomes.forum.tasks_unmet, 1);
        // only the detector's own question tallies as a detector question
        assert_eq!(outcomes.detector_questions.blocked, 1);
        assert_eq!(outcomes.detector_questions.solved, 0);
        // T1027: supported via the nearest detector ancestor (the comment)
        assert_eq!(outcomes.detectors["T1027"].confirmed, 1);
        // T1497.003: only neutral engagement through the post's own set
        assert_eq!(outcomes.detectors["T1497.003"].neutral, 1);
        // T1055: seeded in the rollup, never engaged
        assert_eq!(outcomes.detectors["T1055"].ignored, 1);
        // the question body's Techniques line never mints ids: question and
        // annotation bodies can embed sample-derived text
        assert!(!outcomes.detectors.contains_key("T1105"));
    }

    /// Disproving evidence outranks support on the same technique
    #[test]
    fn disproving_evidence_marks_a_lead_disputed() {
        let posts = vec![post(
            "fn-00402000",
            DETECTOR_AUTHOR,
            "Techniques: T1140.",
            Kind::Statement,
        )];
        let comments = vec![
            comment(
                1,
                ParentRef::Post("fn-00402000".to_owned()),
                "ReverseEngineer(Io)",
                "Matches the decoder shape.",
                Stance::Supports,
            ),
            comment(
                2,
                ParentRef::Post("fn-00402000".to_owned()),
                "ReverseEngineer(Europa)",
                "No: that buffer is a font table.",
                Stance::Disproves,
            ),
        ];
        let outcomes = fold_forum(&snapshot(posts, comments, Vec::new()));
        // one thread, one technique, graded disputed despite the support
        assert_eq!(outcomes.detectors["T1140"].disputed, 1);
        assert_eq!(outcomes.detectors["T1140"].confirmed, 0);
    }

    /// A missing forum directory folds as an empty forum, not an error
    #[tokio::test]
    async fn missing_forums_fold_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcomes = read_forum(&dir.path().join("no-such-session")).await;
        assert_eq!(outcomes.forum.posts, 0);
        assert!(outcomes.detectors.is_empty());
    }

    /// Episode ids restart per process; the restore barrier scopes them apart
    #[test]
    fn restore_barriers_scope_episode_ids_apart() {
        let mut fold = TelemetryFold::default();
        // run 1: main#1 spends its budget and the process dies mid-pause
        fold.observe(
            &TranscriptItem::LlmRetry {
                agent: "main".to_owned(),
                retry_id: "main#1".to_owned(),
                attempt: 12,
                max_attempts: 12,
                error: "429".to_owned(),
                request_id: None,
                backoff_ms: 100,
            },
            Some(1_000),
        );
        fold.observe(
            &TranscriptItem::LlmRetryResolved {
                agent: "main".to_owned(),
                retry_id: "main#1".to_owned(),
                attempts: 12,
                succeeded: false,
            },
            Some(2_000),
        );
        fold.observe(
            &TranscriptItem::ProviderPaused {
                pause: PauseKind::RetryBudgetExhausted,
                agent: "main".to_owned(),
                error: "429".to_owned(),
                request_id: None,
            },
            Some(3_000),
        );
        // run 2 resumes: ids restart at main#1, succeed, and pause cleanly
        fold.observe(
            &TranscriptItem::SessionResumed { interrupted: true },
            Some(4_000),
        );
        fold.observe(
            &TranscriptItem::LlmRetry {
                agent: "main".to_owned(),
                retry_id: "main#1".to_owned(),
                attempt: 1,
                max_attempts: 12,
                error: "429".to_owned(),
                request_id: None,
                backoff_ms: 100,
            },
            Some(5_000),
        );
        fold.observe(
            &TranscriptItem::LlmRetryResolved {
                agent: "main".to_owned(),
                retry_id: "main#1".to_owned(),
                attempts: 2,
                succeeded: true,
            },
            Some(6_000),
        );
        fold.observe(
            &TranscriptItem::ProviderPaused {
                pause: PauseKind::Auth,
                agent: "main".to_owned(),
                error: "401".to_owned(),
                request_id: None,
            },
            Some(7_000),
        );
        fold.observe(
            &TranscriptItem::ProviderResumed {
                pause: PauseKind::Auth,
                recovered: true,
                paused_ms: 1_000,
                probe_attempts: 0,
            },
            Some(8_000),
        );
        let summary = fold.summarize(None);
        // two distinct episodes with one exhaustion, not one overwritten key
        assert_eq!(summary.provider_stalls.retry_episodes, 2);
        assert_eq!(summary.provider_stalls.retries_exhausted, 1);
        // the dead run's dangling pause closed at the barrier, so the session
        // does not read as ending inside a pause
        assert!(!summary.provider_stalls.open_pause);
        // the dead pause still counted; only the open flag was healed
        assert_eq!(
            summary.provider_stalls.pauses["retry_budget_exhausted"].count,
            1
        );
        assert_eq!(summary.provider_stalls.pauses["auth"].count, 1);
    }

    /// A scrub counts rescued only when its own run's episode succeeded
    #[test]
    fn scrub_rescues_never_cross_restore_barriers() {
        let mut fold = TelemetryFold::default();
        // run 1: a scrub on main#1 whose episode never resolves (hard death)
        fold.observe(
            &TranscriptItem::LlmScrubbed {
                agent: "main".to_owned(),
                level: 1,
                retry_id: Some("main#1".to_owned()),
                request_id: None,
            },
            Some(1_000),
        );
        // run 2: an unrelated episode that reuses the id and succeeds
        fold.observe(
            &TranscriptItem::SessionResumed { interrupted: false },
            Some(2_000),
        );
        fold.observe(
            &TranscriptItem::LlmRetry {
                agent: "main".to_owned(),
                retry_id: "main#1".to_owned(),
                attempt: 1,
                max_attempts: 12,
                error: "429".to_owned(),
                request_id: None,
                backoff_ms: 100,
            },
            Some(3_000),
        );
        fold.observe(
            &TranscriptItem::LlmRetryResolved {
                agent: "main".to_owned(),
                retry_id: "main#1".to_owned(),
                attempts: 2,
                succeeded: true,
            },
            Some(4_000),
        );
        let summary = fold.summarize(None);
        // the scrub stays unrescued: run 2's success is a different episode
        assert_eq!(summary.decision_points.llm_scrubbed, 1);
        assert_eq!(summary.decision_points.scrub_rescued, 0);
    }

    /// Only an anonymized session's hex project name ships as the sample id
    #[test]
    fn binary_identity_requires_the_anonymized_gate() {
        // a hand-named project that merely looks like a digest is a file name
        let hex = "a".repeat(64);
        let meta = SessionMeta {
            version: 1,
            name: "case".to_owned(),
            user_named: true,
            renamed: false,
            created_at: chrono::Utc::now(),
            binary: std::path::PathBuf::from(&hex),
            project_name: hex.clone(),
            anonymized: false,
            extra_samples: Vec::new(),
        };
        let fold = TelemetryFold::default();
        assert!(fold.summarize(Some(&meta)).binary_sha256.is_none());
        // the staged anonymized copy's digest is the sample's own identity
        let meta = SessionMeta {
            anonymized: true,
            ..meta
        };
        assert_eq!(
            fold.summarize(Some(&meta)).binary_sha256.as_deref(),
            Some(hex.as_str())
        );
    }

    /// Detector post names classify into the closed vocabulary only
    #[test]
    fn name_classes_are_closed_vocabulary() {
        // every class is a &'static str, so hostile names can only pick one
        let cases = [
            ("fn-00401000", (None, "fn")),
            ("s2-fn-00401000", (Some(2), "fn")),
            ("file-level", (None, "file_level")),
            ("s3-runtime-noise", (Some(3), "runtime_noise")),
            ("primary-not-loaded", (None, "primary_not_loaded")),
            ("q-unpack", (None, "q-unpack")),
            ("s2-q-container-payload", (Some(2), "q-container-payload")),
            ("q-host-gate-4f2c", (None, "q-host-gate")),
            // a promoted script's question name is analyst prose: class only
            ("q-CANARY-custom", (None, "script_question")),
            // a script name merely sharing the host-gate prefix is not native
            ("q-host-gateway-check", (None, "script_question")),
            // a hostile fn- name that is not hex is not the fn class
            ("fn-CANARY", (None, "other")),
            ("anything-else", (None, "other")),
        ];
        for (name, expected) in cases {
            assert_eq!(name_class(name), expected, "for name {name:?}");
        }
    }

    /// Entries list unresolved-first, cap deterministically, and align with
    /// their local-only prose twins
    #[test]
    fn entries_sort_cap_and_align_with_detail() {
        // a solved user question, an unsolved agent comment-question, a
        // blocked detector question with one unmet task, and one met task
        let posts = vec![
            post(
                "user-question",
                thorpal::USER_AUTHOR,
                "What packer is this?",
                Kind::Question(QuestionStatus::Solved),
            ),
            post(
                "s2-q-unpack",
                DETECTOR_AUTHOR,
                "Can the payload be unpacked?\nTechniques: T1027.",
                Kind::Question(QuestionStatus::Blocked),
            ),
        ];
        let comments = vec![Comment {
            id: 1,
            parent: ParentRef::Post("user-question".to_owned()),
            author: "ReverseEngineer(Io)".to_owned(),
            body: "Which import resolves this?\nMore detail.".to_owned(),
            stance: Stance::Neutral,
            kind: Kind::Question(QuestionStatus::Unsolved),
            updated: 9,
        }];
        let tasks = vec![
            Task {
                id: 1,
                parent: ParentRef::Post("s2-q-unpack".to_owned()),
                author: DETECTOR_AUTHOR.to_owned(),
                description: "identify the unpack routine".to_owned(),
                completed: None,
                updated: 3,
            },
            Task {
                id: 2,
                parent: ParentRef::Post("user-question".to_owned()),
                author: "ReverseEngineer(Io)".to_owned(),
                description: "list the callers".to_owned(),
                completed: Some(TaskCompletion {
                    comment: 1,
                    author: "ReverseEngineer(Io)".to_owned(),
                }),
                updated: 8,
            },
        ];
        let (outcomes, detail) = fold_forum_local(&snapshot(posts, comments, tasks));
        let entries = &outcomes.forum.entries;
        // unresolved first: unsolved, blocked, unmet; settled work last
        let statuses: Vec<EntryStatus> = entries.iter().map(|entry| entry.status).collect();
        assert_eq!(
            statuses,
            [
                EntryStatus::Unsolved,
                EntryStatus::Blocked,
                EntryStatus::Unmet,
                // settled work also orders newest-activity-first
                EntryStatus::Met,
                EntryStatus::Solved,
            ]
        );
        // the detector question classifies, scopes, and counts its gate
        let blocked = &entries[1];
        assert_eq!(blocked.author, AuthorClass::Detector);
        assert_eq!(blocked.name_class.as_deref(), Some("q-unpack"));
        assert_eq!(blocked.sample, Some(2));
        assert_eq!(blocked.tasks, 1);
        assert_eq!(blocked.tasks_unmet, 1);
        // the agent comment-question carries its role and parent kind
        let unsolved = &entries[0];
        assert_eq!(unsolved.author, AuthorClass::Agent);
        assert_eq!(unsolved.role.as_deref(), Some("ReverseEngineer"));
        assert_eq!(unsolved.parent, Some(NodeKind::Post));
        // the local prose twins align index-for-index
        assert_eq!(detail.entries.len(), entries.len());
        assert_eq!(detail.entries[0].title, "Which import resolves this?");
        // the post fixture derives its title from the name
        assert_eq!(detail.entries[1].title, "s2-q-unpack title");
        // the comment-question row ships no title (bodies never ship); its
        // prose lives only in the local twin above
        assert_eq!(entries[0].title, None);
        // post and task rows keep their titles for finalize to gate
        assert!(entries[1..].iter().all(|entry| entry.title.is_some()));
    }

    /// The entry cap keeps the unresolved head and counts what it dropped
    #[test]
    fn entries_cap_and_count_drops() {
        // 300 unsolved questions blow the per-session cap
        let posts: Vec<Post> = (0..300)
            .map(|i| {
                let mut question = post(
                    &format!("q-{i}"),
                    "ReverseEngineer(Io)",
                    &format!("question {i}"),
                    Kind::Question(QuestionStatus::Unsolved),
                );
                question.updated = i as u64;
                question
            })
            .collect();
        let outcomes = fold_forum(&snapshot(posts, Vec::new(), Vec::new()));
        assert_eq!(outcomes.forum.entries.len(), 256);
        assert_eq!(outcomes.forum.entries_dropped, 44);
        // newest activity survives the cap
        assert_eq!(outcomes.forum.entries[0].updated, 299);
    }

    /// The seeding watermark marks untouched detector entries, and legacy
    /// revision-0 stores decline to say
    #[test]
    fn untouched_tracks_the_seeding_watermark() {
        // two detector questions seeded at revisions 1 and 2; an agent then
        // engages the second (reply at revision 5)
        let posts = vec![
            {
                let mut seeded = post(
                    "q-unpack",
                    DETECTOR_AUTHOR,
                    "Can it be unpacked?",
                    Kind::Question(QuestionStatus::Unsolved),
                );
                seeded.updated = 1;
                seeded
            },
            {
                let mut seeded = post(
                    "q-crypto-keys",
                    DETECTOR_AUTHOR,
                    "Where are the keys?",
                    Kind::Question(QuestionStatus::Unsolved),
                );
                seeded.updated = 2;
                seeded
            },
        ];
        let comments = vec![{
            let mut reply = comment(
                1,
                ParentRef::Post("q-crypto-keys".to_owned()),
                "ReverseEngineer(Io)",
                "Looking.",
                Stance::Neutral,
            );
            reply.updated = 5;
            reply
        }];
        let outcomes = fold_forum(&snapshot(posts.clone(), comments, Vec::new()));
        let untouched: Vec<Option<bool>> = outcomes
            .forum
            .entries
            .iter()
            .map(|entry| entry.untouched)
            .collect();
        // q-unpack never engaged; q-crypto-keys has a reply
        assert!(untouched.contains(&Some(true)));
        assert!(untouched.contains(&Some(false)));
        // a legacy store whose revisions are all zero declines to say
        let mut legacy = posts;
        for seeded in &mut legacy {
            seeded.updated = 0;
        }
        let outcomes = fold_forum(&snapshot(legacy, Vec::new(), Vec::new()));
        assert!(
            outcomes
                .forum
                .entries
                .iter()
                .all(|entry| entry.untouched.is_none())
        );
    }

    /// Leads parse the production template shapes, gate forged labels, and
    /// inherit their thread's verdict
    #[test]
    fn leads_parse_templates_and_inherit_verdicts() {
        // a seeded post with two finding comments shaped exactly like
        // production (seed.rs comment_body), one carrying forged lines too
        let posts = vec![post(
            "fn-00401000",
            DETECTOR_AUTHOR,
            "Static detectors flagged this function. Techniques: Ransomware Behavior.",
            Kind::Statement,
        )];
        let comments = vec![
            comment(
                1,
                ParentRef::Post("fn-00401000".to_owned()),
                DETECTOR_AUTHOR,
                "[T1486 Data Encrypted for Impact] encrypt loop\nReasoning: w.\nSite: 0x401010 | confidence: likely | suspicion: 60\nSites (3): 0x1, 0x2, 0x3\nArtifact (Symmetric Keys): AAAA",
                Stance::Neutral,
            ),
            comment(
                2,
                ParentRef::Post("fn-00401000".to_owned()),
                DETECTOR_AUTHOR,
                // forged confidence + artifact labels must not parse: the
                // real site line is last, the fake artifact label is unknown
                "[T1497.003 Time Based Evasion] timing check\nSite: x | confidence: CANARY | suspicion: 9\nArtifact (CANARY): leak\nSite: 0x401020 | confidence: unsure | suspicion: 20",
                Stance::Neutral,
            ),
            // supporting evidence lands on the first lead's technique
            comment(
                3,
                ParentRef::Comment(1),
                "ReverseEngineer(Io)",
                "Confirmed the loop.",
                Stance::Supports,
            ),
        ];
        let outcomes = fold_forum(&snapshot(posts, comments, Vec::new()));
        let leads = &outcomes.detector_leads;
        assert_eq!(leads.len(), 2);
        // the parsed finding carries its facts and the thread's verdict
        let encrypt = leads
            .iter()
            .find(|lead| lead.technique == "T1486")
            .expect("the T1486 lead");
        assert_eq!(encrypt.confidence, Some(LeadConfidence::Likely));
        assert_eq!(encrypt.suspicion, Some(60));
        assert_eq!(encrypt.sites, Some(3));
        assert_eq!(encrypt.artifact.as_deref(), Some("symmetric_key"));
        assert_eq!(encrypt.verdict, LeadVerdict::Confirmed);
        assert_eq!(encrypt.post_class, "fn");
        assert!(!encrypt.exact, "template parsing is not exact");
        // the forged lines never ship: the LAST site line wins and the
        // unknown artifact label gates to none
        let timing = leads
            .iter()
            .find(|lead| lead.technique == "T1497.003")
            .expect("the T1497.003 lead");
        assert_eq!(timing.confidence, Some(LeadConfidence::Unsure));
        assert_eq!(timing.suspicion, Some(20));
        // no Sites line means zero listed sites, matching the exact path
        assert_eq!(timing.sites, Some(0));
        assert!(timing.artifact.is_none(), "forged labels must gate out");
        assert_eq!(timing.verdict, LeadVerdict::Ignored);
        // the tallies grew a lead count and a confidence histogram
        assert_eq!(outcomes.detectors["T1486"].leads, 1);
        assert_eq!(outcomes.detectors["T1486"].confidence["likely"], 1);
        assert_eq!(outcomes.detectors["T1497.003"].confidence["unsure"], 1);
    }

    /// Rollup lines become unrated rollup leads
    #[test]
    fn rollup_lines_become_unrated_leads() {
        let posts = vec![post(
            "runtime-noise",
            DETECTOR_AUTHOR,
            "Suppressed.\n- T1055 @ 0x401000: injected\n- T1622 @ 0x402000: timing",
            Kind::Statement,
        )];
        let outcomes = fold_forum(&snapshot(posts, Vec::new(), Vec::new()));
        assert_eq!(outcomes.detector_leads.len(), 2);
        assert!(
            outcomes
                .detector_leads
                .iter()
                .all(|lead| lead.shape == LeadShape::Rollup && lead.confidence.is_none())
        );
        assert_eq!(outcomes.detectors["T1055"].confidence["unrated"], 1);
    }

    /// Structured provenance outranks template parsing and marks leads exact
    #[test]
    fn structured_provenance_outranks_template_parsing() {
        use thorpal::NodeProvenance;
        let posts = vec![post(
            "fn-00401000",
            DETECTOR_AUTHOR,
            "Static detectors flagged this function.",
            Kind::Statement,
        )];
        // the comment's TEXT says unsure/20, the structured record says
        // fact/90 from the yara source -- the record must win
        let comments = vec![comment(
            1,
            ParentRef::Post("fn-00401000".to_owned()),
            DETECTOR_AUTHOR,
            "[T1486 Data Encrypted for Impact] enc\nSite: 0x1 | confidence: unsure | suspicion: 20",
            Stance::Neutral,
        )];
        let mut snap = snapshot(posts, comments, Vec::new());
        snap.detector_provenance = vec![NodeProvenance {
            node: ParentRef::Comment(1),
            shape: "finding".to_owned(),
            question_origin: None,
            findings: vec![thorpal::FindingProvenance {
                technique: "T1486".to_owned(),
                confidence: "fact".to_owned(),
                suspicion: 90,
                source: "yara".to_owned(),
                detection: Some("my_rule".to_owned()),
                sites: 5,
                artifact_class: None,
                suppressed: false,
            }],
        }];
        let outcomes = fold_forum(&snap);
        assert_eq!(outcomes.detector_leads.len(), 1);
        let lead = &outcomes.detector_leads[0];
        assert!(lead.exact);
        assert_eq!(lead.confidence, Some(LeadConfidence::Fact));
        assert_eq!(lead.suspicion, Some(90));
        assert_eq!(lead.sites, Some(5));
        assert_eq!(lead.source.as_deref(), Some("yara"));
        // a non-native source's detection name stays local
        assert!(lead.detection.is_none());
    }

    /// Question bodies never mint technique ids, whatever shapes they hold
    #[test]
    fn question_bodies_never_mint_ids() {
        // a container question embedding an archive entry name shaped like a
        // rollup line, plus an id-shaped Techniques list
        let posts = vec![post(
            "q-container-payload",
            DETECTOR_AUTHOR,
            "Entries:\n- ops-team @ q3.doc (123 bytes)\nTechniques: T1027.",
            Kind::Question(QuestionStatus::Unsolved),
        )];
        let outcomes = fold_forum(&snapshot(posts, Vec::new(), Vec::new()));
        // neither the entry name nor the listed id reaches any tally
        assert!(
            outcomes.detectors.is_empty(),
            "got {:?}",
            outcomes.detectors
        );
        assert!(outcomes.forum.entries[0].techniques.is_empty());
        assert!(outcomes.detector_leads.is_empty());
    }

    /// Sampled detector titles lose their harness-appended file-name suffix
    #[test]
    fn sampled_titles_lose_the_file_name_suffix() {
        let mut scoped = post(
            "s2-q-unpack",
            DETECTOR_AUTHOR,
            "body",
            Kind::Question(QuestionStatus::Unsolved),
        );
        scoped.title = "Can it unpack? — payload_acme.dll".to_owned();
        // a sibling file name containing the separator itself must still cut
        // whole: cutting at the first match leaves no file-name fragment
        let mut dashed = post(
            "s3-q-crypto-keys",
            DETECTOR_AUTHOR,
            "body",
            Kind::Question(QuestionStatus::Unsolved),
        );
        dashed.title = "Where are the keys? — invoice — april.doc".to_owned();
        // an unsampled detector title and an agent title keep their dashes
        let mut plain = post(
            "q-prior-toolkit",
            DETECTOR_AUTHOR,
            "body",
            Kind::Question(QuestionStatus::Unsolved),
        );
        plain.title = "Anti-analysis — what gates execution?".to_owned();
        let mut agent = post(
            "agent-question",
            "ReverseEngineer(Io)",
            "body",
            Kind::Question(QuestionStatus::Unsolved),
        );
        agent.title = "config — where is it?".to_owned();
        let (outcomes, detail) = fold_forum_local(&snapshot(
            vec![scoped, dashed, plain, agent],
            Vec::new(),
            Vec::new(),
        ));
        let title_of = |class: Option<&str>, role: Option<&str>| {
            outcomes
                .forum
                .entries
                .iter()
                .find(|entry| entry.name_class.as_deref() == class && entry.role.as_deref() == role)
                .and_then(|entry| entry.title.clone())
                .expect("the entry's title")
        };
        // the sampled title drops the suffix; the local twin keeps it whole
        assert_eq!(title_of(Some("q-unpack"), None), "Can it unpack?");
        assert!(
            detail
                .entries
                .iter()
                .any(|entry| entry.title == "Can it unpack? — payload_acme.dll")
        );
        // a dashed file name leaves no fragment: the first separator cuts
        assert_eq!(title_of(Some("q-crypto-keys"), None), "Where are the keys?");
        // unsampled and agent titles pass through untouched
        assert_eq!(
            title_of(Some("q-prior-toolkit"), None),
            "Anti-analysis — what gates execution?"
        );
        assert_eq!(
            title_of(None, Some("ReverseEngineer")),
            "config — where is it?"
        );
    }

    /// Forged provenance class fields gate out of the exact path
    #[test]
    fn forged_provenance_class_fields_gate_out() {
        use thorpal::NodeProvenance;
        let posts = vec![post(
            "fn-00401000",
            DETECTOR_AUTHOR,
            "Static detectors flagged this function.",
            Kind::Statement,
        )];
        let comments = vec![comment(
            1,
            ParentRef::Post("fn-00401000".to_owned()),
            DETECTOR_AUTHOR,
            "[T1486 Data Encrypted for Impact] enc",
            Stance::Neutral,
        )];
        let mut snap = snapshot(posts, comments, Vec::new());
        // a forged store claims prose-bearing class fields
        snap.detector_provenance = vec![NodeProvenance {
            node: ParentRef::Comment(1),
            shape: "finding".to_owned(),
            question_origin: None,
            findings: vec![
                thorpal::FindingProvenance {
                    technique: "T1486".to_owned(),
                    confidence: "fact".to_owned(),
                    suspicion: 90,
                    source: "CANARY-source".to_owned(),
                    detection: Some("CANARY-detection".to_owned()),
                    sites: 0,
                    artifact_class: Some("CANARY-class".to_owned()),
                    suppressed: false,
                },
                // a forged store can also CLAIM a native source to smuggle
                // prose through the detection name; the shape gate blocks it
                thorpal::FindingProvenance {
                    technique: "T1486".to_owned(),
                    confidence: "fact".to_owned(),
                    suspicion: 90,
                    source: "native".to_owned(),
                    detection: Some("CANARY Detection Prose!".to_owned()),
                    sites: 0,
                    artifact_class: None,
                    suppressed: false,
                },
            ],
        }];
        let outcomes = fold_forum(&snap);
        let lead = &outcomes.detector_leads[0];
        // every class field outside its closed vocabulary gates to none
        assert!(lead.source.is_none(), "forged sources must gate out");
        assert!(lead.artifact.is_none(), "forged class keys must gate out");
        assert!(lead.detection.is_none(), "non-native detections stay local");
        // the claimed-native lead keeps its source but drops the prose name
        let claimed = &outcomes.detector_leads[1];
        assert_eq!(claimed.source.as_deref(), Some("native"));
        assert!(claimed.detection.is_none(), "prose names must gate out");
    }

    /// Emulation-sourced provenance keeps its source like every other stage
    #[test]
    fn emulation_sources_pass_the_closed_gate() {
        use thorpal::NodeProvenance;
        let posts = vec![post(
            "fn-00401000",
            DETECTOR_AUTHOR,
            "Static detectors flagged this function.",
            Kind::Statement,
        )];
        let comments = vec![comment(
            1,
            ParentRef::Post("fn-00401000".to_owned()),
            DETECTOR_AUTHOR,
            "[T1140 Deobfuscate/Decode] decoded in emulation",
            Stance::Neutral,
        )];
        let mut snap = snapshot(posts, comments, Vec::new());
        // the emulation stage seeds findings with its own source key
        snap.detector_provenance = vec![NodeProvenance {
            node: ParentRef::Comment(1),
            shape: "finding".to_owned(),
            question_origin: None,
            findings: vec![thorpal::FindingProvenance {
                technique: "T1140".to_owned(),
                confidence: "likely".to_owned(),
                suspicion: 55,
                source: thorpal::HitSource::Emulation.key().to_owned(),
                detection: None,
                sites: 1,
                artifact_class: None,
                suppressed: false,
            }],
        }];
        let outcomes = fold_forum(&snap);
        // the source survives the gate instead of reading as forged
        assert_eq!(
            outcomes.detector_leads[0].source.as_deref(),
            Some("emulation")
        );
    }

    /// Script-origin provenance overrides a native-looking question name
    #[test]
    fn script_origin_provenance_overrides_native_names() {
        use thorpal::NodeProvenance;
        // a promoted script reused a native question name for its question
        let posts = vec![post(
            "q-unpack",
            DETECTOR_AUTHOR,
            "script question body",
            Kind::Question(QuestionStatus::Unsolved),
        )];
        let mut snap = snapshot(posts, Vec::new(), Vec::new());
        snap.detector_provenance = vec![NodeProvenance {
            node: ParentRef::Post("q-unpack".to_owned()),
            shape: "question".to_owned(),
            question_origin: Some("script".to_owned()),
            findings: Vec::new(),
        }];
        let outcomes = fold_forum(&snap);
        // the recorded origin outranks the name heuristic
        assert_eq!(
            outcomes.forum.entries[0].name_class.as_deref(),
            Some("script_question")
        );
    }

    /// Titles flatten to one bounded line before they can reach a bundle
    #[test]
    fn titles_flatten_and_cap_for_bundles() {
        // a multi-line task description flattens to one spaced line
        let tasks = vec![Task {
            id: 1,
            parent: ParentRef::Post("agent-note".to_owned()),
            author: "ReverseEngineer(Io)".to_owned(),
            description: format!("line one\nline two\t indented\n{}", "x".repeat(300)),
            completed: None,
            updated: 1,
        }];
        let outcomes = fold_forum(&snapshot(Vec::new(), Vec::new(), tasks));
        let title = outcomes.forum.entries[0].title.as_deref().expect("a title");
        assert!(
            !title.contains('\n'),
            "titles must be single-line: {title:?}"
        );
        assert!(
            title.starts_with("line one line two indented"),
            "got {title:?}"
        );
        assert!(title.chars().count() <= 100, "titles must cap: {title:?}");
    }

    /// A thread verdict counts as engagement, overriding the untouched flag
    #[test]
    fn thread_verdicts_override_untouched() {
        // a bucket post whose finding comment nobody replied to directly,
        // but whose post-level thread got supporting evidence
        let posts = vec![{
            let mut bucket = post(
                "fn-00401000",
                DETECTOR_AUTHOR,
                "Flagged.\nTechniques: T1486.",
                Kind::Statement,
            );
            bucket.updated = 1;
            bucket
        }];
        let comments = vec![
            {
                let mut finding = comment(
                    1,
                    ParentRef::Post("fn-00401000".to_owned()),
                    DETECTOR_AUTHOR,
                    "[T1486 Data Encrypted for Impact] enc",
                    Stance::Neutral,
                );
                finding.updated = 2;
                finding
            },
            {
                let mut support = comment(
                    2,
                    ParentRef::Post("fn-00401000".to_owned()),
                    "ReverseEngineer(Io)",
                    "Confirmed the loop.",
                    Stance::Supports,
                );
                support.updated = 5;
                support
            },
        ];
        let outcomes = fold_forum(&snapshot(posts, comments, Vec::new()));
        let lead = outcomes
            .detector_leads
            .iter()
            .find(|lead| lead.shape == LeadShape::Finding)
            .expect("the finding lead");
        // the confirmed verdict proves engagement, so untouched reads false
        // even though the finding comment's own subtree is silent
        assert_eq!(lead.verdict, LeadVerdict::Confirmed);
        assert_eq!(lead.untouched, Some(false));
    }

    /// Rollup leads forward their recorded artifact and engagement facts
    #[test]
    fn rollup_leads_forward_artifacts_and_engagement() {
        use thorpal::NodeProvenance;
        let posts = vec![{
            let mut rollup = post(
                "runtime-noise",
                DETECTOR_AUTHOR,
                "Suppressed.",
                Kind::Statement,
            );
            rollup.updated = 1;
            rollup
        }];
        // an agent reply engages the rollup post itself
        let comments = vec![{
            let mut reply = comment(
                1,
                ParentRef::Post("runtime-noise".to_owned()),
                "ReverseEngineer(Io)",
                "Looking at the noise.",
                Stance::Neutral,
            );
            reply.updated = 4;
            reply
        }];
        let mut snap = snapshot(posts, comments, Vec::new());
        snap.detector_provenance = vec![NodeProvenance {
            node: ParentRef::Post("runtime-noise".to_owned()),
            shape: "rollup".to_owned(),
            question_origin: None,
            findings: vec![thorpal::FindingProvenance {
                technique: "T1027".to_owned(),
                confidence: "unsure".to_owned(),
                suspicion: 20,
                source: "native".to_owned(),
                detection: Some("packing".to_owned()),
                sites: 2,
                artifact_class: Some("symmetric_key".to_owned()),
                suppressed: true,
            }],
        }];
        let outcomes = fold_forum(&snap);
        let lead = &outcomes.detector_leads[0];
        // the recorded artifact class and producer survive, gated as usual
        assert_eq!(lead.artifact.as_deref(), Some("symmetric_key"));
        assert_eq!(lead.detection.as_deref(), Some("packing"));
        // the rollup post's engagement reaches its leads
        assert_eq!(lead.replies, 1);
        assert_eq!(lead.untouched, Some(false));
    }

    /// Roster lines fold into per-detection fleet tallies
    #[test]
    fn detectors_run_lines_fold_into_tallies() {
        use thorpal::DetectorRunLine;
        let mut fold = TelemetryFold::default();
        for (hits, failed) in [(3u32, false), (0u32, true)] {
            fold.observe(
                &TranscriptItem::DetectorsRun {
                    sample: None,
                    detectors: vec![DetectorRunLine {
                        name: "packing".to_owned(),
                        technique: "T1027.002".to_owned(),
                        hits,
                        failed,
                    }],
                    version: "9.9.9".to_owned(),
                },
                Some(1_000),
            );
        }
        let summary = fold.summarize(None);
        let tally = &summary.detectors_run["packing"];
        assert_eq!(tally.runs, 2);
        assert_eq!(tally.hits, 3);
        assert_eq!(tally.failed, 1);
        assert_eq!(tally.technique, "T1027.002");
    }

    /// Loop detection clusters whole multi-line signatures, not first lines
    #[test]
    fn loops_cluster_on_whole_multiline_signatures() {
        // three identical multi-line failures flag a loop
        let mut fold = TelemetryFold::default();
        for call in 0..3 {
            fold.observe(
                &TranscriptItem::ToolResult {
                    agent: "main".to_owned(),
                    call_id: format!("c{call}"),
                    tool: "run_script".to_owned(),
                    result: "script error\n  at frame 1\n  at frame 2".to_owned(),
                    success: false,
                },
                Some(1_000 + call),
            );
        }
        let summary = fold.summarize(None);
        assert_eq!(summary.signatures.len(), 1);
        assert!(
            summary.signatures[0].looped,
            "three identical failures loop"
        );
        // three failures that differ past the first line never flag
        let mut fold = TelemetryFold::default();
        for call in 0..3u8 {
            fold.observe(
                &TranscriptItem::ToolResult {
                    agent: "main".to_owned(),
                    call_id: format!("c{call}"),
                    tool: "run_script".to_owned(),
                    result: format!("script error\n  at frame {}", char::from(b'a' + call)),
                    success: false,
                },
                Some(1_000 + u64::from(call)),
            );
        }
        let summary = fold.summarize(None);
        assert_eq!(
            summary.signatures.len(),
            3,
            "distinct tails are distinct clusters"
        );
        assert!(summary.signatures.iter().all(|cluster| !cluster.looped));
    }

    /// Native ATT&CK ids pass the shape check; minted ids do not
    #[test]
    fn native_technique_shape_is_exact() {
        assert!(is_native_technique("T1027"));
        assert!(is_native_technique("T1497.003"));
        assert!(!is_native_technique("T102"));
        assert!(!is_native_technique("T1497.03"));
        assert!(!is_native_technique("Capa.Rule_1"));
        assert!(!is_native_technique("YARA_SUSP_XOR"));
    }
}
