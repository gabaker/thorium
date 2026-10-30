//! The read-only session scanner feeding the usage TUI
//!
//! A background task enumerates the sessions root, folds each session's
//! `meta.json` + `session.json` + `transcript.jsonl` into a [`SessionSummary`],
//! and keeps the fold incremental: transcripts are append-only, so growth is
//! re-read from a remembered byte offset, and the atomically-replaced JSON
//! files reload only when their stamps move.
//!
//! Strictly read-only, and tolerant of everything a live writer can produce:
//! [`thorpal::SessionStore::open`] is never called (it creates directories),
//! `*.tmp` siblings and `*.corrupt-*` set-asides are never read, a torn final
//! transcript line waits for the next pass, a session renamed mid-pass is
//! skipped until the next pass sees its new name, and a transcript that shrank
//! (a replaced store) resets its fold and re-reads from the top.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use kanal::AsyncSender;
use thorium::CtlConf;
use thorium::client::conf::AiPricing;
use thorpal::{
    AiProviderConfig, SessionMeta, SessionSnapshot, TranscriptItem, TranscriptLine, UsageTotals,
};

use crate::handlers::ai::chat::{ActivityIntervals, estimate_tokens, now_ms, usage_remainder};
use crate::handlers::ai::pricing::CostModel;
use crate::handlers::ai::session::to_thorpal_provider;
use crate::handlers::ai::telemetry::fold::TelemetryFold;

use super::model::{
    AgentWindow, HourBucket, HourKey, SessionSummary, cost_of_tokens, hour_key, role_of,
};

/// How recently a session file must have changed to call the session live
const LIVE_WINDOW: Duration = Duration::from_secs(120);

/// How long the scanner sleeps between incremental passes
const PASS_INTERVAL: Duration = Duration::from_secs(2);

/// How far into the future a transcript stamp may run before it is ignored
///
/// Protects every window, interval, and bucket computation from one wild
/// stamp (a corrupt line, a badly skewed writer clock) that would otherwise
/// poison the math; a day comfortably covers real clock skew.
const FUTURE_GRACE_MS: u64 = 86_400_000;

/// The best-effort pricing context resolved once from the thorctl config
///
/// Carries the config's optional per-model pricing overrides plus the model
/// the config is written for: overrides apply to a session only when it ran
/// that exact model, since applying them to a different model would misprice
/// it.
#[derive(Debug, Clone, Default)]
pub struct PricingCtx {
    /// The config's pricing overrides for its configured model, if any
    pub overrides: Option<AiPricing>,
    /// The model id the active config block talks to, if one resolved
    pub configured_model: Option<String>,
}

impl PricingCtx {
    /// Resolves the pricing context from an optional thorctl config
    ///
    /// Best-effort by design, mirroring `batch status`: a missing or
    /// unresolvable config just thins the cost columns.
    ///
    /// # Arguments
    ///
    /// * `conf` - The loaded thorctl config, when one loaded at all
    pub fn from_conf(conf: Option<CtlConf>) -> Self {
        // resolve the active provider block, tolerating every failure
        let resolved = conf.and_then(|conf| conf.ai.as_ref().and_then(|ai| ai.resolve().ok()));
        match resolved {
            Some(resolved) => {
                // pull the overrides before the provider conversion consumes it
                let overrides = resolved.pricing();
                // the configured model gates which sessions the overrides touch
                let configured_model = Some(to_thorpal_provider(resolved).model().to_owned());
                PricingCtx {
                    overrides,
                    configured_model,
                }
            }
            None => PricingCtx::default(),
        }
    }
}

/// Resolves one session's cost model from its stored provider
///
/// # Arguments
///
/// * `provider` - The session snapshot's sanitized provider, when it parsed
/// * `pricing` - The config's best-effort pricing context
pub fn session_cost_model(
    provider: Option<&AiProviderConfig>,
    pricing: &PricingCtx,
) -> Option<CostModel> {
    // without a typed provider the cache semantics are unknown; stay unpriced
    let provider = provider?;
    // config pricing overrides are written for the configured model; apply
    // them only when this session actually ran that model
    let overrides = match (&pricing.configured_model, &pricing.overrides) {
        (Some(configured), Some(overrides))
            if configured.eq_ignore_ascii_case(provider.model()) =>
        {
            Some(overrides)
        }
        _ => None,
    };
    CostModel::for_provider(provider, overrides)
}

/// The lean, loss-tolerant pieces of a session snapshot the scanner needs
#[derive(Debug, Clone, Default)]
pub struct LeanSnapshot {
    /// The sanitized provider config, when the full snapshot parsed
    pub provider: Option<AiProviderConfig>,
    /// The model id, scraped even when the full snapshot did not parse
    pub model: Option<String>,
    /// The accumulated per-agent usage ledger
    pub usage: BTreeMap<String, UsageTotals>,
}

/// Reads a session snapshot tolerantly, degrading instead of failing
///
/// The typed loader gives version gating and a priceable provider; when it
/// rejects the file (a future schema version, a corrupt write) the fallback
/// scrapes just the usage map and the provider's model string out of whatever
/// JSON is there, so the session still shows tokens -- just unpriced.
///
/// # Arguments
///
/// * `path` - The `session.json` path to read
pub async fn read_lean_snapshot(path: &Path) -> Option<LeanSnapshot> {
    // the happy path: the full typed, version-gated snapshot
    match SessionSnapshot::load(path).await {
        Ok(snapshot) => {
            // keep the model string handy before the provider moves
            let model = Some(snapshot.provider.model().to_owned());
            Some(LeanSnapshot {
                provider: Some(snapshot.provider),
                model,
                usage: snapshot.usage,
            })
        }
        Err(_) => {
            // scrape what we can out of the raw JSON instead
            let bytes = tokio::fs::read(path).await.ok()?;
            let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
            // the usage map deserializes field-tolerantly via serde defaults
            let usage = value
                .get("usage")
                .cloned()
                .and_then(|usage| serde_json::from_value(usage).ok())
                .unwrap_or_default();
            // the model string rides inside the tagged provider block
            let model = value
                .get("provider")
                .and_then(|provider| provider.get("model"))
                .and_then(|model| model.as_str())
                .map(str::to_owned);
            Some(LeanSnapshot {
                provider: None,
                model,
                usage,
            })
        }
    }
}

/// Reads a transcript's complete lines from a byte offset
///
/// Mirrors the store's lenient loader: unparseable lines are skipped, and a
/// torn final line (no trailing newline yet) is left unconsumed so the next
/// pass re-reads it once its writer finishes the append.
///
/// # Arguments
///
/// * `path` - The `transcript.jsonl` path to read
/// * `offset` - The first byte this fold has not consumed yet
#[tracing::instrument(name = "usage::read_transcript_from", skip_all, err(Debug))]
pub async fn read_transcript_from(
    path: &Path,
    offset: u64,
) -> std::io::Result<(Vec<TranscriptLine>, u64)> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    // a missing transcript is just a session with no replayable lines yet
    let mut file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), offset)),
        Err(err) => return Err(err),
    };
    // seek to the first unconsumed byte and read whatever follows
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).await?;
    // consume only whole lines; a torn tail stays for the next pass
    let consumed = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |pos| pos + 1);
    // parse each whole line, skipping anything unparseable like the store does
    let mut lines = Vec::new();
    for line in bytes[..consumed].split(|byte| *byte == b'\n') {
        // ignore blank lines outright
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<TranscriptLine>(line) {
            Ok(parsed) => lines.push(parsed),
            Err(err) => tracing::debug!(?err, "skipping an unparseable transcript line"),
        }
    }
    Ok((lines, offset + consumed as u64))
}

/// A file's change stamp: its length plus its modification time
type FileStamp = Option<(u64, SystemTime)>;

/// Stats a file into its change stamp, treating any failure as absent
///
/// # Arguments
///
/// * `path` - The file to stat
async fn stamp(path: &Path) -> FileStamp {
    // a missing file (or any stat failure) simply has no stamp
    let meta = tokio::fs::metadata(path).await.ok()?;
    Some((
        meta.len(),
        meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
    ))
}

/// Whether a stamp's modification time falls inside the live window
///
/// # Arguments
///
/// * `stamp` - The file stamp to test
fn stamp_is_live(stamp: &FileStamp) -> bool {
    stamp.is_some_and(|(_, mtime)| match SystemTime::now().duration_since(mtime) {
        // a recent mtime is live activity
        Ok(age) => age < LIVE_WINDOW,
        // a slightly-future mtime is clock skew and still reads live, but a
        // far-future one is corrupt and must not pin the session live forever
        Err(skew) => skew.duration() < LIVE_WINDOW,
    })
}

/// One session's incremental fold state across scanner passes
#[derive(Debug, Default)]
pub struct SessionFold {
    /// The session's name (its directory name under the root)
    name: String,
    /// The parsed metadata, when `meta.json` was readable
    meta: Option<SessionMeta>,
    /// The lean snapshot pieces, when `session.json` was readable
    lean: Option<LeanSnapshot>,
    /// The last-seen stamp of `meta.json`
    meta_stamp: FileStamp,
    /// The last-seen stamp of `session.json`
    snap_stamp: FileStamp,
    /// The last-seen stamp of `transcript.jsonl`
    script_stamp: FileStamp,
    /// The first transcript byte not consumed yet
    offset: u64,
    /// Whether this fold has produced a summary at least once
    summarized: bool,
    /// Whether the last summary reported the session live
    was_live: bool,
    /// In-flight tool calls' start stamps, keyed by call id
    pending: HashMap<String, u64>,
    /// The merged busy intervals the transcript's stamps describe
    activity: ActivityIntervals,
    /// The first stamped transcript event
    first_ts: Option<u64>,
    /// The last stamped transcript event
    last_ts: Option<u64>,
    /// Each agent's window, role, and tool-call count so far
    windows: BTreeMap<String, AgentWindow>,
    /// The per-agent sums of replayed per-turn usage lines
    replayed: BTreeMap<String, UsageTotals>,
    /// Usage-line tokens whose checkpoints carried no stamp, per agent
    unstamped: BTreeMap<String, UsageTotals>,
    /// Exact token sums bucketed at each checkpoint's local hour
    exact: BTreeMap<HourKey, HourBucket>,
    /// Pre-checkpoint estimate weights per agent per hour
    pre_weights: BTreeMap<String, BTreeMap<HourKey, u64>>,
    /// Each agent's total pre-checkpoint estimate weight
    pre_totals: BTreeMap<String, u64>,
    /// Calls per tool name
    tools: BTreeMap<String, u64>,
    /// Total tool calls
    tool_calls: u64,
    /// Model turns recorded by usage lines
    turns: u64,
    /// The telemetry-only accumulator, allocated when this scan feeds the
    /// telemetry exporter (the usage TUI's folds carry none and pay nothing)
    telemetry: Option<Box<TelemetryFold>>,
}

impl SessionFold {
    /// Creates an empty fold for a session
    ///
    /// # Arguments
    ///
    /// * `name` - The session's directory name
    pub fn new(name: String) -> Self {
        SessionFold {
            name,
            ..SessionFold::default()
        }
    }

    /// Opts this fold into gathering the telemetry exporter's counters
    ///
    /// Usage scans never call this, so their folds skip the telemetry work
    /// entirely; the exporter's folds collect failure signatures in memory
    /// whatever the chosen level, so the tier choice never forces a re-scan.
    pub fn with_telemetry(mut self) -> Self {
        // allocate the accumulator the fold pass feeds
        self.telemetry = Some(Box::default());
        self
    }

    /// Drops every transcript-derived accumulator for a full re-read
    ///
    /// Runs when a transcript shrank or was replaced wholesale -- the
    /// append-only invariant broke, so the offset-based fold is stale.
    fn reset_transcript(&mut self) {
        // zero the offset and every accumulator the transcript feeds
        self.offset = 0;
        self.pending = HashMap::new();
        self.activity = ActivityIntervals::default();
        self.first_ts = None;
        self.last_ts = None;
        self.windows = BTreeMap::new();
        self.replayed = BTreeMap::new();
        self.unstamped = BTreeMap::new();
        self.exact = BTreeMap::new();
        self.pre_weights = BTreeMap::new();
        self.pre_totals = BTreeMap::new();
        self.tools = BTreeMap::new();
        self.tool_calls = 0;
        self.turns = 0;
        // a telemetry-gathering fold restarts with a fresh accumulator
        if self.telemetry.is_some() {
            self.telemetry = Some(Box::default());
        }
    }

    /// Gets or creates an agent's window and widens it to a stamp
    ///
    /// # Arguments
    ///
    /// * `agent` - The agent display name to touch
    /// * `ts` - The stamp widening the agent's window, when the line had one
    fn touch(&mut self, agent: &str, ts: Option<u64>) -> &mut AgentWindow {
        // any event upserts its agent, so ledger-only names still get rows
        let window = self
            .windows
            .entry(agent.to_owned())
            .or_insert_with(|| AgentWindow {
                role: role_of(agent),
                ..AgentWindow::default()
            });
        // widen the agent's first/last window around the stamp
        if let Some(ts) = ts {
            window.first_ts = Some(window.first_ts.map_or(ts, |first| first.min(ts)));
            window.last_ts = Some(window.last_ts.map_or(ts, |last| last.max(ts)));
        }
        window
    }

    /// Records an estimate weight for distributing an agent's remainder
    ///
    /// Only pre-checkpoint weights count: once an agent has replayed usage
    /// lines, later events belong to turns whose own checkpoints bucket them
    /// exactly.
    ///
    /// # Arguments
    ///
    /// * `agent` - The agent the payload belongs to
    /// * `ts` - The payload's capture stamp
    /// * `est` - The payload's estimated tokens
    fn weigh(&mut self, agent: &str, ts: Option<u64>, est: u64) {
        // nothing to weigh, or the agent already has exact checkpoints
        if est == 0 || self.replayed.contains_key(agent) {
            return;
        }
        // an unstamped payload cannot land in any hour
        let Some(key) = ts.and_then(hour_key) else {
            return;
        };
        // fold the weight into the agent's per-hour and total tallies
        *self
            .pre_weights
            .entry(agent.to_owned())
            .or_default()
            .entry(key)
            .or_default() += est;
        *self.pre_totals.entry(agent.to_owned()).or_default() += est;
    }

    /// Folds one transcript line into the running accumulators
    ///
    /// # Arguments
    ///
    /// * `line` - The transcript line to fold
    fn fold_line(&mut self, line: &TranscriptLine) {
        // treat a wildly future stamp as unstamped so one corrupt line cannot
        // poison the session window, the activity map, or the hour buckets;
        // the line's tokens still count, just unbucketed
        let ts = line
            .ts
            .filter(|ts| now_ms().is_none_or(|now| *ts <= now.saturating_add(FUTURE_GRACE_MS)));
        // every stamped line bounds the session window and marks activity
        if let Some(ts) = ts {
            self.first_ts = Some(self.first_ts.map_or(ts, |first| first.min(ts)));
            self.last_ts = Some(self.last_ts.map_or(ts, |last| last.max(ts)));
            self.activity.record(ts, ts);
        }
        // a telemetry-gathering fold tallies its own counters off the same
        // line, so the exporter rides this one streaming pass
        if let Some(telemetry) = self.telemetry.as_mut() {
            telemetry.observe(&line.item, ts);
        }
        match &line.item {
            // a tool call: count it, weigh its args, and open its interval
            TranscriptItem::ToolCall {
                agent,
                call_id,
                tool,
                args,
            } => {
                self.touch(agent, ts).tool_calls += 1;
                *self.tools.entry(tool.clone()).or_default() += 1;
                self.tool_calls += 1;
                self.weigh(agent, ts, estimate_tokens(args));
                if let Some(ts) = ts {
                    self.pending.insert(call_id.clone(), ts);
                }
            }
            // a tool result: weigh its payload and close the call's interval
            TranscriptItem::ToolResult {
                agent,
                call_id,
                result,
                ..
            } => {
                self.touch(agent, ts);
                self.weigh(agent, ts, estimate_tokens(result));
                if let (Some(start), Some(end)) = (self.pending.remove(call_id), ts) {
                    self.activity.record(start, end);
                }
            }
            // a durable usage checkpoint: exact buckets, turns, remainder math
            TranscriptItem::Usage {
                agent,
                input,
                output,
                cached,
                cache_creation,
                total,
            } => {
                self.touch(agent, ts);
                // accumulate the replayed sums the remainder subtracts later
                *self.replayed.entry(agent.clone()).or_default() += UsageTotals {
                    input_tokens: *input,
                    output_tokens: *output,
                    total_tokens: *total,
                    cached_input_tokens: *cached,
                    cache_creation_input_tokens: *cache_creation,
                    tool_use_prompt_tokens: 0,
                    reasoning_tokens: 0,
                };
                self.turns += 1;
                match ts.and_then(hour_key) {
                    // bucket the exact deltas at the checkpoint's hour
                    Some(key) => {
                        let bucket = self.exact.entry(key).or_default();
                        bucket.input += *input as f64;
                        bucket.output += *output as f64;
                        bucket.cached += *cached as f64;
                        bucket.cache_creation += *cache_creation as f64;
                    }
                    // an unstamped checkpoint distributes with the remainder
                    None => {
                        *self.unstamped.entry(agent.clone()).or_default() += UsageTotals {
                            input_tokens: *input,
                            output_tokens: *output,
                            total_tokens: *total,
                            cached_input_tokens: *cached,
                            cache_creation_input_tokens: *cache_creation,
                            tool_use_prompt_tokens: 0,
                            reasoning_tokens: 0,
                        };
                    }
                }
            }
            // assistant text: weigh it under the agent that produced it
            TranscriptItem::SubAgentMessage { agent, text } => {
                self.touch(agent, ts);
                self.weigh(agent, ts, estimate_tokens(text));
            }
            // a conversation's final answer attributes to its root agent
            TranscriptItem::Assistant { text, scope } => {
                let agent = match scope {
                    Some(scope) => format!("btw:{scope}"),
                    None => "main".to_owned(),
                };
                self.touch(&agent, ts);
                self.weigh(&agent, ts, estimate_tokens(text));
            }
            // the user's text enters its conversation root's context
            TranscriptItem::User { text, scope } => {
                let agent = match scope {
                    Some(scope) => format!("btw:{scope}"),
                    None => "main".to_owned(),
                };
                self.touch(&agent, ts);
                self.weigh(&agent, ts, estimate_tokens(text));
            }
            // a sub-agent breadcrumb names its role authoritatively
            TranscriptItem::SubAgentStarted { name, role, .. } => {
                let window = self.touch(name, ts);
                if let Some(role) = role {
                    window.role = role.clone();
                }
            }
            // lifecycle breadcrumbs widen their agent's window
            TranscriptItem::SubAgentExtended { name, .. }
            | TranscriptItem::ContextPressure { name, .. }
            | TranscriptItem::SubAgentTerminated { name, .. } => {
                self.touch(name, ts);
            }
            TranscriptItem::TurnFailed { agent, .. }
            | TranscriptItem::LlmScrubbed { agent, .. }
            | TranscriptItem::AnalystAsk { agent, .. }
            | TranscriptItem::ReviewFlag { agent, .. } => {
                self.touch(agent, ts);
            }
            // provider-stall lines widen their agent's window; the telemetry
            // counters over them arrive with the telemetry fold
            TranscriptItem::LlmRetry { agent, .. }
            | TranscriptItem::LlmRetryResolved { agent, .. }
            | TranscriptItem::ProviderPaused { agent, .. } => {
                self.touch(agent, ts);
            }
            // swarm bounds, report degradations, pause endings, manifest
            // lines, and detector rosters (none carry an agent) only mark
            // session activity; the telemetry fold above consumes the
            // telemetry-bearing ones
            TranscriptItem::SwarmStarted { .. }
            | TranscriptItem::SwarmFinished { .. }
            | TranscriptItem::ReportDegraded { .. }
            | TranscriptItem::SessionResumed { .. }
            | TranscriptItem::ProviderResumed { .. }
            | TranscriptItem::ToolsOffered { .. }
            | TranscriptItem::DetectorsRun { .. } => {}
        }
    }

    /// Re-reads whatever changed on disk and summarizes when anything did
    ///
    /// Returns `Ok(None)` when nothing changed since the last pass, and an
    /// error when the session directory vanished mid-pass (a rename race) or
    /// a transcript read failed -- the caller skips the session this pass,
    /// keeps its state, and retries next pass.
    ///
    /// # Arguments
    ///
    /// * `root` - The sessions root the session lives under
    /// * `pricing` - The config's best-effort pricing context
    #[tracing::instrument(name = "SessionFold::refresh", skip_all, err(Debug))]
    pub async fn refresh(
        &mut self,
        root: &Path,
        pricing: &PricingCtx,
    ) -> std::io::Result<Option<SessionSummary>> {
        let dir = root.join(&self.name);
        // a vanished directory is a rename race; skip this pass entirely
        tokio::fs::metadata(&dir).await?;
        // the first summary must go out even if every file is still absent
        let mut changed = !self.summarized;
        // reload the metadata when its stamp moved
        let meta_path = dir.join("meta.json");
        let meta_stamp = stamp(&meta_path).await;
        if meta_stamp != self.meta_stamp {
            // tolerate a missing or corrupt meta: the session still lists
            self.meta = match tokio::fs::read(&meta_path).await {
                Ok(bytes) => serde_json::from_slice(&bytes).ok(),
                Err(_) => None,
            };
            self.meta_stamp = meta_stamp;
            changed = true;
        }
        // reload the lean snapshot when its stamp moved
        let snap_path = dir.join("session.json");
        let snap_stamp = stamp(&snap_path).await;
        if snap_stamp != self.snap_stamp {
            self.lean = read_lean_snapshot(&snap_path).await;
            self.snap_stamp = snap_stamp;
            changed = true;
        }
        // fold any transcript growth from the remembered offset
        let script_path = dir.join("transcript.jsonl");
        let script_stamp = stamp(&script_path).await;
        if script_stamp != self.script_stamp {
            // a shrunk transcript broke the append-only invariant: refold
            if script_stamp.is_none_or(|(len, _)| len < self.offset) {
                self.reset_transcript();
            }
            let (lines, offset) = read_transcript_from(&script_path, self.offset).await?;
            for line in &lines {
                self.fold_line(line);
            }
            self.offset = offset;
            self.script_stamp = script_stamp;
            changed = true;
        }
        // a session goes (or stops being) live purely by its stamps aging
        let live = stamp_is_live(&self.meta_stamp)
            || stamp_is_live(&self.snap_stamp)
            || stamp_is_live(&self.script_stamp);
        if live != self.was_live {
            self.was_live = live;
            changed = true;
        }
        // only a change produces a fresh summary for the app
        if changed {
            self.summarized = true;
            Ok(Some(self.summarize(pricing, live)))
        } else {
            Ok(None)
        }
    }

    /// Builds the session's summary from the current fold state
    ///
    /// # Arguments
    ///
    /// * `pricing` - The config's best-effort pricing context
    /// * `live` - Whether the session's files changed recently
    pub fn summarize(&self, pricing: &PricingCtx, live: bool) -> SessionSummary {
        // the ledger is the snapshot's accumulated per-agent usage
        let ledger = self
            .lean
            .as_ref()
            .map(|lean| lean.usage.clone())
            .unwrap_or_default();
        // whatever the replayed usage lines did not cover seeds the estimate
        let remainder = usage_remainder(&ledger, &self.replayed);
        // effective per-agent totals: replayed + remainder is the per-field
        // max of the two sources, so crash skew never undercounts
        let mut effective = self.replayed.clone();
        for (agent, rem) in &remainder {
            *effective.entry(agent.clone()).or_default() += *rem;
        }
        // resolve the session's own cost model from its stored provider
        let cost_model = session_cost_model(
            self.lean.as_ref().and_then(|lean| lean.provider.as_ref()),
            pricing,
        );
        // fill each agent's window with its effective totals and cost
        let mut agents = self.windows.clone();
        let mut totals = UsageTotals::default();
        for (agent, eff) in &effective {
            totals += *eff;
            let window = agents.entry(agent.clone()).or_insert_with(|| AgentWindow {
                role: role_of(agent),
                ..AgentWindow::default()
            });
            window.totals = *eff;
            window.cost = cost_model.as_ref().map(|model| {
                model.cost(
                    eff.input_tokens,
                    eff.output_tokens,
                    eff.cached_input_tokens,
                    eff.cache_creation_input_tokens,
                )
            });
        }
        // start the hour buckets from the exact checkpoint sums
        let mut hours = self.exact.clone();
        // the amounts estimation must distribute: the ledger remainder plus
        // any checkpoints that carried no stamp
        let mut spread = remainder.clone();
        for (agent, tokens) in &self.unstamped {
            *spread.entry(agent.clone()).or_default() += *tokens;
        }
        for (agent, amount) in &spread {
            self.distribute(&mut hours, agent, amount);
        }
        // intersect the busy intervals with hour-sized chunks, labeled by
        // each chunk's local hour (UTC-aligned steps keep DST math exact for
        // whole-hour zones; half-hour zones shift labels by one neighbor)
        for (start, end) in self.activity.segments() {
            let mut cursor = *start;
            while cursor < *end {
                // step to the next UTC hour boundary; the saturating math and
                // the max keep this loop total even on an absurd stamp
                let next = (cursor / 3_600_000)
                    .saturating_add(1)
                    .saturating_mul(3_600_000)
                    .max(cursor.saturating_add(1));
                let chunk_end = next.min(*end);
                if let Some(key) = hour_key(cursor) {
                    hours.entry(key).or_default().active_ms += chunk_end - cursor;
                }
                cursor = chunk_end;
            }
        }
        // normalize inclusive-input providers (the OpenAI protocol folds
        // cache reads into reported input) to the uncached remainder, so
        // token sums never double-count cache reads once buckets aggregate
        // across sessions with different providers
        if cost_model.is_some_and(|model| model.input_includes_cached) {
            for bucket in hours.values_mut() {
                bucket.input = (bucket.input - bucket.cached).max(0.0);
            }
        }
        // price every bucket under the session's own model; the buckets were
        // just normalized to uncached input, so pricing must not subtract the
        // cache reads a second time
        if let Some(model) = &cost_model {
            let bucket_model = CostModel {
                input_includes_cached: false,
                ..*model
            };
            for bucket in hours.values_mut() {
                bucket.cost = cost_of_tokens(
                    &bucket_model,
                    bucket.input,
                    bucket.output,
                    bucket.cached,
                    bucket.cache_creation,
                );
            }
        }
        // price the session totals and cache savings the same way
        let cost = cost_model.as_ref().map(|model| {
            model.cost(
                totals.input_tokens,
                totals.output_tokens,
                totals.cached_input_tokens,
                totals.cache_creation_input_tokens,
            )
        });
        let cache_savings = cost_model.as_ref().map(|model| {
            model.cache_savings(
                totals.cached_input_tokens,
                totals.cache_creation_input_tokens,
            )
        });
        SessionSummary {
            name: self.name.clone(),
            created_at: self.meta.as_ref().map(|meta| meta.created_at),
            binary: self.meta.as_ref().map(|meta| {
                // show just the file name; full paths drown the table
                meta.binary
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| meta.binary.display().to_string())
            }),
            model: self.lean.as_ref().and_then(|lean| lean.model.clone()),
            totals,
            cost,
            cache_savings,
            first_ts: self.first_ts,
            last_ts: self.last_ts,
            active_ms: self
                .activity
                .segments()
                .iter()
                .map(|(start, end)| end - start)
                .sum(),
            activity: self.activity.segments().to_vec(),
            agents,
            tools: self.tools.clone(),
            tool_calls: self.tool_calls,
            turns: (self.turns > 0).then_some(self.turns),
            hours,
            // a telemetry-gathering fold hands the exporter its counters too
            telemetry: self
                .telemetry
                .as_ref()
                .map(|fold| Box::new(fold.summarize(self.meta.as_ref()))),
            live,
            // exact only when estimation had nothing left to distribute
            exact_hours: remainder.is_empty() && self.unstamped.is_empty(),
        }
    }

    /// Distributes one agent's unbucketed tokens across its weighted hours
    ///
    /// Spreads proportionally over the agent's pre-checkpoint estimate
    /// weights; with no weights at all (an unstamped old transcript) the
    /// whole amount lands in the agent's -- then the session's -- first hour,
    /// and with no stamps anywhere it stays out of the buckets entirely (the
    /// totals still carry it).
    ///
    /// # Arguments
    ///
    /// * `hours` - The hour buckets being built
    /// * `agent` - The agent whose tokens are being placed
    /// * `amount` - The token totals to distribute
    fn distribute(
        &self,
        hours: &mut BTreeMap<HourKey, HourBucket>,
        agent: &str,
        amount: &UsageTotals,
    ) {
        // find the agent's weights and their total
        let weights = self.pre_weights.get(agent);
        let weight_total = self.pre_totals.get(agent).copied().unwrap_or_default();
        if let (Some(weights), true) = (weights, weight_total > 0) {
            // spread each field proportionally over the weighted hours
            for (key, weight) in weights {
                let frac = *weight as f64 / weight_total as f64;
                let bucket = hours.entry(*key).or_default();
                bucket.input += amount.input_tokens as f64 * frac;
                bucket.output += amount.output_tokens as f64 * frac;
                bucket.cached += amount.cached_input_tokens as f64 * frac;
                bucket.cache_creation += amount.cache_creation_input_tokens as f64 * frac;
            }
            return;
        }
        // no weights: fall back to the agent's, then the session's, first hour
        let fallback = self
            .windows
            .get(agent)
            .and_then(|window| window.first_ts)
            .or(self.first_ts)
            .and_then(hour_key)
            .or_else(|| {
                // a stamp-less session can still bucket at its creation time
                self.meta.as_ref().and_then(|meta| {
                    u64::try_from(meta.created_at.timestamp_millis())
                        .ok()
                        .and_then(hour_key)
                })
            });
        if let Some(key) = fallback {
            let bucket = hours.entry(key).or_default();
            bucket.input += amount.input_tokens as f64;
            bucket.output += amount.output_tokens as f64;
            bucket.cached += amount.cached_input_tokens as f64;
            bucket.cache_creation += amount.cache_creation_input_tokens as f64;
        }
    }
}

/// One update the scanner sends the app
pub enum ScanUpdate {
    /// Initial-pass progress: how many sessions have been folded so far
    Progress {
        /// How many sessions the pass has folded
        done: usize,
        /// How many sessions the pass found in total
        total: usize,
    },
    /// A session's fresh (or first) summary
    Loaded(Box<SessionSummary>),
    /// A session's directory vanished; drop its row
    Removed(String),
    /// A full pass finished; initial progress can clear
    PassDone,
}

/// Runs the read-only scanner loop until the app goes away
///
/// The first pass walks every session sequentially with progress updates;
/// later passes are mtime-gated and only re-read what actually changed. A
/// nudge on the rescan channel skips the between-pass sleep.
///
/// # Arguments
///
/// * `root` - The sessions root to scan
/// * `pricing` - The config's best-effort pricing context
/// * `tx` - The app's event channel updates are sent over
/// * `rescan` - The channel an operator's refresh key nudges
/// * `telemetry` - Whether folds also gather the telemetry exporter's counters
/// * `wrap` - How the caller's event type wraps a scan update
pub async fn run_scanner<E: Send + 'static>(
    root: PathBuf,
    pricing: PricingCtx,
    tx: AsyncSender<E>,
    mut rescan: tokio::sync::mpsc::Receiver<()>,
    telemetry: bool,
    wrap: impl Fn(ScanUpdate) -> E + Send + 'static,
) {
    // the incremental fold state per session, keyed by directory name
    let mut folds: HashMap<String, SessionFold> = HashMap::new();
    let mut first_pass = true;
    loop {
        // list the session dirs; a missing root simply holds no sessions
        let names = match thorpal::SessionStore::list(&root).await {
            Ok(names) => names,
            Err(_) => Vec::new(),
        };
        // drop state (and rows) for sessions whose directories vanished
        let current: HashSet<&String> = names.iter().collect();
        let gone: Vec<String> = folds
            .keys()
            .filter(|name| !current.contains(name))
            .cloned()
            .collect();
        for name in gone {
            folds.remove(&name);
            if tx.send(wrap(ScanUpdate::Removed(name))).await.is_err() {
                return;
            }
        }
        // fold every current session, sending summaries as they change
        let total = names.len();
        for (done, name) in names.iter().enumerate() {
            // surface initial-pass progress so a cold 2 GB store shows life
            if first_pass
                && tx
                    .send(wrap(ScanUpdate::Progress { done, total }))
                    .await
                    .is_err()
            {
                return;
            }
            let fold = folds.entry(name.clone()).or_insert_with(|| {
                // telemetry scans gather the exporter's counters alongside
                let fold = SessionFold::new(name.clone());
                if telemetry {
                    fold.with_telemetry()
                } else {
                    fold
                }
            });
            match fold.refresh(&root, &pricing).await {
                // something changed: ship the fresh summary
                Ok(Some(summary)) => {
                    if tx
                        .send(wrap(ScanUpdate::Loaded(Box::new(summary))))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                // nothing changed since the last pass
                Ok(None) => {}
                // the directory vanished mid-read (a rename race) or a read
                // failed: keep the old state and let the next pass retry
                Err(_) => {}
            }
        }
        first_pass = false;
        // tell the app the pass settled so initial progress can clear
        if tx.send(wrap(ScanUpdate::PassDone)).await.is_err() {
            return;
        }
        // wait out the poll interval, or an operator's explicit rescan nudge
        tokio::select! {
            _ = tokio::time::sleep(PASS_INTERVAL) => {}
            _ = rescan.recv() => {}
        }
    }
}

/// Folds every session under a root once, for headless telemetry exports
///
/// Strictly read-only like the scanner loop, but sequential and one-shot:
/// each session folds fully with the telemetry counters on, and sessions
/// whose directories vanish mid-read (a rename race) are skipped rather than
/// failing the export.
///
/// # Arguments
///
/// * `root` - The sessions root to scan
/// * `pricing` - The config's best-effort pricing context
pub async fn scan_once(root: &Path, pricing: &PricingCtx) -> Vec<SessionSummary> {
    // list the session dirs; a missing root simply holds no sessions
    let names = match thorpal::SessionStore::list(root).await {
        Ok(names) => names,
        Err(_) => Vec::new(),
    };
    // fold each session fully in one pass
    let mut summaries = Vec::with_capacity(names.len());
    for name in names {
        let mut fold = SessionFold::new(name).with_telemetry();
        // a fresh fold always reports changed state on success
        if let Ok(Some(summary)) = fold.refresh(root, pricing).await {
            summaries.push(summary);
        }
    }
    summaries
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::TimeZone;

    /// A one-hour span in milliseconds, for picking stamps in distinct hours
    const HOUR_MS: u64 = 3_600_000;

    /// Builds a stamped usage checkpoint line
    fn usage_line(agent: &str, input: u64, output: u64, ts: u64) -> TranscriptLine {
        TranscriptLine {
            ts: Some(ts),
            item: TranscriptItem::Usage {
                agent: agent.to_owned(),
                input,
                output,
                cached: 0,
                cache_creation: 0,
                total: input + output,
            },
        }
    }

    /// Builds a stamped tool call line with an args payload of a given size
    fn call_line(agent: &str, call_id: &str, args_chars: usize, ts: u64) -> TranscriptLine {
        TranscriptLine {
            ts: Some(ts),
            item: TranscriptItem::ToolCall {
                agent: agent.to_owned(),
                call_id: call_id.to_owned(),
                tool: "decomp_function".to_owned(),
                args: "a".repeat(args_chars),
            },
        }
    }

    /// Builds a stamped tool result line closing a call
    fn result_line(agent: &str, call_id: &str, ts: u64) -> TranscriptLine {
        TranscriptLine {
            ts: Some(ts),
            item: TranscriptItem::ToolResult {
                agent: agent.to_owned(),
                call_id: call_id.to_owned(),
                tool: "decomp_function".to_owned(),
                result: String::new(),
                success: true,
            },
        }
    }

    /// A bedrock provider whose model prices from the builtin table
    fn bedrock_provider() -> AiProviderConfig {
        AiProviderConfig::Bedrock {
            region: None,
            profile: None,
            mantle: false,
            access_key_id: None,
            secret_access_key: None,
            session_token: None,
            model: "claude-opus-5-5".to_owned(),
        }
    }

    /// A ledger with one agent holding the given input/output buckets
    fn ledger(agent: &str, input: u64, output: u64) -> BTreeMap<String, UsageTotals> {
        let mut usage = BTreeMap::new();
        usage.insert(
            agent.to_owned(),
            UsageTotals {
                input_tokens: input,
                output_tokens: output,
                total_tokens: input + output,
                ..Default::default()
            },
        );
        usage
    }

    /// Tail reads consume whole lines only and resume at the saved offset
    #[tokio::test]
    async fn transcript_tail_reads_resume_at_offsets() {
        // write two whole lines plus a torn third with no newline yet
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("transcript.jsonl");
        let first = serde_json::to_string(&usage_line("main", 10, 1, 1_000)).unwrap();
        let second = serde_json::to_string(&usage_line("main", 20, 2, 2_000)).unwrap();
        let third = serde_json::to_string(&usage_line("main", 30, 3, 3_000)).unwrap();
        let torn = &third[..third.len() / 2];
        std::fs::write(&path, format!("{first}\n{second}\n{torn}")).unwrap();
        // the first read consumes both whole lines and stops at the tear
        let (lines, offset) = read_transcript_from(&path, 0).await.expect("a tail read");
        assert_eq!(lines.len(), 2);
        assert_eq!(offset as usize, first.len() + second.len() + 2);
        // the writer finishes the third line; the next read picks it up alone
        std::fs::write(&path, format!("{first}\n{second}\n{third}\n")).unwrap();
        let (lines, next) = read_transcript_from(&path, offset).await.expect("a resume");
        assert_eq!(lines.len(), 1);
        assert_eq!(next as usize, first.len() + second.len() + third.len() + 3);
        // a missing file is just an empty transcript at the same offset
        let (lines, same) = read_transcript_from(&dir.path().join("gone.jsonl"), 7)
            .await
            .expect("a missing-file read");
        assert!(lines.is_empty());
        assert_eq!(same, 7);
    }

    /// A rejected snapshot still scrapes its usage map and model string
    #[tokio::test]
    async fn lean_snapshot_falls_back_to_a_scrape() {
        // a snapshot from the future: the typed loader must reject version 99
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("session.json");
        std::fs::write(
            &path,
            r#"{"version":99,"provider":{"provider":"bedrock","model":"claude-next"},
               "usage":{"main":{"input_tokens":5,"output_tokens":1}}}"#,
        )
        .unwrap();
        let lean = read_lean_snapshot(&path).await.expect("a scraped snapshot");
        // the scrape keeps the tokens and the model string but no provider
        assert!(lean.provider.is_none());
        assert_eq!(lean.model.as_deref(), Some("claude-next"));
        assert_eq!(lean.usage.get("main").map(|u| u.input_tokens), Some(5));
        // garbage yields nothing rather than a panic
        std::fs::write(&path, "not json").unwrap();
        assert!(read_lean_snapshot(&path).await.is_none());
    }

    /// Usage lines bucket exactly: sums match the ledger and turns count
    #[test]
    fn summarize_buckets_exact_sessions() {
        let mut fold = SessionFold::new("exact".to_owned());
        // two checkpoints an hour apart, covered exactly by the ledger
        fold.fold_line(&usage_line("main", 600, 60, 10 * HOUR_MS));
        fold.fold_line(&usage_line("main", 400, 40, 11 * HOUR_MS));
        fold.lean = Some(LeanSnapshot {
            provider: Some(bedrock_provider()),
            model: Some("claude-opus-5-5".to_owned()),
            usage: ledger("main", 1_000, 100),
        });
        let summary = fold.summarize(&PricingCtx::default(), false);
        // nothing was left to estimate
        assert!(summary.exact_hours);
        assert_eq!(summary.turns, Some(2));
        assert_eq!(summary.totals.input_tokens, 1_000);
        // the two checkpoints landed in two different local hours
        let input_sum: f64 = summary.hours.values().map(|bucket| bucket.input).sum();
        assert!((input_sum - 1_000.0).abs() < 1e-6);
        assert_eq!(
            summary
                .hours
                .values()
                .filter(|bucket| bucket.input > 0.0)
                .count(),
            2
        );
        // the session priced under its own model's builtin rates
        assert!(summary.cost.is_some());
        let window = summary.agents.get("main").expect("an agent window");
        assert_eq!(window.totals.input_tokens, 1_000);
        assert!(window.cost.is_some());
    }

    /// Provider-stall lines mark activity but never disturb the usage math
    #[test]
    fn stall_lines_leave_usage_undisturbed() {
        let mut fold = SessionFold::new("stalled".to_owned());
        // the same two checkpoints as the exact-session test, with the whole
        // retry/pause lifecycle interleaved between them
        fold.fold_line(&usage_line("main", 600, 60, 10 * HOUR_MS));
        fold.fold_line(&TranscriptLine {
            ts: Some(10 * HOUR_MS + 1_000),
            item: TranscriptItem::LlmRetry {
                agent: "main".to_owned(),
                retry_id: "main#1".to_owned(),
                attempt: 1,
                max_attempts: 12,
                error: "429 Too Many Requests".to_owned(),
                request_id: None,
                backoff_ms: 2_000,
            },
        });
        fold.fold_line(&TranscriptLine {
            ts: Some(10 * HOUR_MS + 3_000),
            item: TranscriptItem::LlmRetryResolved {
                agent: "main".to_owned(),
                retry_id: "main#1".to_owned(),
                attempts: 2,
                succeeded: true,
            },
        });
        fold.fold_line(&TranscriptLine {
            ts: Some(10 * HOUR_MS + 4_000),
            item: TranscriptItem::ProviderPaused {
                pause: thorpal::PauseKind::Auth,
                agent: "main".to_owned(),
                error: "401 Unauthorized".to_owned(),
                request_id: None,
            },
        });
        fold.fold_line(&TranscriptLine {
            ts: Some(10 * HOUR_MS + 64_000),
            item: TranscriptItem::ProviderResumed {
                pause: thorpal::PauseKind::Auth,
                recovered: true,
                paused_ms: 60_000,
                probe_attempts: 0,
            },
        });
        fold.fold_line(&usage_line("main", 400, 40, 11 * HOUR_MS));
        fold.lean = Some(LeanSnapshot {
            provider: Some(bedrock_provider()),
            model: Some("claude-opus-5-5".to_owned()),
            usage: ledger("main", 1_000, 100),
        });
        let summary = fold.summarize(&PricingCtx::default(), false);
        // the usage math matches the stall-free exact session: same turns,
        // same totals, same two populated hour buckets, nothing estimated
        assert!(summary.exact_hours);
        assert_eq!(summary.turns, Some(2));
        assert_eq!(summary.totals.input_tokens, 1_000);
        let input_sum: f64 = summary.hours.values().map(|bucket| bucket.input).sum();
        assert!((input_sum - 1_000.0).abs() < 1e-6);
        assert_eq!(
            summary
                .hours
                .values()
                .filter(|bucket| bucket.input > 0.0)
                .count(),
            2
        );
        // the stall lines named only the main agent, so no window appeared
        assert_eq!(summary.agents.len(), 1);
    }

    /// Offered-tool manifests union per role across a session's process runs
    #[test]
    fn offered_manifests_union_per_role() {
        let mut fold = SessionFold::new("offered".to_owned()).with_telemetry();
        // two process runs recorded different rosters for one role (a binary
        // upgrade mid-session), plus a second role's manifest
        fold.fold_line(&TranscriptLine {
            ts: Some(10 * HOUR_MS),
            item: TranscriptItem::ToolsOffered {
                role: "ReverseEngineer".to_owned(),
                tools: vec!["decomp_function".to_owned(), "think".to_owned()],
                version: "0.1.0".to_owned(),
            },
        });
        fold.fold_line(&TranscriptLine {
            ts: Some(11 * HOUR_MS),
            item: TranscriptItem::ToolsOffered {
                role: "ReverseEngineer".to_owned(),
                tools: vec!["memory_map".to_owned(), "think".to_owned()],
                version: "0.2.0".to_owned(),
            },
        });
        fold.fold_line(&TranscriptLine {
            ts: Some(11 * HOUR_MS),
            item: TranscriptItem::ToolsOffered {
                role: "main".to_owned(),
                tools: vec!["reverse_engineer".to_owned()],
                version: "0.2.0".to_owned(),
            },
        });
        // each role's roster is the union across its manifests, surfaced on
        // the telemetry summary the exporter consumes
        let summary = fold.summarize(&PricingCtx::default(), false);
        let telemetry = summary.telemetry.expect("a telemetry summary");
        let re = telemetry
            .tools_offered
            .get("ReverseEngineer")
            .expect("a roster");
        assert_eq!(re.len(), 3);
        assert!(re.contains(&"decomp_function".to_owned()));
        assert!(re.contains(&"memory_map".to_owned()));
        assert!(re.contains(&"think".to_owned()));
        let main = telemetry.tools_offered.get("main").expect("a roster");
        assert_eq!(main.len(), 1);
        // the newest manifest's harness version wins
        assert_eq!(telemetry.harness_version.as_deref(), Some("0.2.0"));
        // manifest lines name no agent, so no per-agent window appeared
        assert!(fold.windows.is_empty());
    }

    /// With no usage lines the ledger distributes over estimate weights
    #[test]
    fn summarize_distributes_remainder_over_weights() {
        let mut fold = SessionFold::new("old".to_owned());
        // one hour gets a 100-char payload, the next a 300-char payload, so
        // the weights split 25/75 across the two hours
        fold.fold_line(&call_line("main", "call-1", 100, 20 * HOUR_MS));
        fold.fold_line(&result_line("main", "call-1", 20 * HOUR_MS + 1_000));
        fold.fold_line(&call_line("main", "call-2", 300, 21 * HOUR_MS));
        fold.fold_line(&result_line("main", "call-2", 21 * HOUR_MS + 1_000));
        fold.lean = Some(LeanSnapshot {
            provider: Some(bedrock_provider()),
            model: Some("claude-opus-5-5".to_owned()),
            usage: ledger("main", 1_000, 100),
        });
        let summary = fold.summarize(&PricingCtx::default(), false);
        // everything was estimated, but the sums still match the ledger
        assert!(!summary.exact_hours);
        assert_eq!(summary.turns, None);
        let input_sum: f64 = summary.hours.values().map(|bucket| bucket.input).sum();
        assert!((input_sum - 1_000.0).abs() < 1e-6);
        // the split follows the 25/75 weights
        let mut inputs: Vec<f64> = summary
            .hours
            .values()
            .map(|bucket| bucket.input)
            .filter(|input| *input > 0.0)
            .collect();
        inputs.sort_by(f64::total_cmp);
        assert_eq!(inputs.len(), 2);
        assert!((inputs[0] - 250.0).abs() < 1e-6);
        assert!((inputs[1] - 750.0).abs() < 1e-6);
        // the tool call tallies folded too
        assert_eq!(summary.tool_calls, 2);
        assert_eq!(summary.tools.get("decomp_function"), Some(&2));
    }

    /// A mixed session keeps exact buckets and distributes only the remainder
    #[test]
    fn summarize_mixes_exact_and_remainder() {
        let mut fold = SessionFold::new("mixed".to_owned());
        // pre-upgrade activity leaves weights, then a checkpoint lands exactly
        fold.fold_line(&call_line("main", "call-1", 400, 30 * HOUR_MS));
        fold.fold_line(&result_line("main", "call-1", 30 * HOUR_MS + 1_000));
        fold.fold_line(&usage_line("main", 600, 60, 31 * HOUR_MS));
        // the ledger carries more than the one replayed checkpoint
        fold.lean = Some(LeanSnapshot {
            provider: None,
            model: None,
            usage: ledger("main", 1_000, 100),
        });
        let summary = fold.summarize(&PricingCtx::default(), false);
        // the remainder (400 input) spread over the pre-checkpoint weights
        assert!(!summary.exact_hours);
        assert_eq!(summary.totals.input_tokens, 1_000);
        let input_sum: f64 = summary.hours.values().map(|bucket| bucket.input).sum();
        assert!((input_sum - 1_000.0).abs() < 1e-6);
        // no provider parsed, so nothing priced
        assert!(summary.cost.is_none());
        // a transcript ahead of the ledger can never undercount: totals are
        // the per-field max of the two sources
        let mut ahead = SessionFold::new("ahead".to_owned());
        ahead.fold_line(&usage_line("main", 2_000, 200, 31 * HOUR_MS));
        ahead.lean = Some(LeanSnapshot {
            provider: None,
            model: None,
            usage: ledger("main", 1_000, 100),
        });
        let summary = ahead.summarize(&PricingCtx::default(), false);
        assert_eq!(summary.totals.input_tokens, 2_000);
        assert!(summary.exact_hours);
    }

    /// Busy intervals split their active time across the hours they span
    #[test]
    fn summarize_intersects_active_time_with_hours() {
        let mut fold = SessionFold::new("active".to_owned());
        // a call opens 30 minutes before an hour boundary and closes 30 after
        let start = 40 * HOUR_MS + 30 * 60_000;
        let end = 41 * HOUR_MS + 30 * 60_000;
        fold.fold_line(&call_line("main", "call-1", 4, start));
        fold.fold_line(&result_line("main", "call-1", end));
        let summary = fold.summarize(&PricingCtx::default(), false);
        // the hour of busy time survives intact across the buckets
        assert_eq!(summary.active_ms, HOUR_MS);
        let bucket_sum: u64 = summary.hours.values().map(|bucket| bucket.active_ms).sum();
        assert_eq!(bucket_sum, HOUR_MS);
        // the session window spans the whole call
        assert_eq!(summary.first_ts, Some(start));
        assert_eq!(summary.last_ts, Some(end));
    }

    /// Inclusive-input providers normalize buckets to the uncached remainder
    #[test]
    fn summarize_normalizes_inclusive_input_buckets() {
        let mut fold = SessionFold::new("openai".to_owned());
        // one checkpoint whose provider-reported input includes its cache reads
        fold.fold_line(&TranscriptLine {
            ts: Some(50 * HOUR_MS),
            item: TranscriptItem::Usage {
                agent: "main".to_owned(),
                input: 100,
                output: 10,
                cached: 80,
                cache_creation: 0,
                total: 110,
            },
        });
        // the ledger matches and the provider speaks the OpenAI protocol
        let mut usage = BTreeMap::new();
        usage.insert(
            "main".to_owned(),
            UsageTotals {
                input_tokens: 100,
                output_tokens: 10,
                total_tokens: 110,
                cached_input_tokens: 80,
                ..Default::default()
            },
        );
        fold.lean = Some(LeanSnapshot {
            provider: Some(AiProviderConfig::LiteLlm {
                endpoint: "https://llm.example".to_owned(),
                api_key: None,
                model: "gpt-4.1-mini".to_owned(),
            }),
            model: Some("gpt-4.1-mini".to_owned()),
            usage,
        });
        let summary = fold.summarize(&PricingCtx::default(), false);
        // the bucket's input dropped to the uncached remainder, so the token
        // metric never double-counts the cache reads
        let bucket = summary.hours.values().next().expect("one bucket");
        assert!((bucket.input - 20.0).abs() < 1e-9);
        assert!((bucket.tokens() - (20.0 + 10.0 + 80.0)).abs() < 1e-9);
        // the normalized bucket prices identically to the raw session totals
        let cost = summary.cost.expect("a priced session");
        assert!((bucket.cost - cost).abs() < 1e-12);
    }

    /// Config overrides only touch sessions that ran the configured model
    #[test]
    fn overrides_gate_on_the_configured_model() {
        // the config prices a different model than this session ran
        let mismatched = PricingCtx {
            overrides: Some(AiPricing {
                input: Some(1.0),
                output: Some(1.0),
                cache_read: None,
                cache_write: None,
            }),
            configured_model: Some("some-other-model".to_owned()),
        };
        let provider = bedrock_provider();
        let model = session_cost_model(Some(&provider), &mismatched).expect("builtin rates");
        // the builtin opus rate survived instead of the override's $1
        assert!((model.pricing.input - 4.0).abs() < 1e-9);
        // the same override applies once the models match (case-insensitively)
        let matched = PricingCtx {
            overrides: mismatched.overrides.clone(),
            configured_model: Some("Claude-Opus-5-5".to_owned()),
        };
        let model = session_cost_model(Some(&provider), &matched).expect("override rates");
        assert!((model.pricing.input - 1.0).abs() < 1e-9);
        // and no provider at all stays unpriced
        assert!(session_cost_model(None, &matched).is_none());
    }

    /// Local hour keys shift with the hour and the day boundary
    #[test]
    fn hour_keys_follow_local_hours() {
        // two stamps one hour apart always land in adjacent local hours
        let first = hour_key(100 * HOUR_MS).expect("a key");
        let second = hour_key(101 * HOUR_MS).expect("a key");
        assert_ne!(first, second);
        // the local calendar advances across enough hours
        let far = chrono::Local
            .timestamp_millis_opt((100 * HOUR_MS + 24 * HOUR_MS) as i64)
            .single()
            .expect("a local stamp");
        assert_eq!(
            hour_key(100 * HOUR_MS + 24 * HOUR_MS).map(|(day, _)| day),
            Some(far.date_naive())
        );
    }
}
