//! Forum-promotion tracking for decoded constants.
//!
//! Decode tools routinely recover protocol verbs, URLs, keys, and other
//! constants that an agent reads in the tool result and then never posts to
//! the shared forum -- so the value dies before the report (the Forum Evidence
//! appendix can only preserve what reached the forum). This module watches
//! every successful tool result for recovered constants, remembers which ones
//! the forum has never carried, and nudges the model twice over:
//!
//! 1. Inline: a short advisory paragraph appended to the tool result the
//!    moment an unposted constant is recovered (at most once per constant and
//!    a few times per session, so it steers without nagging).
//! 2. At finish: a one-shot refusal listing still-unposted constants, mirroring
//!    the rename/promote nudges so a stubborn run can never deadlock.
//!
//! Extraction reads each tool's *structured* result JSON (never the rendered
//! text), so the qualifying fields are pinned by the result types themselves.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError};

use rig::agent::hook::ToolResultAction;
use rig::message::ToolResultContent;
use rig::tool::{ToolContext, ToolOutput, ToolResult};

use crate::thorpal::tools::forum::ForumState;

/// The most inline constant nudges one session will ever emit
pub const MAX_CONSTANT_NUDGES: usize = 4;

/// The shortest constant worth tracking, in characters
pub const MIN_CONSTANT_LEN: usize = 6;

/// The longest constant worth tracking, in characters
pub const MAX_CONSTANT_LEN: usize = 128;

/// The most distinct constants one session will track
pub const MAX_TRACKED_CONSTANTS: usize = 256;

/// The most unposted constants the finish-time refusal lists verbatim
pub const MAX_FINISH_CONSTANTS: usize = 5;

/// The most constants one inline nudge lists verbatim
const NUDGE_LIST_CAP: usize = 3;

/// The most constants harvested from any one decode-shaped result
const MAX_PER_RESULT: usize = 8;

/// The most strings harvested from one `run_script` result value
const MAX_SCRIPT_STRINGS: usize = 64;

/// How deep the `run_script` result-value walk recurses
const SCRIPT_WALK_DEPTH: usize = 4;

/// How much of a decode output must be printable before its text qualifies
const MIN_PRINTABLE_RATIO: f64 = 0.8;

/// How often one constant has been recovered, and whether it was nudged yet
#[derive(Debug, Clone, Copy)]
struct SeenState {
    /// How many tool results have carried this constant
    count: usize,
    /// Whether an inline nudge already named this constant
    nudged: bool,
}

/// One cached forum-presence verdict for a constant
#[derive(Debug, Clone, Copy)]
struct PresenceCache {
    /// The forum revision the verdict was computed at
    revision: u64,
    /// Whether the forum carried the constant at that revision
    present: bool,
}

/// The session-scoped ledger of decoded constants and their nudge state
///
/// One tracker is shared across the main agent, every sub-agent, and every
/// swarm member through the run's `ToolContext`, exactly like the forum state.
/// It is deliberately in-memory only (the same precedent as the rename and
/// promote latches): on resume the restored forum makes already-posted
/// constants read as present, so nothing is nudged twice about posted values,
/// and the latches simply re-arm.
pub struct ConstantTracker {
    /// Every tracked constant with its sighting count and nudge state
    seen: Mutex<HashMap<String, SeenState>>,
    /// How many inline nudges this session has emitted
    nudges_issued: AtomicUsize,
    /// A one-shot latch so the `finish` tool nudges about unposted constants
    /// exactly once rather than deadlocking a run that declines to post them
    finish_nudged: AtomicBool,
    /// Cached forum-presence verdicts, keyed by constant
    presence: Mutex<HashMap<String, PresenceCache>>,
}

impl Default for ConstantTracker {
    /// Builds an empty tracker, same as [`ConstantTracker::new`]
    fn default() -> Self {
        Self::new()
    }
}

impl ConstantTracker {
    /// Creates a fresh, empty constant tracker
    pub fn new() -> Self {
        ConstantTracker {
            seen: Mutex::new(HashMap::new()),
            nudges_issued: AtomicUsize::new(0),
            finish_nudged: AtomicBool::new(false),
            presence: Mutex::new(HashMap::new()),
        }
    }

    /// Pulls the shared constant tracker out of a tool call's [`ToolContext`]
    ///
    /// A missing tracker is not an error -- a consumer that never installed one
    /// simply runs without constant nudges.
    ///
    /// # Arguments
    ///
    /// * `context` - The tool call context the run was prompted with
    pub fn from_context(context: &ToolContext) -> Option<Arc<ConstantTracker>> {
        // recover the shared tracker, treating a missing slot as nudges-off
        context.get::<Arc<ConstantTracker>>().map(Arc::clone)
    }

    /// Reads and sets the one-shot finish-time constant nudge
    ///
    /// Returns true exactly once, so the `finish` tool can list unposted
    /// constants a single time without ever deadlocking a run that declines
    /// to post them.
    pub fn take_finish_nudge(&self) -> bool {
        // flip the latch and report whether we were the first to do so
        !self.finish_nudged.swap(true, Ordering::SeqCst)
    }

    /// Records sightings and builds the inline nudge for unposted constants
    ///
    /// Every constant is counted; a nudge names a constant at most once per
    /// session, at most [`NUDGE_LIST_CAP`] constants per nudge, and at most
    /// [`MAX_CONSTANT_NUDGES`] nudges per session. Constants the forum already
    /// carries are never nudged (presence verdicts are cached by forum
    /// revision; a "present" verdict is cached forever since posts are never
    /// deleted).
    ///
    /// # Arguments
    ///
    /// * `forum` - The shared forum to test constants against
    /// * `constants` - The normalized constants one tool result recovered
    pub async fn inline_nudge(&self, forum: &ForumState, constants: Vec<String>) -> Option<String> {
        // record every sighting and keep the ones never nudged before
        let mut fresh = Vec::new();
        {
            // a poisoned lock just hands back the data; the sections below are short
            let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
            for value in constants {
                match seen.get_mut(&value) {
                    // a known constant bumps its count and stays a candidate
                    // until some nudge has actually named it
                    Some(state) => {
                        state.count += 1;
                        if !state.nudged && !fresh.contains(&value) {
                            fresh.push(value);
                        }
                    }
                    // a new constant is tracked unless the ledger is full
                    None => {
                        if seen.len() >= MAX_TRACKED_CONSTANTS {
                            continue;
                        }
                        seen.insert(
                            value.clone(),
                            SeenState {
                                count: 1,
                                nudged: false,
                            },
                        );
                        fresh.push(value);
                    }
                }
            }
        }
        // bail quietly once the session-wide nudge budget is spent
        if self.nudges_issued.load(Ordering::SeqCst) >= MAX_CONSTANT_NUDGES {
            return None;
        }
        // keep only constants the forum has never carried, a few at a time
        let mut unposted = Vec::new();
        for value in fresh {
            if !self.forum_has(forum, &value).await {
                unposted.push(value);
            }
            if unposted.len() == NUDGE_LIST_CAP {
                break;
            }
        }
        if unposted.is_empty() {
            return None;
        }
        // claim a nudge slot; a racing claim past the cap stands down
        if self.nudges_issued.fetch_add(1, Ordering::SeqCst) >= MAX_CONSTANT_NUDGES {
            return None;
        }
        // mark the named constants so no later nudge repeats them
        {
            let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
            for value in &unposted {
                if let Some(state) = seen.get_mut(value) {
                    state.nudged = true;
                }
            }
        }
        // render the advisory in the same voice as the long-post notice
        let list = unposted
            .iter()
            .map(|value| format!("`{value}`"))
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!(
            "Note: the recovered constant(s) {list} do not appear anywhere on the \
             shared forum. If a recovered constant is meaningful (a C2 protocol \
             verb, URL, key, or campaign ID), post it with create_post/create_comment \
             so it survives into the report — constants that only live in tool \
             output are lost."
        ))
    }

    /// Lists tracked constants the forum still does not carry
    ///
    /// Used by the finish-time backstop. Constants are ordered most-seen
    /// first (ties alphabetical) so the refusal leads with what the session
    /// recovered most often.
    ///
    /// # Arguments
    ///
    /// * `forum` - The shared forum to test constants against
    /// * `limit` - The most constants to return
    pub async fn unposted(&self, forum: &ForumState, limit: usize) -> Vec<String> {
        // snapshot the ledger so no lock is held across the forum scans
        let mut snapshot: Vec<(String, usize)> = {
            let seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
            seen.iter()
                .map(|(value, state)| (value.clone(), state.count))
                .collect()
        };
        // most-seen first, then alphabetical so the order is deterministic
        snapshot.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        // keep the first `limit` constants the forum has never carried
        let mut unposted = Vec::new();
        for (value, _) in snapshot {
            if !self.forum_has(forum, &value).await {
                unposted.push(value);
                if unposted.len() == limit {
                    break;
                }
            }
        }
        unposted
    }

    /// Tests whether the forum carries a constant, caching by forum revision
    ///
    /// A "present" verdict is cached forever (posts and comments are never
    /// deleted); a "not present" verdict is re-tested once the forum revision
    /// moves.
    ///
    /// # Arguments
    ///
    /// * `forum` - The shared forum to test against
    /// * `value` - The constant to look for
    async fn forum_has(&self, forum: &ForumState, value: &str) -> bool {
        // read the revision first so a cached miss can be tied to it
        let revision = forum.current_revision();
        {
            let presence = self.presence.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(cached) = presence.get(value) {
                // a hit is permanent; a miss holds until the forum changes
                if cached.present || cached.revision == revision {
                    return cached.present;
                }
            }
        }
        // scan the live forum outside any tracker lock
        let present = forum.contains_text(value).await;
        // cache the verdict, bounded by the same ledger cap
        let mut presence = self.presence.lock().unwrap_or_else(PoisonError::into_inner);
        if presence.len() < MAX_TRACKED_CONSTANTS || presence.contains_key(value) {
            presence.insert(value.to_owned(), PresenceCache { revision, present });
        }
        present
    }
}

/// Normalizes one recovered string into a trackable constant
///
/// Trims surrounding whitespace, rejects anything outside the
/// [`MIN_CONSTANT_LEN`]..=[`MAX_CONSTANT_LEN`] character range, and rejects
/// control characters. Case is deliberately preserved: C2 protocol verbs and
/// keys must match the forum verbatim.
///
/// # Arguments
///
/// * `raw` - The recovered string as the tool reported it
pub fn normalize(raw: &str) -> Option<String> {
    // constants are compared verbatim, so only strip the surrounding whitespace
    let trimmed = raw.trim();
    // gate on the character count so multibyte text is measured fairly
    let chars = trimmed.chars().count();
    if !(MIN_CONSTANT_LEN..=MAX_CONSTANT_LEN).contains(&chars) {
        return None;
    }
    // control characters mean the run is byte noise, not a constant
    if trimmed.chars().any(char::is_control) {
        return None;
    }
    Some(trimmed.to_owned())
}

/// Extracts recovered constants from one tool's structured result JSON
///
/// Only tools that recover hidden/decoded content are scanned, and each is
/// read through its own result type's fields -- never by regexing rendered
/// text. Unknown tools yield nothing.
///
/// # Arguments
///
/// * `tool_name` - The tool that produced the result
/// * `args` - The call's effective JSON arguments
/// * `raw` - The tool's structured result JSON
pub fn extract_constants(
    tool_name: &str,
    args: &serde_json::Value,
    raw: &serde_json::Value,
) -> Vec<String> {
    // harvest raw candidate strings per tool, then normalize and dedup once
    let mut candidates = Vec::new();
    match tool_name {
        // decode_bytes always yields a DecodeResult; byte_transform yields a
        // BruteResult for sweeps and a DecodeResult for keyed decodes -- the
        // `candidates` field is what tells the two shapes apart
        "decode_bytes" | "byte_transform" => {
            if let Some(brute) = raw.get("candidates").and_then(|v| v.as_array()) {
                // a sweep's best candidate carries the readable runs
                if let Some(runs) = brute
                    .first()
                    .and_then(|c| c.get("runs"))
                    .and_then(|v| v.as_array())
                {
                    collect_strs(runs, &mut candidates, MAX_PER_RESULT);
                }
            } else {
                // a keyed decode qualifies only when it mostly reads as text
                // AND the caller asked for text: under `emit: "bytes"` the
                // `text` field is a hex rendering, not recovered strings
                let emit = args.get("emit").and_then(|v| v.as_str());
                if matches!(emit, None | Some("strings")) {
                    harvest_decode_text(raw, &mut candidates);
                }
            }
        }
        // pairwise_sub surfaces its readable runs directly; cut runs end with
        // the `… (truncated)` marker and are skipped as partial values
        "pairwise_sub" => {
            if let Some(hidden) = raw.get("hidden").and_then(|v| v.as_array()) {
                for run in hidden {
                    if candidates.len() >= MAX_PER_RESULT {
                        break;
                    }
                    if let Some(text) = run.get("text").and_then(|v| v.as_str()) {
                        if !text.ends_with("(truncated)") {
                            candidates.push(text.to_owned());
                        }
                    }
                }
            }
        }
        // resolved API-hash names are dictionary words, kept behind the caps
        "api_hash_match" => {
            if let Some(matched) = raw.get("matched").and_then(|v| v.as_array()) {
                for entry in matched {
                    if candidates.len() >= MAX_PER_RESULT {
                        break;
                    }
                    if let Some(name) = entry.get("name").and_then(|v| v.as_str()) {
                        candidates.push(name.to_owned());
                    }
                }
            }
        }
        // strings_range only counts when it scanned a decoded artifact (a
        // non-empty `target` arg); raw-sample strings are not decoded content
        "strings_range" => {
            let target = args.get("target").and_then(|v| v.as_str()).unwrap_or("");
            if !target.is_empty() {
                if let Some(strings) = raw.get("strings").and_then(|v| v.as_array()) {
                    for entry in strings {
                        if candidates.len() >= MAX_PER_RESULT {
                            break;
                        }
                        if let Some(text) = entry.get("text").and_then(|v| v.as_str()) {
                            candidates.push(text.to_owned());
                        }
                    }
                }
            }
        }
        // a script's final value can nest recovered strings arbitrarily deep,
        // so walk it; the printed `output` lines are deliberately skipped
        "run_script" => {
            if let Some(value) = raw.get("value") {
                collect_json_strings(value, SCRIPT_WALK_DEPTH, &mut candidates);
            }
        }
        // an emulation run reports the strings it recovered directly; each is
        // already sanitized and length-capped by the bridge
        "emulate_function" => {
            if let Some(strings) = raw.get("strings").and_then(|v| v.as_array()) {
                for entry in strings.iter().take(MAX_PER_RESULT) {
                    if let Some(text) = entry.get("text").and_then(|v| v.as_str()) {
                        candidates.push(text.to_owned());
                    }
                }
            }
        }
        _ => {}
    }
    // normalize, then dedup while preserving first-seen order
    let mut seen = HashSet::new();
    candidates
        .iter()
        .filter_map(|raw| normalize(raw))
        .filter(|value| seen.insert(value.clone()))
        .collect()
}

/// Harvests readable line segments from a qualifying `DecodeResult`
///
/// The decode's `text` field is a bounded preview with control bytes dotted
/// out, so the harvest is line-oriented: each line is trimmed of whitespace
/// and of the `.` padding that dotted-out bytes leave at the edges.
///
/// # Arguments
///
/// * `raw` - The `DecodeResult`-shaped result JSON
/// * `candidates` - The candidate list to append onto
fn harvest_decode_text(raw: &serde_json::Value, candidates: &mut Vec<String>) {
    // only a mostly-printable decode is trusted as recovered text
    let ratio = raw
        .get("printable_ratio")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    if ratio < MIN_PRINTABLE_RATIO {
        return;
    }
    let Some(text) = raw.get("text").and_then(|v| v.as_str()) else {
        return;
    };
    // split on the line breaks the preview preserves and trim the dot padding
    for line in text.lines() {
        if candidates.len() >= MAX_PER_RESULT {
            break;
        }
        let segment = line.trim().trim_matches('.');
        if !segment.is_empty() {
            candidates.push(segment.to_owned());
        }
    }
}

/// Appends the string items of a JSON array onto a candidate list
///
/// # Arguments
///
/// * `values` - The JSON array to read strings out of
/// * `candidates` - The candidate list to append onto
/// * `cap` - The candidate-list size to stop at
fn collect_strs(values: &[serde_json::Value], candidates: &mut Vec<String>, cap: usize) {
    // copy out each string item until the cap is reached
    for value in values {
        if candidates.len() >= cap {
            break;
        }
        if let Some(text) = value.as_str() {
            candidates.push(text.to_owned());
        }
    }
}

/// Recursively collects the strings nested in a JSON value
///
/// # Arguments
///
/// * `value` - The JSON value to walk
/// * `depth` - How many more levels of nesting to descend
/// * `candidates` - The candidate list to append onto
fn collect_json_strings(value: &serde_json::Value, depth: usize, candidates: &mut Vec<String>) {
    // stop at the collection cap so a huge script value stays bounded
    if candidates.len() >= MAX_SCRIPT_STRINGS {
        return;
    }
    match value {
        // strings are the candidates themselves
        serde_json::Value::String(text) => candidates.push(text.clone()),
        // containers recurse while depth remains
        serde_json::Value::Array(items) if depth > 0 => {
            for item in items {
                collect_json_strings(item, depth - 1, candidates);
            }
        }
        serde_json::Value::Object(map) if depth > 0 => {
            for item in map.values() {
                collect_json_strings(item, depth - 1, candidates);
            }
        }
        // numbers, bools, nulls, and too-deep containers carry no constants
        _ => {}
    }
}

/// Scans one successful tool result and appends the inline nudge if due
///
/// This is the factored body of the agent hook's `on_tool_result` arm, kept a
/// free function over publicly-constructible types so it can be unit tested
/// (rig's `HookContext` cannot be built outside rig). A rewrite appends one
/// trailing text block to the existing presentation, so structured content
/// passes through untouched; any construction failure degrades to `Keep`.
///
/// # Arguments
///
/// * `tool_name` - The tool that produced the result
/// * `args` - The call's effective JSON argument text
/// * `raw_result` - The tool's immutable raw execution result
/// * `presentation` - The current model-visible presentation
/// * `tracker` - The session's shared constant tracker
/// * `forum` - The session's shared forum
pub async fn apply_inline_nudge(
    tool_name: &str,
    args: &str,
    raw_result: &ToolResult,
    presentation: &ToolOutput,
    tracker: Arc<ConstantTracker>,
    forum: Arc<ForumState>,
) -> ToolResultAction {
    // only a clean success carries trustworthy recovered content
    if !raw_result.is_success() {
        return ToolResultAction::Keep;
    }
    // decode tools return structs, which rig presents as one JSON block
    let Some(raw) = raw_result.output().as_json() else {
        return ToolResultAction::Keep;
    };
    // the args gate (strings_range's `target`) reads the effective call args
    let args: serde_json::Value = serde_json::from_str(args).unwrap_or(serde_json::Value::Null);
    // harvest the constants this result recovered, if any
    let constants = extract_constants(tool_name, &args, raw);
    if constants.is_empty() {
        return ToolResultAction::Keep;
    }
    // record the sightings and ask for a nudge, honoring every cap
    let Some(nudge) = tracker.inline_nudge(&forum, constants).await else {
        return ToolResultAction::Keep;
    };
    // append the advisory as one trailing text block on the presentation
    let mut blocks = presentation.as_content().to_vec();
    blocks.push(ToolResultContent::text(nudge));
    match ToolOutput::content(blocks) {
        Ok(output) => ToolResultAction::Rewrite(output),
        // construction cannot fail on a non-empty list; keep the result if it does
        Err(_) => ToolResultAction::Keep,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thorpal::tools::decode::{
        BruteCandidate, BruteResult, DecodeResult, HashMatchResult, HiddenRun, MatchedHash,
        PairwiseResult,
    };
    use crate::thorpal::tools::file::{FileString, FileStrings};
    use crate::thorpal::tools::forum::{ForumState, Kind};
    use crate::thorpal::tools::scripts::ScriptRun;

    /// Builds a qualifying DecodeResult carrying the given preview text.
    fn decode_result(text: &str, printable_ratio: f32) -> DecodeResult {
        DecodeResult {
            steps: vec!["xor key=0x2a".to_owned()],
            input_len: 64,
            output_len: 64,
            printable_ratio,
            text: text.to_owned(),
            hexdump: String::new(),
            saved: None,
            truncated: None,
        }
    }

    /// Serializes a result struct the way rig presents it to the hook.
    fn to_json<T: serde::Serialize>(result: &T) -> serde_json::Value {
        serde_json::to_value(result).expect("result structs serialize")
    }

    /// Normalization trims, gates length, keeps case, and rejects control bytes.
    #[test]
    fn normalize_gates_and_preserves_case() {
        // surrounding whitespace is trimmed off a kept constant
        assert_eq!(
            normalize("  act=recive_message  "),
            Some("act=recive_message".to_owned())
        );
        // case survives verbatim: C2 verbs must match the forum exactly
        assert_eq!(normalize("ACT=Life99"), Some("ACT=Life99".to_owned()));
        // too short and too long are both rejected
        assert_eq!(normalize("abcde"), None);
        assert_eq!(normalize(&"x".repeat(MAX_CONSTANT_LEN + 1)), None);
        // control characters mean byte noise, not a constant
        assert_eq!(normalize("abc\u{1}def"), None);
    }

    /// A keyed decode's preview lines are harvested only when mostly printable.
    #[test]
    fn extract_reads_decode_result_lines() {
        // the drift guard: build the real struct and serialize it
        let result = decode_result("act=recive_message\n..act=get_message..\nxy", 0.95);
        let raw = to_json(&result);
        let constants = extract_constants("decode_bytes", &serde_json::Value::Null, &raw);
        // both qualifying lines survive, dot padding trimmed, short runs dropped
        assert_eq!(
            constants,
            vec![
                "act=recive_message".to_owned(),
                "act=get_message".to_owned()
            ]
        );
        // a mostly-binary decode is not trusted as recovered text
        let noisy = decode_result("act=recive_message", 0.5);
        let raw = to_json(&noisy);
        assert!(extract_constants("decode_bytes", &serde_json::Value::Null, &raw).is_empty());
    }

    /// A brute sweep contributes only its best candidate's readable runs.
    #[test]
    fn extract_reads_brute_top_candidate() {
        let result = BruteResult {
            op: "xor".to_owned(),
            tried: 256,
            candidates: vec![
                BruteCandidate {
                    key: 0x2a,
                    score: 40,
                    text_score: 30,
                    runs: vec!["https://evil.example/gate".to_owned()],
                    preview: String::new(),
                },
                BruteCandidate {
                    key: 0x2b,
                    score: 4,
                    text_score: 2,
                    runs: vec!["loser-run-ignored".to_owned()],
                    preview: String::new(),
                },
            ],
            truncated: None,
        };
        let raw = to_json(&result);
        let constants = extract_constants("byte_transform", &serde_json::Value::Null, &raw);
        // only the strongest candidate's runs qualify
        assert_eq!(constants, vec!["https://evil.example/gate".to_owned()]);
    }

    /// Emulation results surface their recovered strings verbatim.
    #[test]
    fn extract_reads_emulated_strings() {
        // construct the real result struct so wire drift fails this test
        let result = ghidra_cli::models::EmulateResult {
            stop: ghidra_cli::models::EmulateStop::ReturnReached,
            steps: 131,
            strings: vec![ghidra_cli::models::EmulatedString {
                address: "0x7f0000".to_owned(),
                text: "act=recive_message".to_owned(),
                encoding: "ascii".to_owned(),
            }],
            ..Default::default()
        };
        let constants = extract_constants(
            "emulate_function",
            &serde_json::Value::Null,
            &to_json(&result),
        );
        assert_eq!(constants, ["act=recive_message"]);
    }

    /// Pairwise runs are harvested, skipping truncated (partial) runs.
    #[test]
    fn extract_reads_pairwise_hidden_runs() {
        let result = PairwiseResult {
            decode: decode_result("ignored-here", 0.1),
            hidden: vec![
                HiddenRun {
                    offset: "0x10".to_owned(),
                    text: "stack-built-verb".to_owned(),
                },
                HiddenRun {
                    offset: "0x40".to_owned(),
                    text: "partial-value… (truncated)".to_owned(),
                },
            ],
        };
        let raw = to_json(&result);
        let constants = extract_constants("pairwise_sub", &serde_json::Value::Null, &raw);
        // the full run is kept; the cut run is a partial value and skipped
        assert_eq!(constants, vec!["stack-built-verb".to_owned()]);
    }

    /// Resolved API-hash names are harvested from the matched list.
    #[test]
    fn extract_reads_api_hash_names() {
        let result = HashMatchResult {
            matched: vec![MatchedHash {
                hash: "0xdeadbeef".to_owned(),
                name: "VirtualAllocEx".to_owned(),
                family: "crc32",
                variant: "VirtualAllocEx".to_owned(),
            }],
            unmatched: vec!["0x1".to_owned()],
            names_tried: 1000,
            note: None,
        };
        let raw = to_json(&result);
        let constants = extract_constants("api_hash_match", &serde_json::Value::Null, &raw);
        assert_eq!(constants, vec!["VirtualAllocEx".to_owned()]);
    }

    /// strings_range counts only when it scanned a decoded artifact.
    #[test]
    fn extract_gates_strings_range_on_target() {
        let result = FileStrings {
            strings: vec![FileString {
                offset: "0x0".to_owned(),
                encoding: "ascii",
                text: "c2.staging.example".to_owned(),
            }],
            scanned_from: "0x0".to_owned(),
            scanned_to: "0x100".to_owned(),
            file_size: 256,
            truncated: None,
        };
        let raw = to_json(&result);
        // with a target the scan read a decoded artifact, so strings qualify
        let args = serde_json::json!({"target": "decoded.bin"});
        assert_eq!(
            extract_constants("strings_range", &args, &raw),
            vec!["c2.staging.example".to_owned()]
        );
        // without a target the scan read the raw sample: nothing qualifies
        let args = serde_json::json!({"offset": 0});
        assert!(extract_constants("strings_range", &args, &raw).is_empty());
    }

    /// A script's final value is walked recursively, bounded by depth.
    #[test]
    fn extract_walks_script_value() {
        let result = ScriptRun {
            origin: "session",
            value: serde_json::json!({
                "verbs": ["act=recive_message", {"deep": {"deeper": {"deepest": "below-depth-cap"}}}],
                "count": 7,
            }),
            output: vec!["printed-line-is-skipped".to_owned()],
            truncated: None,
        };
        let raw = to_json(&result);
        let constants = extract_constants("run_script", &serde_json::Value::Null, &raw);
        // the nested string within depth qualifies; print output never does
        assert_eq!(constants, vec!["act=recive_message".to_owned()]);
    }

    /// Unknown tools are never scanned.
    #[test]
    fn extract_ignores_unknown_tools() {
        let raw = serde_json::json!({"text": "act=recive_message", "printable_ratio": 1.0});
        assert!(extract_constants("list_strings", &serde_json::Value::Null, &raw).is_empty());
    }

    /// The inline nudge names each constant at most once per session.
    #[tokio::test]
    async fn inline_nudge_is_once_per_constant() {
        let tracker = ConstantTracker::new();
        let forum = ForumState::new();
        // the first sighting of an unposted constant draws the nudge
        let first = tracker
            .inline_nudge(&forum, vec!["act=recive_message".to_owned()])
            .await;
        assert!(first.is_some_and(|text| text.contains("`act=recive_message`")));
        // the same constant never draws a second nudge
        let second = tracker
            .inline_nudge(&forum, vec!["act=recive_message".to_owned()])
            .await;
        assert!(second.is_none());
    }

    /// Constants the forum already carries are never nudged.
    #[tokio::test]
    async fn inline_nudge_skips_forum_present_constants() {
        let tracker = ConstantTracker::new();
        let forum = ForumState::new();
        // post the constant first, the way an agent finding would land
        forum
            .seed_post(
                "tester".to_owned(),
                "c2-verbs".to_owned(),
                "C2 verbs".to_owned(),
                "the beacon sends act=recive_message".to_owned(),
                Kind::Statement,
            )
            .await
            .expect("seeding the post should succeed");
        // a posted constant draws no nudge at all
        let nudge = tracker
            .inline_nudge(&forum, vec!["act=recive_message".to_owned()])
            .await;
        assert!(nudge.is_none());
    }

    /// The session-wide nudge budget stops the inline advisories.
    #[tokio::test]
    async fn inline_nudge_respects_session_cap() {
        let tracker = ConstantTracker::new();
        let forum = ForumState::new();
        // burn the whole budget on distinct constants
        for n in 0..MAX_CONSTANT_NUDGES {
            let nudge = tracker
                .inline_nudge(&forum, vec![format!("unique-constant-{n}")])
                .await;
            assert!(nudge.is_some());
        }
        // the next distinct constant is still tracked but never nudged
        let over = tracker
            .inline_nudge(&forum, vec!["constant-over-cap".to_owned()])
            .await;
        assert!(over.is_none());
        // ...and it still surfaces through the finish-time unposted list
        let unposted = tracker.unposted(&forum, MAX_FINISH_CONSTANTS).await;
        assert!(unposted.contains(&"constant-over-cap".to_owned()));
    }

    /// The finish-time list orders by sightings and honors its limit.
    #[tokio::test]
    async fn unposted_orders_and_limits() {
        let tracker = ConstantTracker::new();
        let forum = ForumState::new();
        // one constant seen twice, two seen once
        for batch in [
            vec!["seen-twice-verb".to_owned(), "seen-once-aaa".to_owned()],
            vec!["seen-twice-verb".to_owned(), "seen-once-bbb".to_owned()],
        ] {
            let _ = tracker.inline_nudge(&forum, batch).await;
        }
        // most-seen first, then alphabetical; the limit clips the tail
        let unposted = tracker.unposted(&forum, 2).await;
        assert_eq!(
            unposted,
            vec!["seen-twice-verb".to_owned(), "seen-once-aaa".to_owned()]
        );
    }

    /// The finish-time constant nudge fires exactly once.
    #[tokio::test]
    async fn finish_nudge_is_one_shot() {
        let tracker = ConstantTracker::new();
        // only the first take sees the nudge
        assert!(tracker.take_finish_nudge());
        assert!(!tracker.take_finish_nudge());
    }

    /// The hook body rewrites a qualifying result and preserves its blocks.
    #[tokio::test]
    async fn apply_inline_nudge_appends_one_text_block() {
        let tracker = Arc::new(ConstantTracker::new());
        let forum = Arc::new(ForumState::new());
        // the structured result rig would present for a keyed decode
        let raw = to_json(&decode_result("act=recive_message", 1.0));
        let result = ToolResult::success(ToolOutput::json(raw.clone()));
        let presentation = ToolOutput::json(raw);
        // the first scan appends exactly one trailing text block
        let action = apply_inline_nudge(
            "decode_bytes",
            "{}",
            &result,
            &presentation,
            Arc::clone(&tracker),
            Arc::clone(&forum),
        )
        .await;
        let ToolResultAction::Rewrite(output) = action else {
            panic!("a fresh unposted constant must rewrite the presentation");
        };
        let blocks = output.as_content();
        assert_eq!(blocks.len(), 2);
        // the original structured block passes through untouched
        assert!(blocks[0].as_json().is_some());
        // the appended block is the advisory text naming the constant
        let advisory = blocks[1].as_text().expect("the nudge is a text block");
        assert!(advisory.contains("`act=recive_message`"));
        // the same constant draws no second rewrite
        let again =
            apply_inline_nudge("decode_bytes", "{}", &result, &presentation, tracker, forum).await;
        assert_eq!(again, ToolResultAction::Keep);
    }

    /// Results from unscanned tools are kept untouched.
    #[tokio::test]
    async fn apply_inline_nudge_keeps_unknown_tools() {
        let tracker = Arc::new(ConstantTracker::new());
        let forum = Arc::new(ForumState::new());
        let raw = serde_json::json!({"text": "act=recive_message", "printable_ratio": 1.0});
        let result = ToolResult::success(ToolOutput::json(raw.clone()));
        let presentation = ToolOutput::json(raw);
        // an unscanned tool's result is always kept as-is
        let action =
            apply_inline_nudge("list_strings", "{}", &result, &presentation, tracker, forum).await;
        assert_eq!(action, ToolResultAction::Keep);
    }
}
