//! Contains models for hunt entities
//!
//! A hunt is a user-created, bounded, paced pass of a frozen set of rules over
//! historical data. The hunt machinery is an engine-generic chassis: everything
//! engine-specific (what a corpus item is, how it is fetched and evaluated, and
//! how findings are written) hangs off the [`HuntEngine`] plug points below.

use chrono::{DateTime, Utc};
use std::collections::HashMap;
use uuid::Uuid;

use crate::models::{
    Entity, EntityKinds, EntityMetadata, EventData, EventType, HuntChunkEvent, KindDepth, YaraRule,
};

/// The rule engine a hunt runs
///
/// M4 ships Yara; adding an engine means implementing the plug point methods
/// on this enum (rule kind, snapshot filter, chunk event data) plus the
/// execution side plugs that land with the engine's worker.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    strum::Display,
    strum::EnumString,
    strum::AsRefStr,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum HuntEngine {
    /// Scan sample bytes with yara rules
    Yara,
}

impl HuntEngine {
    /// The entity kind this engine's rules are
    #[must_use]
    pub fn rule_kind(&self) -> EntityKinds {
        match self {
            HuntEngine::Yara => EntityKinds::YaraRule,
        }
    }

    /// Whether this engine's snapshot filter excludes a rule at hunt creation
    ///
    /// The yara engine excludes rules already quarantined as `Noisy` or
    /// `Slow`, matching the live context build's list time skip; `Broken`
    /// tagged rules are deliberately not excluded since their sources drop out
    /// at the compile classify step and a broken tagged rule whose text was
    /// since fixed hunts exactly like it scans. Exempt rules are never
    /// excluded, even when a stale status tag survived a quarantine race —
    /// exemption always wins over quarantine. Engines without a quarantine
    /// concept make this a no-op.
    ///
    /// # Arguments
    ///
    /// * `rule` - The rule entity to test against this engine's snapshot filter
    #[must_use]
    pub fn excludes_from_snapshot(&self, rule: &Entity) -> bool {
        match self {
            HuntEngine::Yara => {
                // exemption always wins over a stale quarantine status tag
                if matches!(&rule.metadata, EntityMetadata::YaraRule(yara) if yara.quarantine_exempt)
                {
                    return false;
                }
                // check this rules status tag for quarantine values
                match rule.tags.get(YaraRule::STATUS_TAG) {
                    // quarantined rules carry a Noisy or Slow status tag value
                    Some(values) => {
                        values.contains_key(YaraRule::STATUS_NOISY)
                            || values.contains_key(YaraRule::STATUS_SLOW)
                    }
                    // rules with no status tag are not quarantined
                    None => false,
                }
            }
        }
    }

    /// Wrap a chunk event payload in this engine's event data variant
    ///
    /// Each engine keeps its own chunk event kind so the queue a chunk lands
    /// on is the engine's choice, never the chassis's.
    ///
    /// # Arguments
    ///
    /// * `chunk` - The chunk event payload to wrap
    #[must_use]
    pub fn chunk_event_data(&self, chunk: HuntChunkEvent) -> EventData {
        match self {
            HuntEngine::Yara => EventData::YaraHuntChunk(chunk),
        }
    }

    /// The event kind this engine's chunk events ride on
    ///
    /// This names the queue the checkpoint commit script emits the next chunk
    /// event into, even when a particular commit emits nothing.
    #[must_use]
    pub fn chunk_event_type(&self) -> EventType {
        match self {
            HuntEngine::Yara => EventType::YaraHuntChunk,
        }
    }
}

/// How a hunt chose its rules
///
/// The selector is kept for display; the frozen snapshot resolved from it at
/// creation - filtered to the hunt engine's rule kind - is what actually
/// scans.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum HuntSelector {
    /// An explicit list of rule entity ids
    Rules(Vec<Uuid>),
    /// Every rule of the engine's kind carrying this entity tag at creation
    Tag {
        /// The tag key to match
        key: String,
        /// The tag value to match
        value: String,
    },
    /// Every rule of the engine's kind visible to the creator at creation
    All,
    /// The members of a RuleSet entity at creation time
    RuleSet(Uuid),
}

/// The lifecycle of a hunt
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::Display,
    strum::EnumString,
    strum::AsRefStr,
)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum HuntStatus {
    /// Created, but no chunk has checkpointed yet
    Created,
    /// At least one chunk has checkpointed and some lane still has work
    Running,
    /// Every lane exhausted its time range; terminal
    Completed,
    /// Cancelled by the submitter or an admin; terminal
    Cancelled,
    /// Stopped by the system because the hunt can no longer produce work
    /// (e.g. its frozen snapshot loads to zero usable rules); terminal
    Failed,
}

impl HuntStatus {
    /// Whether this status counts as active for quotas and the active tag
    ///
    /// A hunt is active while it is Created or Running; every other status is
    /// terminal.
    #[must_use]
    pub fn is_active(&self) -> bool {
        matches!(self, HuntStatus::Created | HuntStatus::Running)
    }
}

/// One lane: a contiguous time shard of the corpus, walked newest to oldest
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct HuntLane {
    /// The newest timestamp this lane covers
    pub start: DateTime<Utc>,
    /// The upload timestamp of the oldest line this lane has checkpointed
    pub current: DateTime<Utc>,
    /// The oldest timestamp this lane covers
    pub end: DateTime<Utc>,
    /// The checkpoint sequence number this lane expects next
    pub seq: u64,
    /// The server-side list cursor this lane is walking, while one is alive
    pub cursor: Option<Uuid>,
    /// Whether this lane has exhausted its time range
    pub exhausted: bool,
}

/// Scan counters persisted at each checkpoint
///
/// Counters are operational telemetry: a chunk replayed after a lost
/// checkpoint re-counts its work, so totals can drift. The `HuntId` tag
/// browse is the exact record of what a hunt found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct HuntCounters {
    /// How many corpus list lines the hunt has walked
    pub listed: u64,
    /// How many corpus items were fetched and evaluated
    pub scanned: u64,
    /// How many evaluations matched at least one rule
    pub matched: u64,
    /// How many findings the engine's writer reported written
    pub written: u64,
    /// How many finding writes the engine's writer reported suppressed
    pub suppressed: u64,
    /// How many items the engine's admission policy declined (yara: oversize)
    pub skipped: u64,
    /// How many items were deleted before they could be evaluated
    pub missing: u64,
    /// How many items failed to fetch or evaluate
    pub errors: u64,
    /// A bounded sample of per-item errors (capped by max_error_samples)
    pub error_samples: Vec<String>,
}

impl HuntCounters {
    /// Fold one chunk's counter deltas into these totals
    ///
    /// Every count saturates so replayed chunks can never wrap a total, and
    /// the error sample list stays bounded by the configured cap no matter how
    /// many chunks report errors.
    ///
    /// # Arguments
    ///
    /// * `delta` - The counter deltas from one processed chunk
    /// * `max_samples` - The max number of error samples to retain
    pub fn absorb(&mut self, delta: HuntCounters, max_samples: usize) {
        // fold each counter in saturating so replays can never wrap totals
        self.listed = self.listed.saturating_add(delta.listed);
        self.scanned = self.scanned.saturating_add(delta.scanned);
        self.matched = self.matched.saturating_add(delta.matched);
        self.written = self.written.saturating_add(delta.written);
        self.suppressed = self.suppressed.saturating_add(delta.suppressed);
        self.skipped = self.skipped.saturating_add(delta.skipped);
        self.missing = self.missing.saturating_add(delta.missing);
        self.errors = self.errors.saturating_add(delta.errors);
        // keep only enough new samples to reach the bounded cap
        let room = max_samples.saturating_sub(self.error_samples.len());
        self.error_samples
            .extend(delta.error_samples.into_iter().take(room));
    }
}

/// The shared corpus bounds: the file/repo list params shape
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct HuntCorpusBounds {
    /// The groups to list from; must be a subset of the hunt's groups
    pub groups: Vec<String>,
    /// The newest bound; clamped to the hunt's creation instant
    pub start: DateTime<Utc>,
    /// The oldest bound; defaults to the corpus kind's configured earliest
    pub end: DateTime<Utc>,
    /// Tag filters; strict AND across every pair, like the file list
    #[serde(default)]
    pub tags: HashMap<String, Vec<String>>,
    /// Whether tag matching is case-insensitive
    #[serde(default)]
    pub tags_case_insensitive: bool,
}

/// The corpus a hunt walks: a kind picking the listing, plus shared bounds
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum HuntCorpus {
    /// Walk the sample listing (the only kind an M4 engine scans)
    Files(HuntCorpusBounds),
    /// Walk the repo listing (rejected at create until an engine scans repos)
    Repos(HuntCorpusBounds),
}

impl HuntCorpus {
    /// Get the shared bounds for this corpus regardless of its kind
    #[must_use]
    pub fn bounds(&self) -> &HuntCorpusBounds {
        match self {
            HuntCorpus::Files(bounds) | HuntCorpus::Repos(bounds) => bounds,
        }
    }
}

/// The metadata for a user-created hunt
///
/// Hunt metadata is machine written: it is created through the dedicated hunt
/// create route and advanced by checkpoints, never through the generic entity
/// update path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct Hunt {
    /// The rule engine this hunt runs
    pub engine: HuntEngine,
    /// The selector as submitted, kept for display
    pub selector: HuntSelector,
    /// The frozen snapshot of rule entity ids resolved at creation
    pub rules: Vec<Uuid>,
    /// How many rules the engine's snapshot filter excluded (yara: Noisy/Slow)
    pub excluded_quarantined: u64,
    /// How many RuleSet members of another engine's kind were excluded
    pub excluded_foreign: u64,
    /// The corpus this hunt walks
    pub corpus: HuntCorpus,
    /// Where this hunt is in its lifecycle
    pub status: HuntStatus,
    /// Per-lane progress through the corpus
    pub lanes: Vec<HuntLane>,
    /// Scan counters, persisted at each checkpoint
    #[serde(default)]
    pub counters: HuntCounters,
    /// Why this hunt was marked Failed if the system stopped it
    #[serde(default)]
    pub failure: Option<String>,
}

impl Hunt {
    /// The entity tag key a hunt's status is mirrored under
    pub const STATUS_TAG: &str = "HuntStatus";

    /// The entity tag key marking Created or Running hunts
    ///
    /// Tag filters are strict AND so "status is Created or Running" is
    /// inexpressible in one tag query; this binary tag makes both the active
    /// hunt browse and the create-time quota census a single tag listing. It
    /// must never be written on any other entity kind since the quota census
    /// relies on it being hunt-exclusive (tag walks cannot filter by kind).
    pub const ACTIVE_TAG: &str = "HuntActive";

    /// The entity tag key a hunt's engine is mirrored under
    ///
    /// One `Hunt` kind cannot be split by the `kinds[]` list param so "my
    /// yara hunts" is a `HuntEngine=yara` tag filter.
    pub const ENGINE_TAG: &str = "HuntEngine";

    /// The entity tag key hunt written findings carry their hunt's id under
    ///
    /// `GET /api/entities/?kinds[]=<finding kind>&tags[HuntId][]=<hunt id>`
    /// is the browse path for "what did my hunt hit?"; entity tags are kind
    /// blind so the same query shape works for any engine's finding kinds.
    /// This tag is the exact record of what a hunt found - counters are only
    /// telemetry that can drift under replay.
    pub const ID_TAG: &str = "HuntId";
}

/// The all-optional request mirror of a corpus's bounds
///
/// Omitted bounds default to everything the creator can see up to the hunt's
/// creation instant.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct HuntCorpusBoundsRequest {
    /// The groups to list from; defaults to the hunt's groups
    pub groups: Vec<String>,
    /// The newest bound; clamped to the hunt's creation instant
    pub start: Option<DateTime<Utc>>,
    /// The oldest bound; defaults to the corpus kind's configured earliest
    pub end: Option<DateTime<Utc>>,
    /// Tag filters; strict AND across every pair, like the file list
    pub tags: HashMap<String, Vec<String>>,
    /// Whether tag matching is case-insensitive
    pub tags_case_insensitive: bool,
}

/// The all-optional request mirror of [`HuntCorpus`]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum HuntCorpusRequest {
    /// Walk the sample listing
    Files(HuntCorpusBoundsRequest),
    /// Walk the repo listing (rejected at create until an engine scans repos)
    Repos(HuntCorpusBoundsRequest),
}

/// A request to create a hunt
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct HuntCreateRequest {
    /// The name for the hunt entity
    pub name: String,
    /// The groups this hunt, and everything it writes, is bounded to
    pub groups: Vec<String>,
    /// The rule engine this hunt runs
    pub engine: HuntEngine,
    /// How to choose the rules this hunt freezes
    pub selector: HuntSelector,
    /// The corpus; omitted bounds default to everything up to creation
    #[serde(default)]
    pub corpus: Option<HuntCorpusRequest>,
    /// A description for the hunt entity
    #[serde(default)]
    pub description: Option<String>,
}

/// The commit for one processed hunt chunk
///
/// The admin service account's hunt workers are the only callers. The `seq`
/// field is the compare-and-set term: exactly one committer wins each sequence
/// number, losers get a 409 and drop their chain so duplicate deliveries
/// collapse instead of forking.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct HuntCheckpoint {
    /// The lane this chunk advanced
    pub lane: u32,
    /// The sequence number this chunk is committing: the CAS term
    pub seq: u64,
    /// The oldest upload timestamp this chunk processed
    pub current: DateTime<Utc>,
    /// The live list cursor to resume from, if one is still alive
    #[serde(default)]
    pub cursor: Option<Uuid>,
    /// Whether this lane exhausted its time range
    #[serde(default)]
    pub exhausted: bool,
    /// A terminal failure, when the hunt can no longer produce work
    #[serde(default)]
    pub failed: Option<String>,
    /// Counter deltas from this chunk
    #[serde(default)]
    pub counters: HuntCounters,
}

/// The outcome of committing one hunt chunk checkpoint
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct HuntCheckpointResponse {
    /// The hunt's status after this checkpoint committed
    pub status: HuntStatus,
    /// Whether this checkpoint emitted the next chunk event on its lane
    pub emitted: bool,
}

/// One line in a hunt's frozen rule snapshot listing
///
/// Lines for rules that were deleted after the freeze, or that the caller
/// cannot see, carry only the id (every optional field is None/empty). The
/// two cases are deliberately indistinguishable so listing a hunt never leaks
/// whether an invisible rule still exists.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct HuntRuleLine {
    /// The frozen rule entity id
    pub id: Uuid,
    /// The rule's name when it is visible to the caller
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The rule's entity kind when it is visible to the caller
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<EntityKinds>,
    /// When the rule was created, when it is visible to the caller
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<DateTime<Utc>>,
    /// The rule's current quarantine status tag values (e.g. Noisy/Slow/Broken)
    ///
    /// These are the current values of the rule's `YaraRuleStatus` tag, not
    /// the values at freeze time; empty when the rule is not quarantined.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub status: Vec<String>,
    /// Whether the rule is currently exempt from quarantine
    #[serde(default)]
    pub quarantine_exempt: bool,
}

/// Default the hunt frozen rule list limit to 50
fn default_hunt_rule_limit() -> usize {
    50
}

/// The query params for listing a hunt's frozen rules
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "api", derive(utoipa::IntoParams))]
#[cfg_attr(feature = "api", into_params(parameter_in = Query))]
pub struct HuntRuleListParams {
    /// The cursor to resume listing at
    ///
    /// The cursor is an index into the hunt's frozen rule vec encoded as a
    /// Uuid (`Uuid::from_u128(index)`) since the shared cursor wire type is a
    /// Uuid; it is opaque to clients and stays valid forever because the
    /// frozen snapshot is immutable.
    pub cursor: Option<Uuid>,
    /// The max number of frozen rule lines to return in this response
    #[serde(default = "default_hunt_rule_limit")]
    pub limit: usize,
}

impl Default for HuntRuleListParams {
    /// Create a default hunt frozen rule list params
    fn default() -> Self {
        HuntRuleListParams {
            cursor: None,
            limit: default_hunt_rule_limit(),
        }
    }
}

/// The options for listing a hunt's frozen rules as a client
#[derive(Debug, Clone)]
pub struct HuntRuleListOpts {
    /// The cursor to resume listing at
    pub cursor: Option<Uuid>,
    /// The number of frozen rule lines to request per page
    pub page_size: usize,
    /// The total number of frozen rule lines to return
    pub limit: Option<usize>,
}

impl Default for HuntRuleListOpts {
    /// Create a default hunt frozen rule list opts
    fn default() -> Self {
        HuntRuleListOpts {
            cursor: None,
            page_size: 50,
            limit: None,
        }
    }
}

impl HuntRuleListOpts {
    /// Set the cursor to resume listing at
    ///
    /// # Arguments
    ///
    /// * `cursor` - The cursor to resume at
    #[must_use]
    pub fn cursor(mut self, cursor: Uuid) -> Self {
        self.cursor = Some(cursor);
        self
    }

    /// Set the number of frozen rule lines to request per page
    ///
    /// # Arguments
    ///
    /// * `page_size` - The page size to use
    #[must_use]
    pub fn page_size(mut self, page_size: usize) -> Self {
        self.page_size = page_size;
        self
    }

    /// Set the total number of frozen rule lines to return
    ///
    /// # Arguments
    ///
    /// * `limit` - The max number of lines to return across all pages
    #[must_use]
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }
}

/// Default the hunt census recent display cap to 20
fn default_status_recent() -> usize {
    20
}

/// Default the hunt census window to 7 days
fn default_status_window_days() -> u32 {
    7
}

/// The query params for the admin hunt census
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "api", derive(utoipa::IntoParams))]
#[cfg_attr(feature = "api", into_params(parameter_in = Query))]
pub struct HuntStatusParams {
    /// The max number of recently terminal hunts to return (capped at 100)
    #[serde(default = "default_status_recent")]
    pub recent: usize,
    /// How many days of created hunts the recent window covers (capped at 30)
    #[serde(default = "default_status_window_days")]
    pub window_days: u32,
}

impl Default for HuntStatusParams {
    /// Create a default hunt census params
    fn default() -> Self {
        HuntStatusParams {
            recent: default_status_recent(),
            window_days: default_status_window_days(),
        }
    }
}

/// The options for reading the admin hunt census as a client
#[derive(Debug, Clone, Default)]
pub struct HuntStatusOpts {
    /// The max number of recently terminal hunts to return
    pub recent: Option<usize>,
    /// How many days of created hunts the recent window covers
    pub window_days: Option<u32>,
}

impl HuntStatusOpts {
    /// Set the max number of recently terminal hunts to return
    ///
    /// # Arguments
    ///
    /// * `recent` - The recent display cap to request
    #[must_use]
    pub fn recent(mut self, recent: usize) -> Self {
        self.recent = Some(recent);
        self
    }

    /// Set how many days of created hunts the recent window covers
    ///
    /// # Arguments
    ///
    /// * `window_days` - The window to request in days
    #[must_use]
    pub fn window_days(mut self, window_days: u32) -> Self {
        self.window_days = Some(window_days);
        self
    }
}

/// One hunt on the admin status page
///
/// A bounded projection of a hunt entity: the frozen rule snapshot (up to
/// `max_rules_per_hunt` ids) is reduced to a count, while the small, bounded
/// lanes and counters ride whole so the UI can reuse its lane progress math.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct HuntStatusLine {
    /// The unique id of this hunt
    pub id: Uuid,
    /// The name of this hunt
    pub name: String,
    /// The user that created this hunt
    pub submitter: String,
    /// The groups this hunt is visible to
    pub groups: Vec<String>,
    /// The engine this hunt scans with
    pub engine: HuntEngine,
    /// The current status of this hunt
    pub status: HuntStatus,
    /// When this hunt was created
    pub created: DateTime<Utc>,
    /// How many rules the frozen snapshot holds (never the ids themselves)
    pub frozen_rules: u64,
    /// The time lanes this hunt walks and their progress
    pub lanes: Vec<HuntLane>,
    /// The cumulative counters for this hunt
    pub counters: HuntCounters,
    /// Why this hunt failed if it did
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

impl HuntStatusLine {
    /// Project a hunt entity into a bounded status line
    ///
    /// Returns `None` for entities that are not hunts so census assembly can
    /// skip them defensively instead of erroring.
    ///
    /// # Arguments
    ///
    /// * `entity` - The entity to project
    pub fn from_entity(entity: Entity) -> Option<Self> {
        // only hunt entities can project into a status line
        let EntityMetadata::Hunt(hunt) = entity.metadata else {
            return None;
        };
        // reduce the frozen snapshot to a count and keep the bounded rest
        Some(HuntStatusLine {
            id: entity.id,
            name: entity.name,
            submitter: entity.submitter,
            groups: entity.groups,
            engine: hunt.engine,
            status: hunt.status,
            created: entity.created,
            frozen_rules: hunt.rules.len() as u64,
            lanes: hunt.lanes,
            counters: hunt.counters,
            failure: hunt.failure,
        })
    }
}

/// One user's hunt usage in the admin census
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct UserHuntCensus {
    /// The username this census row covers
    pub username: String,
    /// How many Created or Running hunts this user has right now
    pub active: u64,
    /// The per-user active quota (`thorium.hunts.max_active_per_user`)
    pub quota: u64,
    /// How many hunts this user created inside the recent window
    ///
    /// Saturates at the census window scan bound when a window holds an
    /// extraordinary number of created hunts.
    pub recent_created: u64,
}

impl UserHuntCensus {
    /// Create an empty census row for one user
    ///
    /// # Arguments
    ///
    /// * `username` - The username this census row covers
    /// * `quota` - The per-user active quota
    #[must_use]
    pub fn new(username: &str, quota: u64) -> Self {
        UserHuntCensus {
            username: username.to_owned(),
            active: 0,
            quota,
            recent_created: 0,
        }
    }
}

/// The configured quotas and current global usage
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct HuntQuotaView {
    /// The per-user active hunt quota
    pub max_active_per_user: u64,
    /// The global active hunt quota
    pub max_active_global: u64,
    /// How many hunts are active globally right now
    pub active_global: u64,
}

/// The admin hunt census (`GET /api/entities/hunt/status/`)
///
/// Computed at read time from entities, tags, and queue depths; nothing in it
/// is ever persisted. The whole census is cross-group, cross-user data so the
/// route is admin only.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct HuntSystemStatus {
    /// Hunts currently running
    pub ongoing: Vec<HuntStatusLine>,
    /// Hunts created but not yet running (queued, not scheduled)
    pub upcoming: Vec<HuntStatusLine>,
    /// Recently terminal hunts inside the window, newest first
    pub recent: Vec<HuntStatusLine>,
    /// Per-user usage against quotas
    pub users: Vec<UserHuntCensus>,
    /// Queue depths for the hunt chunk kinds (the paced backlog)
    ///
    /// Future-scored paced chunks count as queued: that is the pacing config
    /// working, not staleness.
    pub chunk_backlog: HashMap<EventType, KindDepth>,
    /// The quotas and global usage
    pub quotas: HuntQuotaView,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{EntityMetadata, EventType};
    use std::collections::HashSet;

    /// Build a yara rule entity carrying the given status tag values
    ///
    /// # Arguments
    ///
    /// * `statuses` - The `YaraRuleStatus` tag values to set on the entity
    fn rule_entity(statuses: &[&str]) -> Entity {
        // build the status tag values for this rule
        let mut tags = HashMap::new();
        if !statuses.is_empty() {
            // build the value map for the status tag key
            let mut values = HashMap::new();
            for status in statuses {
                // the group set a value is visible in doesn't matter here
                values.insert((*status).to_owned(), HashSet::new());
            }
            tags.insert(YaraRule::STATUS_TAG.to_owned(), values);
        }
        // build a bare entity carrying those tags
        Entity {
            id: Uuid::new_v4(),
            name: "test-rule".to_owned(),
            kind: EntityKinds::YaraRule,
            metadata: EntityMetadata::Other,
            description: None,
            submitter: "mcarson".to_owned(),
            groups: vec!["corn".to_owned()],
            tags,
            image: None,
            created: Utc::now(),
        }
    }

    /// The engine's wire format must stay pinned to lowercase "yara"
    ///
    /// The book documents `tags[HuntEngine][]=yara` as the browse query and
    /// `conf.thorium.hunts.yara` as the config block, so the engine's string
    /// form is a wire contract in both directions.
    #[test]
    fn engine_string_pinned_lowercase() {
        // the display form drives the HuntEngine tag value
        assert_eq!(HuntEngine::Yara.to_string(), "yara");
        // the serde form drives the create request wire format
        let raw = serde_json::to_string(&HuntEngine::Yara).expect("failed to serialize engine");
        assert_eq!(raw, "\"yara\"");
        // both parse directions must accept the lowercase form
        let parsed: HuntEngine = serde_json::from_str("\"yara\"").expect("failed to deserialize");
        assert_eq!(parsed, HuntEngine::Yara);
        let from_str: HuntEngine = "yara".parse().expect("failed to parse engine");
        assert_eq!(from_str, HuntEngine::Yara);
    }

    /// An engine without a shipped plug must fail to deserialize
    ///
    /// This is the serde layer half of the "create naming an engine with no
    /// shipped plug is a 400" exit criterion.
    #[test]
    fn unknown_engine_fails_to_deserialize() {
        // sigma has no shipped hunt engine so it must not parse
        assert!(serde_json::from_str::<HuntEngine>("\"sigma\"").is_err());
        // casing is part of the contract so the variant case must not parse
        assert!(serde_json::from_str::<HuntEngine>("\"Yara\"").is_err());
    }

    /// Every selector shape must keep its pinned wire format
    ///
    /// The UI and clients build these shapes by hand so a serde layout change
    /// is a breaking wire change.
    #[test]
    fn selector_serde_pinned_vectors() {
        // pin a uuid so the vectors are stable
        let id = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
        // pin the explicit rules selector shape
        let rules = serde_json::to_string(&HuntSelector::Rules(vec![id])).unwrap();
        assert_eq!(
            rules,
            r#"{"Rules":["11111111-2222-3333-4444-555555555555"]}"#
        );
        // pin the tag selector shape
        let tag = serde_json::to_string(&HuntSelector::Tag {
            key: "family".to_owned(),
            value: "emotet".to_owned(),
        })
        .unwrap();
        assert_eq!(tag, r#"{"Tag":{"key":"family","value":"emotet"}}"#);
        // pin the all selector shape
        let all = serde_json::to_string(&HuntSelector::All).unwrap();
        assert_eq!(all, r#""All""#);
        // pin the rule set selector shape
        let ruleset = serde_json::to_string(&HuntSelector::RuleSet(id)).unwrap();
        assert_eq!(
            ruleset,
            r#"{"RuleSet":"11111111-2222-3333-4444-555555555555"}"#
        );
        // every shape must round trip back to itself
        for raw in [rules, tag, all, ruleset] {
            let parsed: HuntSelector = serde_json::from_str(&raw).unwrap();
            assert_eq!(serde_json::to_string(&parsed).unwrap(), raw);
        }
    }

    /// Status display values must stay pinned since they are tag values
    #[test]
    fn status_tag_values_pinned() {
        // every status's display form is its HuntStatus tag value
        assert_eq!(HuntStatus::Created.to_string(), "Created");
        assert_eq!(HuntStatus::Running.to_string(), "Running");
        assert_eq!(HuntStatus::Completed.to_string(), "Completed");
        assert_eq!(HuntStatus::Cancelled.to_string(), "Cancelled");
        assert_eq!(HuntStatus::Failed.to_string(), "Failed");
        // only Created and Running count as active
        assert!(HuntStatus::Created.is_active());
        assert!(HuntStatus::Running.is_active());
        assert!(!HuntStatus::Completed.is_active());
        assert!(!HuntStatus::Cancelled.is_active());
        assert!(!HuntStatus::Failed.is_active());
    }

    /// The hunt tag keys must stay pinned since the quota census queries them
    #[test]
    fn hunt_tag_keys_pinned() {
        assert_eq!(Hunt::STATUS_TAG, "HuntStatus");
        assert_eq!(Hunt::ACTIVE_TAG, "HuntActive");
        assert_eq!(Hunt::ENGINE_TAG, "HuntEngine");
        assert_eq!(Hunt::ID_TAG, "HuntId");
        // the quarantine tag key and values must match the scanners
        assert_eq!(YaraRule::STATUS_TAG, "YaraRuleStatus");
        assert_eq!(YaraRule::STATUS_NOISY, "Noisy");
        assert_eq!(YaraRule::STATUS_SLOW, "Slow");
    }

    /// The yara snapshot filter must exclude Noisy and Slow but not Broken
    ///
    /// This mirrors the live context build's list time skip: broken sources
    /// drop out at the compile classify step so a broken tagged rule whose
    /// text was since fixed hunts exactly like it scans.
    #[test]
    fn snapshot_filter_noisy_slow_not_broken() {
        // noisy and slow quarantined rules must be excluded
        assert!(HuntEngine::Yara.excludes_from_snapshot(&rule_entity(&["Noisy"])));
        assert!(HuntEngine::Yara.excludes_from_snapshot(&rule_entity(&["Slow"])));
        // broken tagged rules are not excluded at snapshot
        assert!(!HuntEngine::Yara.excludes_from_snapshot(&rule_entity(&["Broken"])));
        // untagged rules are not excluded
        assert!(!HuntEngine::Yara.excludes_from_snapshot(&rule_entity(&[])));
        // a rule that is both broken and slow is still excluded
        assert!(HuntEngine::Yara.excludes_from_snapshot(&rule_entity(&["Broken", "Slow"])));
    }

    /// The yara snapshot filter must never exclude an exempt rule
    ///
    /// A stale status tag can survive a quarantine race (a worker tagging
    /// concurrently with the admin's exemption) so exemption must win over
    /// the tag or an exempt rule could be silently benched forever.
    #[test]
    fn snapshot_filter_exemption_wins_over_stale_tag() {
        // build a noisy tagged rule that carries an exemption
        let mut exempt = rule_entity(&["Noisy", "Slow"]);
        exempt.metadata = EntityMetadata::YaraRule(
            YaraRule::new(
                "rule a { condition: true }",
                crate::models::YaraRuleAppliesTo::Samples,
            )
            .expect("failed to build yara rule"),
        );
        if let EntityMetadata::YaraRule(rule) = &mut exempt.metadata {
            rule.quarantine_exempt = true;
        }
        // the exemption must win over the stale quarantine tag
        assert!(!HuntEngine::Yara.excludes_from_snapshot(&exempt));
        // the same rule without the exemption must stay excluded
        if let EntityMetadata::YaraRule(rule) = &mut exempt.metadata {
            rule.quarantine_exempt = false;
        }
        assert!(HuntEngine::Yara.excludes_from_snapshot(&exempt));
    }

    /// The yara engine's plug points must map to the yara kinds and queues
    #[test]
    fn yara_engine_plug_points() {
        // the yara engine's rules are yara rule entities
        assert_eq!(HuntEngine::Yara.rule_kind(), EntityKinds::YaraRule);
        // the yara engine's chunks land on the yara hunt chunk queue
        let chunk = HuntChunkEvent {
            hunt: Uuid::new_v4(),
            lane: 0,
            seq: 0,
        };
        let data = HuntEngine::Yara.chunk_event_data(chunk);
        assert_eq!(EventType::from(&data), EventType::YaraHuntChunk);
        // the standalone kind accessor must agree with the wrapped data
        assert_eq!(HuntEngine::Yara.chunk_event_type(), EventType::from(&data));
    }

    /// Omitted corpus bounds must default to the everything shape
    #[test]
    fn corpus_request_defaults() {
        // deserialize a corpus request with no bounds set
        let corpus: HuntCorpusRequest =
            serde_json::from_str(r#"{"Files":{}}"#).expect("failed to deserialize corpus");
        // unwrap the files bounds
        let HuntCorpusRequest::Files(bounds) = corpus else {
            panic!("corpus request deserialized to the wrong kind");
        };
        // every omitted bound must fall back to its default
        assert!(bounds.groups.is_empty());
        assert!(bounds.start.is_none());
        assert!(bounds.end.is_none());
        assert!(bounds.tags.is_empty());
        assert!(!bounds.tags_case_insensitive);
    }

    /// Hunt metadata must round trip through serde
    #[test]
    fn hunt_serde_round_trip() {
        // build a hunt with every field set to a distinctive value
        let hunt = Hunt {
            engine: HuntEngine::Yara,
            selector: HuntSelector::All,
            rules: vec![Uuid::new_v4(), Uuid::new_v4()],
            excluded_quarantined: 1,
            excluded_foreign: 2,
            corpus: HuntCorpus::Files(HuntCorpusBounds {
                groups: vec!["corn".to_owned()],
                start: Utc::now(),
                end: Utc::now(),
                tags: HashMap::default(),
                tags_case_insensitive: false,
            }),
            status: HuntStatus::Created,
            lanes: vec![HuntLane {
                start: Utc::now(),
                current: Utc::now(),
                end: Utc::now(),
                seq: 0,
                cursor: None,
                exhausted: false,
            }],
            counters: HuntCounters::default(),
            failure: Some("test failure".to_owned()),
        };
        // serialize and deserialize the hunt
        let raw = serde_json::to_string(&hunt).expect("failed to serialize hunt");
        let round: Hunt = serde_json::from_str(&raw).expect("failed to deserialize hunt");
        // the hunt must survive the round trip unchanged
        assert_eq!(hunt, round);
        // counters and failure must default when missing so rolling upgrades
        // keep reading hunts written by older replicas
        let mut value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        value.as_object_mut().unwrap().remove("counters");
        value.as_object_mut().unwrap().remove("failure");
        let defaulted: Hunt = serde_json::from_value(value).expect("counters must default");
        assert_eq!(defaulted.counters, HuntCounters::default());
        assert_eq!(defaulted.failure, None);
    }

    /// Counter absorption must saturate and keep error samples bounded
    #[test]
    fn counters_absorb_saturates_and_caps_samples() {
        // start with totals that are near their limits
        let mut totals = HuntCounters {
            listed: u64::MAX - 1,
            scanned: 10,
            matched: 1,
            written: 2,
            suppressed: 3,
            skipped: 4,
            missing: 5,
            errors: 6,
            error_samples: vec!["first".to_owned()],
        };
        // build a delta that would wrap the listed total
        let delta = HuntCounters {
            listed: 10,
            scanned: 1,
            matched: 1,
            written: 1,
            suppressed: 1,
            skipped: 1,
            missing: 1,
            errors: 1,
            error_samples: vec!["second".to_owned(), "third".to_owned(), "fourth".to_owned()],
        };
        // fold the delta in with a cap of 3 error samples
        totals.absorb(delta, 3);
        // the listed total must saturate instead of wrapping
        assert_eq!(totals.listed, u64::MAX);
        // every other counter must simply add
        assert_eq!(totals.scanned, 11);
        assert_eq!(totals.matched, 2);
        assert_eq!(totals.written, 3);
        assert_eq!(totals.suppressed, 4);
        assert_eq!(totals.skipped, 5);
        assert_eq!(totals.missing, 6);
        assert_eq!(totals.errors, 7);
        // only enough new samples to reach the cap may be kept
        assert_eq!(totals.error_samples, vec!["first", "second", "third"]);
        // a full sample list must absorb no further samples
        totals.absorb(
            HuntCounters {
                error_samples: vec!["fifth".to_owned()],
                ..HuntCounters::default()
            },
            3,
        );
        assert_eq!(totals.error_samples.len(), 3);
    }

    /// Checkpoint requests must round trip and default their optional fields
    #[test]
    fn checkpoint_serde_round_trip_and_defaults() {
        // build a checkpoint with every field set to a distinctive value
        let checkpoint = HuntCheckpoint {
            lane: 2,
            seq: 7,
            current: Utc::now(),
            cursor: Some(Uuid::new_v4()),
            exhausted: true,
            failed: Some("no usable rules".to_owned()),
            counters: HuntCounters {
                listed: 1,
                ..HuntCounters::default()
            },
        };
        // the checkpoint must survive a serde round trip unchanged
        let raw = serde_json::to_string(&checkpoint).expect("failed to serialize checkpoint");
        let round: HuntCheckpoint =
            serde_json::from_str(&raw).expect("failed to deserialize checkpoint");
        assert_eq!(checkpoint, round);
        // a minimal checkpoint must default every optional field
        let minimal: HuntCheckpoint =
            serde_json::from_str(r#"{"lane":0,"seq":0,"current":"2026-10-16T00:00:00Z"}"#)
                .expect("failed to deserialize a minimal checkpoint");
        assert_eq!(minimal.cursor, None);
        assert!(!minimal.exhausted);
        assert_eq!(minimal.failed, None);
        assert_eq!(minimal.counters, HuntCounters::default());
        // the response shape must round trip as well
        let resp = HuntCheckpointResponse {
            status: HuntStatus::Running,
            emitted: true,
        };
        let raw = serde_json::to_string(&resp).expect("failed to serialize response");
        let round: HuntCheckpointResponse =
            serde_json::from_str(&raw).expect("failed to deserialize response");
        assert_eq!(resp, round);
    }
}
