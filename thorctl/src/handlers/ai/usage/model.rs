//! The compact data model the usage TUI aggregates and renders
//!
//! The scanner folds each session into one [`SessionSummary`] -- exact token
//! totals from its snapshot ledger reconciled with any replayed per-turn usage
//! lines, local day/hour buckets, activity intervals, and per-agent windows --
//! and [`GlobalStats::rebuild`] rolls every summary up into the aggregates the
//! views slice. Costs are priced per session with that session's own model
//! (sessions can span different models/providers), so a global cost is always
//! the sum of correctly-priced per-session costs.

use std::collections::BTreeMap;
use std::ops::AddAssign;

use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone, Timelike, Utc};
use thorpal::UsageTotals;

use crate::handlers::ai::chat::{ActivityIntervals, fmt_cost, fmt_duration, with_thousands};
use crate::handlers::ai::pricing::CostModel;

/// One local-time heatmap cell key: the calendar day plus the hour of day
pub type HourKey = (NaiveDate, u8);

/// What one local hour of one day cost: active time, tokens, and dollars
///
/// Token fields are floats because estimation distributes a session's exact
/// totals fractionally across its hours; the fractions always sum back to the
/// exact per-session totals. `cost` is zero for sessions whose model could not
/// be priced -- the aggregate tracks how many sessions that thinned.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HourBucket {
    /// The milliseconds of merged busy time inside this hour
    pub active_ms: u64,
    /// The input (prompt) tokens attributed to this hour
    pub input: f64,
    /// The output (completion) tokens attributed to this hour
    pub output: f64,
    /// The cache-read input tokens attributed to this hour
    pub cached: f64,
    /// The cache-write input tokens attributed to this hour
    pub cache_creation: f64,
    /// The estimated dollars this hour cost, over priceable sessions only
    pub cost: f64,
}

impl HourBucket {
    /// The tokens of every bucket summed, the heatmap's token metric
    ///
    /// The scanner normalizes inclusive-input providers to the uncached
    /// remainder before buckets land here, so cache reads never double-count.
    pub fn tokens(&self) -> f64 {
        // sum each bucket; cache reads count as real traffic here
        self.input + self.output + self.cached + self.cache_creation
    }
}

/// A usage tally's display token count: the provider total, else the sum
///
/// Older ledgers may lack a provider-reported total; the fallback sums the
/// four billing buckets instead (which can double-count cache reads under the
/// OpenAI protocol's inclusive input -- acceptable for the rare total-less
/// ledger).
///
/// # Arguments
///
/// * `totals` - The usage tally to size
pub fn token_sum(totals: &UsageTotals) -> u64 {
    // prefer the provider's own total when it recorded one
    if totals.total_tokens > 0 {
        totals.total_tokens
    } else {
        totals
            .input_tokens
            .saturating_add(totals.output_tokens)
            .saturating_add(totals.cached_input_tokens)
            .saturating_add(totals.cache_creation_input_tokens)
    }
}

impl AddAssign<&HourBucket> for HourBucket {
    /// Folds another bucket into this one
    ///
    /// # Arguments
    ///
    /// * `other` - The bucket to fold in
    fn add_assign(&mut self, other: &HourBucket) {
        // fold each component; floats cannot overflow into panics
        self.active_ms = self.active_ms.saturating_add(other.active_ms);
        self.input += other.input;
        self.output += other.output;
        self.cached += other.cached;
        self.cache_creation += other.cache_creation;
        self.cost += other.cost;
    }
}

/// Maps an epoch stamp onto its local day/hour heatmap key
///
/// # Arguments
///
/// * `ts` - Milliseconds since the Unix epoch to place
pub fn hour_key(ts: u64) -> Option<HourKey> {
    // a stamp past i64 range (or a pre-epoch clock) keys nothing
    let dt = Local
        .timestamp_millis_opt(i64::try_from(ts).ok()?)
        .single()?;
    Some((dt.date_naive(), dt.hour() as u8))
}

/// Prices a fractional token bucket with a session's cost model
///
/// Mirrors [`CostModel::cost`] on floats so estimation's fractional hour
/// buckets price exactly like the integer totals they sum back to.
///
/// # Arguments
///
/// * `model` - The session's resolved cost model
/// * `input` - The input tokens attributed to the bucket
/// * `output` - The output tokens attributed to the bucket
/// * `cached` - The cache-read input tokens attributed to the bucket
/// * `cache_creation` - The cache-write input tokens attributed to the bucket
pub fn cost_of_tokens(
    model: &CostModel,
    input: f64,
    output: f64,
    cached: f64,
    cache_creation: f64,
) -> f64 {
    // normalize to the fresh-input count under the provider's cache semantics
    let uncached = if model.input_includes_cached {
        (input - cached).max(0.0)
    } else {
        input
    };
    // price each bucket at its own rate, in dollars per million tokens
    let dollars = uncached * model.pricing.input
        + cached * model.pricing.cache_read
        + cache_creation * model.pricing.cache_write
        + output * model.pricing.output;
    dollars / 1_000_000.0
}

/// The role grouping key for an agent display name
///
/// Ledger keys look like `"main"`, `"ReverseEngineer(Europa)"`, `"btw:x"`, or
/// `"btw:x/Role(Moon)"`; grouping strips the unique moon name and side scope
/// so the same role aggregates across sessions.
///
/// # Arguments
///
/// * `agent` - The agent display name to group
pub fn role_of(agent: &str) -> String {
    // a side sub-agent's role sits after its scope prefix
    let bare = match agent.split_once('/') {
        Some((_, rest)) => rest,
        None => agent,
    };
    // a side conversation's root groups under one shared label
    if bare.starts_with("btw:") {
        return "side chats".to_owned();
    }
    // a sub-agent's role is its prefix before the unique moon name
    match bare.split_once('(') {
        Some((role, _)) => role.to_owned(),
        None => bare.to_owned(),
    }
}

/// One agent's window and spend inside a single session
#[derive(Debug, Clone, Default)]
pub struct AgentWindow {
    /// The role this agent groups under across sessions
    pub role: String,
    /// Milliseconds since the epoch of the agent's first stamped event
    pub first_ts: Option<u64>,
    /// Milliseconds since the epoch of the agent's last stamped event
    pub last_ts: Option<u64>,
    /// How many tool calls the agent made
    pub tool_calls: u64,
    /// The agent's effective token totals (replayed lines plus the ledger
    /// remainder, so crash skew in either direction never undercounts)
    pub totals: UsageTotals,
    /// The agent's estimated cost under the session's model, when priceable
    pub cost: Option<f64>,
}

/// Everything the views need to know about one session
#[derive(Debug, Clone, Default)]
pub struct SessionSummary {
    /// The session's name (its directory name under the sessions root)
    pub name: String,
    /// When the session was created, from its metadata when readable
    pub created_at: Option<DateTime<Utc>>,
    /// The analyzed binary's file name, for the sessions table
    pub binary: Option<String>,
    /// The model id the session ran against, when known
    pub model: Option<String>,
    /// The session's effective token totals summed across every agent
    pub totals: UsageTotals,
    /// The session's estimated cost, when its model is priceable
    pub cost: Option<f64>,
    /// The estimated dollars prompt caching saved (or cost) this session
    pub cache_savings: Option<f64>,
    /// Milliseconds since the epoch of the first stamped transcript event
    pub first_ts: Option<u64>,
    /// Milliseconds since the epoch of the last stamped transcript event
    pub last_ts: Option<u64>,
    /// The merged busy time across the whole session, in milliseconds
    pub active_ms: u64,
    /// The merged `(start, end)` busy intervals, for the global timeline
    pub activity: Vec<(u64, u64)>,
    /// Each agent's window and spend, keyed by its ledger display name
    pub agents: BTreeMap<String, AgentWindow>,
    /// How many calls each tool got, for the top-tools panels
    pub tools: BTreeMap<String, u64>,
    /// The session's total tool calls
    pub tool_calls: u64,
    /// The model turns the transcript recorded, when any usage lines exist
    pub turns: Option<u64>,
    /// The session's spend bucketed by local day/hour
    pub hours: BTreeMap<HourKey, HourBucket>,
    /// The telemetry exporter's extra counters, boxed off the usage TUI's hot
    /// path and `None` for every scan that did not ask for them
    pub telemetry: Option<Box<crate::handlers::ai::telemetry::fold::TelemetrySummary>>,
    /// Whether any of the session's files changed in the last ~2 minutes
    pub live: bool,
    /// Whether every hour bucket came from exact per-turn usage lines
    ///
    /// False means some of the ledger predates durable usage lines and was
    /// distributed over estimate weights; the views mark those figures `~`.
    pub exact_hours: bool,
}

impl SessionSummary {
    /// The session's display token count: the provider total, else the sum
    pub fn tokens(&self) -> u64 {
        // size the session's effective totals with the shared helper
        token_sum(&self.totals)
    }

    /// The local calendar day of the session's last recorded activity
    pub fn last_active_day(&self) -> Option<NaiveDate> {
        // prefer the transcript's last stamp, falling back to creation time
        match self.last_ts.and_then(hour_key) {
            Some((day, _)) => Some(day),
            None => self
                .created_at
                .map(|created| created.with_timezone(&Local).date_naive()),
        }
    }
}

/// The metric the global usage bars and sessions table rank sessions by
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageMetric {
    /// The estimated dollar cost under each session's own model
    Cost,
    /// The provider-reported total tokens
    TotalTokens,
    /// The input (prompt) tokens
    InputTokens,
    /// The output (completion) tokens
    OutputTokens,
    /// The merged busy time
    ActiveTime,
    /// The tool calls made
    ToolCalls,
    /// The model turns recorded by per-turn usage lines
    Turns,
}

impl UsageMetric {
    /// The metrics in their cycling order
    const ALL: [UsageMetric; 7] = [
        UsageMetric::Cost,
        UsageMetric::TotalTokens,
        UsageMetric::InputTokens,
        UsageMetric::OutputTokens,
        UsageMetric::ActiveTime,
        UsageMetric::ToolCalls,
        UsageMetric::Turns,
    ];

    /// The next metric in the cycle, wrapping at the end
    pub fn next(self) -> UsageMetric {
        // find this metric's position and step forward with wraparound
        let pos = UsageMetric::ALL
            .iter()
            .position(|m| *m == self)
            .unwrap_or(0);
        UsageMetric::ALL[(pos + 1) % UsageMetric::ALL.len()]
    }

    /// The previous metric in the cycle, wrapping at the start
    pub fn prev(self) -> UsageMetric {
        // find this metric's position and step backward with wraparound
        let pos = UsageMetric::ALL
            .iter()
            .position(|m| *m == self)
            .unwrap_or(0);
        UsageMetric::ALL[(pos + UsageMetric::ALL.len() - 1) % UsageMetric::ALL.len()]
    }

    /// The human-facing label for this metric
    pub fn label(self) -> &'static str {
        match self {
            UsageMetric::Cost => "cost ($)",
            UsageMetric::TotalTokens => "total tokens",
            UsageMetric::InputTokens => "input tokens",
            UsageMetric::OutputTokens => "output tokens",
            UsageMetric::ActiveTime => "active time",
            UsageMetric::ToolCalls => "tool calls",
            UsageMetric::Turns => "turns",
        }
    }

    /// This metric's value for one session, or `None` when it is unknowable
    ///
    /// # Arguments
    ///
    /// * `session` - The session summary to read
    pub fn value(self, session: &SessionSummary) -> Option<f64> {
        match self {
            // cost needs the session's model to have priced
            UsageMetric::Cost => session.cost,
            UsageMetric::TotalTokens => Some(session.tokens() as f64),
            UsageMetric::InputTokens => Some(session.totals.input_tokens as f64),
            UsageMetric::OutputTokens => Some(session.totals.output_tokens as f64),
            UsageMetric::ActiveTime => Some(session.active_ms as f64),
            UsageMetric::ToolCalls => Some(session.tool_calls as f64),
            // sessions saved before usage lines carry no turn counts
            UsageMetric::Turns => session.turns.map(|turns| turns as f64),
        }
    }

    /// Formats one of this metric's values for a value column
    ///
    /// # Arguments
    ///
    /// * `value` - The value to format
    pub fn format(self, value: f64) -> String {
        match self {
            UsageMetric::Cost => fmt_cost(value),
            UsageMetric::ActiveTime => fmt_duration(value as u64),
            _ => with_thousands(value.round() as u64),
        }
    }
}

/// The metric the heatmap and trend views color by
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeatMetric {
    /// The merged busy time inside each bucket
    ActiveTime,
    /// The tokens attributed to each bucket
    Tokens,
    /// The estimated dollars attributed to each bucket
    Cost,
}

impl HeatMetric {
    /// The metrics in their cycling order
    const ALL: [HeatMetric; 3] = [HeatMetric::ActiveTime, HeatMetric::Tokens, HeatMetric::Cost];

    /// The next metric in the cycle, wrapping at the end
    pub fn next(self) -> HeatMetric {
        // find this metric's position and step forward with wraparound
        let pos = HeatMetric::ALL.iter().position(|m| *m == self).unwrap_or(0);
        HeatMetric::ALL[(pos + 1) % HeatMetric::ALL.len()]
    }

    /// The previous metric in the cycle, wrapping at the start
    pub fn prev(self) -> HeatMetric {
        // find this metric's position and step backward with wraparound
        let pos = HeatMetric::ALL.iter().position(|m| *m == self).unwrap_or(0);
        HeatMetric::ALL[(pos + HeatMetric::ALL.len() - 1) % HeatMetric::ALL.len()]
    }

    /// The human-facing label for this metric
    pub fn label(self) -> &'static str {
        match self {
            HeatMetric::ActiveTime => "active time",
            HeatMetric::Tokens => "tokens",
            HeatMetric::Cost => "cost ($)",
        }
    }

    /// This metric's value for one bucket
    ///
    /// # Arguments
    ///
    /// * `bucket` - The hour bucket to read
    pub fn value(self, bucket: &HourBucket) -> f64 {
        match self {
            HeatMetric::ActiveTime => bucket.active_ms as f64,
            HeatMetric::Tokens => bucket.tokens(),
            HeatMetric::Cost => bucket.cost,
        }
    }

    /// Formats one of this metric's values for a legend or value column
    ///
    /// # Arguments
    ///
    /// * `value` - The value to format
    pub fn format(self, value: f64) -> String {
        match self {
            HeatMetric::ActiveTime => fmt_duration(value as u64),
            HeatMetric::Tokens => with_thousands(value.round() as u64),
            HeatMetric::Cost => fmt_cost(value),
        }
    }
}

/// One time window's rollup on the overview dashboard
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowTotals {
    /// The window's display label
    pub label: &'static str,
    /// The window's summed spend
    pub bucket: HourBucket,
    /// How many sessions were active inside the window
    pub sessions: usize,
}

/// One model's rollup across every session that ran it
#[derive(Debug, Clone, Default)]
pub struct ModelRollup {
    /// How many sessions ran this model
    pub sessions: usize,
    /// The summed effective token totals
    pub totals: UsageTotals,
    /// The summed cost over this model's priceable sessions
    pub cost: f64,
    /// How many of this model's sessions priced
    pub priced: usize,
    /// The summed cache savings over this model's priceable sessions
    pub savings: f64,
    /// The summed busy time across this model's sessions, in milliseconds
    pub active_ms: u64,
}

/// One agent role's rollup across every session
#[derive(Debug, Clone, Default)]
pub struct RoleRollup {
    /// How many per-session agent windows folded into this role
    pub count: usize,
    /// The summed effective token totals
    pub totals: UsageTotals,
    /// The summed cost over windows whose sessions priced
    pub cost: f64,
    /// How many of the folded windows carried a priced cost
    pub priced: usize,
    /// The summed tool calls
    pub tool_calls: u64,
}

/// The cross-session aggregates every view renders from
///
/// Rebuilt from the full summary map whenever the scanner delivers a change;
/// cheap enough (hundreds of sessions, thousands of buckets) that no
/// incremental maintenance is worth the bugs.
#[derive(Debug, Clone, Default)]
pub struct GlobalStats {
    /// The overview windows: today, 7 days, 30 days, and all time
    pub windows: [WindowTotals; 4],
    /// The spend summed per local calendar day, for the trend view
    pub days: BTreeMap<NaiveDate, HourBucket>,
    /// The spend per local day/hour cell, for the heatmap
    pub grid: BTreeMap<HourKey, HourBucket>,
    /// The spend per weekday/hour cell (Monday first), the heatmap aggregate
    pub weekdays: [[HourBucket; 24]; 7],
    /// Each model's rollup, keyed by its model id
    pub models: BTreeMap<String, ModelRollup>,
    /// Each agent role's rollup, keyed by the role grouping label
    pub roles: BTreeMap<String, RoleRollup>,
    /// Total calls per tool across every session
    pub tools: BTreeMap<String, u64>,
    /// Every session's busy intervals merged, for the global timeline axis
    pub union_activity: Vec<(u64, u64)>,
    /// The all-time rollup (the last overview window, kept handy)
    pub totals: HourBucket,
    /// The summed cache savings across priceable sessions
    pub cache_savings: f64,
    /// How many sessions were scanned in total
    pub total_sessions: usize,
    /// How many sessions are currently live (files changed recently)
    pub live_sessions: usize,
    /// How many sessions could not be priced (no cost in any rollup)
    pub unpriced: usize,
    /// Whether any session's hour buckets came from estimation
    pub estimated: bool,
}

impl GlobalStats {
    /// Rolls every session summary up into fresh aggregates
    ///
    /// # Arguments
    ///
    /// * `sessions` - The per-session summaries to aggregate
    pub fn rebuild(sessions: &BTreeMap<String, SessionSummary>) -> GlobalStats {
        let mut stats = GlobalStats {
            total_sessions: sessions.len(),
            ..GlobalStats::default()
        };
        // the union of busy intervals reuses the same gap-fusing merger the
        // gantt state uses, so the global axis compresses identically
        let mut union = ActivityIntervals::default();
        for session in sessions.values() {
            // fold the session's hour buckets into the grid, days, and weekdays
            for (key, bucket) in &session.hours {
                *stats.grid.entry(*key).or_default() += bucket;
                *stats.days.entry(key.0).or_default() += bucket;
                let weekday = key.0.weekday().num_days_from_monday() as usize;
                stats.weekdays[weekday][key.1 as usize] += bucket;
            }
            // fold the busy intervals into the global union
            for (start, end) in &session.activity {
                union.record(*start, *end);
            }
            // fold the per-model rollup under the session's model id
            let model = session
                .model
                .clone()
                .unwrap_or_else(|| "(unknown)".to_owned());
            let rollup = stats.models.entry(model).or_default();
            rollup.sessions += 1;
            rollup.totals += session.totals;
            rollup.active_ms = rollup.active_ms.saturating_add(session.active_ms);
            if let Some(cost) = session.cost {
                rollup.cost += cost;
                rollup.priced += 1;
            }
            if let Some(savings) = session.cache_savings {
                rollup.savings += savings;
            }
            // fold each agent window into its role's rollup
            for window in session.agents.values() {
                let role = stats.roles.entry(window.role.clone()).or_default();
                role.count += 1;
                role.totals += window.totals;
                role.tool_calls = role.tool_calls.saturating_add(window.tool_calls);
                if let Some(cost) = window.cost {
                    role.cost += cost;
                    role.priced += 1;
                }
            }
            // fold the per-tool call counts
            for (tool, calls) in &session.tools {
                *stats.tools.entry(tool.clone()).or_default() += calls;
            }
            // fold the session-level counters
            if let Some(savings) = session.cache_savings {
                stats.cache_savings += savings;
            }
            if session.cost.is_none() {
                stats.unpriced += 1;
            }
            if session.live {
                stats.live_sessions += 1;
            }
            if !session.exact_hours {
                stats.estimated = true;
            }
        }
        stats.union_activity = union.segments().to_vec();
        // build the overview windows off local-midnight day boundaries
        let today = Local::now().date_naive();
        let starts = [
            Some(today),
            today.checked_sub_days(chrono::Days::new(6)),
            today.checked_sub_days(chrono::Days::new(29)),
            None,
        ];
        let labels = ["today", "7 days", "30 days", "all time"];
        for (index, (label, start)) in labels.iter().zip(starts).enumerate() {
            let mut window = WindowTotals {
                label,
                ..WindowTotals::default()
            };
            // sum every day at or past the window's start boundary
            for (day, bucket) in &stats.days {
                if start.is_none_or(|start| *day >= start) {
                    window.bucket += bucket;
                }
            }
            // a session counts toward a window when it was active inside it
            window.sessions = sessions
                .values()
                .filter(|session| match (session.last_active_day(), start) {
                    // the unbounded all-time window counts every session,
                    // even one with no stamp and no readable metadata
                    (_, None) => true,
                    // dated windows require a known day inside them
                    (Some(day), Some(start)) => day >= start,
                    (None, Some(_)) => false,
                })
                .count();
            stats.windows[index] = window;
        }
        // keep the all-time rollup handy for footers and side panels
        stats.totals = stats.windows[3].bucket;
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::TimeZone;

    /// Builds an epoch stamp at noon local time on a day
    fn noon_ms(day: NaiveDate) -> u64 {
        // noon dodges every DST transition this test could trip over
        let noon = day.and_hms_opt(12, 0, 0).expect("noon exists");
        Local
            .from_local_datetime(&noon)
            .single()
            .expect("an unambiguous local noon")
            .timestamp_millis()
            .max(0) as u64
    }

    /// Two sessions roll up into windows, grids, and per-model/role rollups
    #[test]
    fn rebuild_rolls_sessions_up() {
        let today = Local::now().date_naive();
        let old_day = today
            .checked_sub_days(chrono::Days::new(45))
            .expect("a date 45 days back");
        let mut sessions: BTreeMap<String, SessionSummary> = BTreeMap::new();
        // session one: priced spend today, live, exact
        let mut one = SessionSummary {
            name: "one".to_owned(),
            model: Some("model-a".to_owned()),
            cost: Some(2.0),
            cache_savings: Some(0.5),
            last_ts: Some(noon_ms(today)),
            live: true,
            exact_hours: true,
            tool_calls: 3,
            ..SessionSummary::default()
        };
        one.totals.input_tokens = 100;
        one.hours.insert(
            (today, 10),
            HourBucket {
                active_ms: 60_000,
                input: 100.0,
                cost: 2.0,
                ..HourBucket::default()
            },
        );
        one.activity = vec![(1_000, 2_000)];
        one.agents.insert(
            "main".to_owned(),
            AgentWindow {
                role: "main".to_owned(),
                tool_calls: 3,
                cost: Some(2.0),
                ..AgentWindow::default()
            },
        );
        one.tools.insert("decomp_function".to_owned(), 3);
        sessions.insert(one.name.clone(), one);
        // session two: old, unpriced, estimated spend
        let mut two = SessionSummary {
            name: "two".to_owned(),
            last_ts: Some(noon_ms(old_day)),
            exact_hours: false,
            ..SessionSummary::default()
        };
        two.totals.input_tokens = 50;
        two.hours.insert(
            (old_day, 5),
            HourBucket {
                input: 50.0,
                ..HourBucket::default()
            },
        );
        sessions.insert(two.name.clone(), two);
        let stats = GlobalStats::rebuild(&sessions);
        // the headline counters cover both sessions
        assert_eq!(stats.total_sessions, 2);
        assert_eq!(stats.live_sessions, 1);
        assert_eq!(stats.unpriced, 1);
        assert!(stats.estimated);
        assert_eq!(stats.days.len(), 2);
        // today's window holds only today's spend; all time holds both
        assert!((stats.windows[0].bucket.input - 100.0).abs() < 1e-6);
        assert!((stats.windows[3].bucket.input - 150.0).abs() < 1e-6);
        assert_eq!(stats.windows[0].sessions, 1);
        assert_eq!(stats.windows[3].sessions, 2);
        // the model rollups split by model id, with the unknown bucketed
        assert_eq!(stats.models.len(), 2);
        assert_eq!(stats.models["model-a"].priced, 1);
        assert_eq!(stats.models["(unknown)"].sessions, 1);
        // the role and tool rollups folded session one's window
        assert_eq!(stats.roles["main"].tool_calls, 3);
        assert_eq!(stats.tools["decomp_function"], 3);
        // the weekday grid carries today's bucket under today's weekday
        let weekday = today.weekday().num_days_from_monday() as usize;
        assert!((stats.weekdays[weekday][10].input - 100.0).abs() < 1e-6);
        // the cache savings summed over priced sessions
        assert!((stats.cache_savings - 0.5).abs() < 1e-9);
    }

    /// The role grouping strips moons and side scopes
    #[test]
    fn roles_group_across_name_shapes() {
        assert_eq!(role_of("main"), "main");
        assert_eq!(role_of("ReverseEngineer(Europa)"), "ReverseEngineer");
        assert_eq!(role_of("btw:crypto"), "side chats");
        assert_eq!(role_of("btw:crypto/Researcher(Moon)"), "Researcher");
        assert_eq!(role_of("ghidra"), "ghidra");
    }
}
