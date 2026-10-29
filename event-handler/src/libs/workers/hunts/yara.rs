//! Hunt through samples with yara rules
//!
//! The yara scanner pops scan events, coalesces them by sha256, downloads each
//! samples bytes straight into memory (never to disk), scans them once with the
//! global compiled ruleset, and writes partition correct hits through the
//! single audited [`writer::YaraHitWriter`] path.
//!
//! Error taxonomy invariant: `process` returns `Ok` for every per sample
//! failure (the events stay in flight and ride the retry pass); an `Err`
//! escaping this worker kills the whole daemon via the controllers
//! `handle.await??`, so it is reserved for unrecoverable states only.

pub mod budget;
pub mod context;
pub mod hunter;
pub mod subsets;
pub mod writer;

use chrono::prelude::*;
use futures::StreamExt;
use futures::stream;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use thorium::client::SampleBytes;
use thorium::conf::Yara;
use thorium::models::{
    EntityMetadata, Event, EventData, EventIds, EventType, MatchedRule, MatchedString,
    NotificationLevel, NotificationParams, NotificationRequest, TagRequest, YaraRule,
    YaraScanTrigger,
};
use thorium::{Error, Thorium};
use tracing::{Level, event, instrument};
use uuid::Uuid;

use crate::libs::EventWorkerCache;
use crate::libs::workers::{
    EventWorkerMutableSupport, EventWorkerSupportCore, WorkerCacheUpdateKinds, WorkerCacheUpdates,
};
pub use context::YaraRuleContext;

use budget::ScanBudget;
use context::STATUS_TAG;
use writer::{GroupAllowances, YaraHitWriter, skip_groups, write_groups};

/// The stats for a yara scanner worker
#[derive(Default, Debug, Clone, Eq, PartialEq)]
pub struct YaraScannerStats {
    /// The total number of samples scanned
    pub scanned: usize,
    /// The total number of rule matches observed
    pub matched: usize,
    /// The total number of hits written or merged
    pub hits_written: usize,
    /// The total number of hits suppressed by group policy
    pub hits_suppressed_by_policy: usize,
    /// The total number of hits dropped because their sample was deleted
    pub hits_dropped_missing: usize,
    /// The total number of samples skipped for exceeding the scan cap
    pub skipped_oversize: usize,
    /// The total number of samples skipped because they no longer exist
    pub skipped_missing: usize,
    /// The total number of revocation shrinks performed
    pub shrunk: usize,
    /// The total number of per sample errors left in flight for retry
    pub errors: usize,
}

impl std::ops::AddAssign for YaraScannerStats {
    fn add_assign(&mut self, rhs: Self) {
        self.scanned = self.scanned.saturating_add(rhs.scanned);
        self.matched = self.matched.saturating_add(rhs.matched);
        self.hits_written = self.hits_written.saturating_add(rhs.hits_written);
        self.hits_suppressed_by_policy = self
            .hits_suppressed_by_policy
            .saturating_add(rhs.hits_suppressed_by_policy);
        self.hits_dropped_missing = self
            .hits_dropped_missing
            .saturating_add(rhs.hits_dropped_missing);
        self.skipped_oversize = self.skipped_oversize.saturating_add(rhs.skipped_oversize);
        self.skipped_missing = self.skipped_missing.saturating_add(rhs.skipped_missing);
        self.shrunk = self.shrunk.saturating_add(rhs.shrunk);
        self.errors = self.errors.saturating_add(rhs.errors);
    }
}

/// A page of scan events coalesced down to one scan per sample
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoalescedScan {
    /// The sha256 of the sample to scan
    pub sha256: String,
    /// The union of groups the sample became newly visible too
    ///
    /// Only `NewSubmission` and `GroupsAdded` groups land here;
    /// `GroupsRemoved` groups must never enter the scan write set or hits
    /// would be written into just revoked groups.
    pub scan_groups: Vec<String>,
    /// Whether any coalesced event revoked groups from this sample
    pub removed: bool,
    /// The max originating trigger depth across the coalesced events
    pub depth: u8,
    /// The ids of every event this scan covers
    pub event_ids: Vec<Uuid>,
}

/// Coalesce a page of events into one scan per sha256
///
/// Returns the coalesced scans along with any event ids that carried the wrong
/// event data kind and should just be cleared.
///
/// # Arguments
///
/// * `events` - The popped events to coalesce
pub fn coalesce(events: Vec<Event>) -> (Vec<CoalescedScan>, Vec<Uuid>) {
    // track our coalesced scans by sha256
    let mut map: HashMap<String, CoalescedScan> = HashMap::default();
    // track any events this worker can never handle
    let mut stray = Vec::default();
    for event in events {
        // only yara scan events belong on this queue
        let EventData::YaraScannableSample(inner) = event.data else {
            // track this stray event so it gets cleared instead of leaking
            stray.push(event.id);
            continue;
        };
        // get or build the coalesced scan for this sample
        let scan = map
            .entry(inner.sha256.clone())
            .or_insert_with(|| CoalescedScan {
                sha256: inner.sha256,
                scan_groups: Vec::default(),
                removed: false,
                depth: 0,
                event_ids: Vec::default(),
            });
        // fold this event into the coalesced scan
        match inner.trigger {
            YaraScanTrigger::NewSubmission | YaraScanTrigger::GroupsAdded => {
                // union this events groups into the scan write set
                for group in inner.groups {
                    if !scan.scan_groups.contains(&group) {
                        scan.scan_groups.push(group);
                    }
                }
            }
            // revoked groups never enter the scan write set; the shrink
            // recomputes against the samples current groups instead
            YaraScanTrigger::GroupsRemoved => scan.removed = true,
        }
        // carry the deepest originating trigger depth
        scan.depth = scan.depth.max(event.depth);
        // this scan covers this event
        scan.event_ids.push(event.id);
    }
    (map.into_values().collect(), stray)
}

/// The matches one scan produced keyed by the rule entity that fired
pub(crate) type ScanMatches = HashMap<Uuid, Vec<MatchedRule>>;

/// The outcome of downloading and scanning one sample
pub(crate) enum ScanOutcome {
    /// The sample was scanned and these rules matched
    Scanned {
        /// The matches keyed by rule entity id
        matches: ScanMatches,
        /// The rules the profiler attributed a slow scan too
        slow: Vec<Uuid>,
    },
    /// The sample exceeded the scan cap and was deliberately skipped
    Oversize,
    /// The sample no longer exists so there is nothing to scan
    Missing,
    /// This sample failed and its events should stay in flight
    Failed(Error),
}

/// Download and scan one samples bytes entirely in memory
///
/// # Arguments
///
/// * `thorium` - A service account client for Thorium
/// * `context` - The compiled ruleset context to scan with
/// * `conf` - The yara settings for this Thorium instance
/// * `budget` - The byte budget bounding in flight scan buffers
/// * `sha256` - The sha256 of the sample to scan
pub(crate) async fn run_scan(
    thorium: Arc<Thorium>,
    context: YaraRuleContext,
    conf: Yara,
    budget: ScanBudget,
    sha256: String,
) -> ScanOutcome {
    // admit this download into the small class first; most samples are tiny
    let mut grant = match budget.admit_small().await {
        Ok(grant) => grant,
        Err(error) => return ScanOutcome::Failed(error),
    };
    // download this samples bytes straight into memory under our grants cap
    let mut bytes = match thorium.files.download_bytes(&sha256, grant.cap).await {
        Ok(bytes) => bytes,
        Err(error) => {
            // a missing sample is a deliberate skip, not a retryable failure
            if error.status() == Some(reqwest::StatusCode::NOT_FOUND) {
                return ScanOutcome::Missing;
            }
            return ScanOutcome::Failed(error);
        }
    };
    // a sample that outgrew its small grant upgrades to a large grant
    if matches!(bytes, SampleBytes::Oversize { .. }) && grant.cap < conf.max_scan_size {
        // release our small grant before queueing for a large one
        drop(grant);
        // queue for one of the bounded large grants
        grant = match budget.admit_large().await {
            Ok(grant) => grant,
            Err(error) => return ScanOutcome::Failed(error),
        };
        // re-download under the large grants cap
        bytes = match thorium.files.download_bytes(&sha256, grant.cap).await {
            Ok(bytes) => bytes,
            Err(error) => {
                // the sample may have been deleted between downloads
                if error.status() == Some(reqwest::StatusCode::NOT_FOUND) {
                    return ScanOutcome::Missing;
                }
                return ScanOutcome::Failed(error);
            }
        };
    }
    // anything still over its cap is a deliberate oversize skip
    let buffer = match bytes {
        SampleBytes::Complete(buffer) => buffer,
        SampleBytes::Oversize { .. } => return ScanOutcome::Oversize,
    };
    // scan on the blocking pool so cpu bound matching never starves the
    // runtime that also services trigger handling
    let rules = context.rules.clone();
    let timeout = Duration::from_secs(conf.scan_timeout_secs);
    let slow_ms = conf.slow_rule_ms;
    let scan = tokio::task::spawn_blocking(move || scan_buffer(&rules, &buffer, timeout, slow_ms));
    match scan.await {
        Ok(outcome) => outcome,
        Err(error) => ScanOutcome::Failed(Error::new(format!("scan task failed: {error}"))),
    }
}

/// Scan a buffer with the compiled ruleset and gather bounded match info
///
/// This runs on the blocking pool and owns its buffer so the closure is
/// `'static`; the ruleset rides in via its `Arc`.
///
/// # Arguments
///
/// * `rules` - The compiled ruleset to scan with
/// * `buffer` - The samples bytes to scan
/// * `timeout` - The max wall time this scan may take
/// * `slow_ms` - The scan time above which rules are profiled as slow
fn scan_buffer(
    rules: &yara_x::Rules,
    buffer: &[u8],
    timeout: Duration,
    slow_ms: u64,
) -> ScanOutcome {
    // build a scanner over our shared compiled ruleset
    let mut scanner = yara_x::Scanner::new(rules);
    // bound the wall time any single scan may take
    scanner.set_timeout(timeout);
    // track how long this scan takes so slow rules can be attributed
    let started = std::time::Instant::now();
    // scan this buffer
    let results = match scanner.scan(buffer) {
        Ok(results) => results,
        Err(error) => {
            // a timeout is still a per sample failure (retried) but we try to
            // attribute the slow rules so quarantine can act on strikes
            let slow = slowest_rules(&mut scanner);
            // log the slow rules this timeout was attributed too
            event!(
                Level::ERROR,
                msg = "Yara scan failed",
                error = error.to_string(),
                slow = slow.len(),
            );
            return ScanOutcome::Failed(Error::new(format!("yara scan failed: {error}")));
        }
    };
    // gather bounded match info for every rule that fired
    let mut matches: ScanMatches = HashMap::default();
    for rule in results.matching_rules() {
        // namespaces are rule entity ids by construction
        let Ok(rule_id) = Uuid::parse_str(rule.namespace()) else {
            // log that a namespace somehow was not an entity id
            event!(
                Level::ERROR,
                msg = "Yara namespace is not an entity id",
                namespace = rule.namespace(),
            );
            continue;
        };
        // gather this rules matched strings and their first offsets
        let mut strings = Vec::default();
        for pattern in rule.patterns() {
            // count this patterns matches and keep its first offsets
            let mut count = 0u64;
            let mut first_offsets = Vec::default();
            for found in pattern.matches() {
                count = count.saturating_add(1);
                if first_offsets.len() < 10 {
                    first_offsets.push(found.range().start as u64);
                }
            }
            // skip patterns that never matched
            if count == 0 {
                continue;
            }
            strings.push(MatchedString {
                id: pattern.identifier().to_owned(),
                count,
                first_offsets,
            });
        }
        // add this rules match info under its entity id
        matches.entry(rule_id).or_default().push(MatchedRule {
            rule_identifier: rule.identifier().to_owned(),
            strings,
        });
    }
    // profile slow rules when this scan ran long even without timing out
    let slow = if started.elapsed().as_millis() as u64 >= slow_ms {
        slowest_rules(&mut scanner)
    } else {
        Vec::default()
    };
    ScanOutcome::Scanned { matches, slow }
}

/// Attribute a slow scan to its slowest rules via the yara-x profiler
///
/// # Arguments
///
/// * `scanner` - The scanner that just ran a slow scan
fn slowest_rules(scanner: &mut yara_x::Scanner) -> Vec<Uuid> {
    // ask the profiler for the slowest rules in the last scan
    scanner
        .slowest_rules(5)
        .iter()
        .filter_map(|profile| Uuid::parse_str(profile.namespace).ok())
        .collect()
}

/// A single yara scanner worker
pub struct YaraScannerWorker {
    /// The compiled ruleset context for this worker
    context: YaraRuleContext,
    /// The per group allowability snapshots for partitioning writes
    allowances: HashMap<String, GroupAllowances>,
    /// A service account client for Thorium
    thorium: Arc<Thorium>,
    /// The identity of this event handler process that owns our popped events
    worker: String,
    /// The single audited writer for hits, tags, and skip markers
    writer: YaraHitWriter,
    /// The yara settings for this Thorium instance
    conf: Yara,
    /// The byte budget bounding in flight scan buffers
    budget: ScanBudget,
    /// The timestamp to retry failed events at if any failed
    retry_ts: Option<DateTime<Utc>>,
    /// The matched rule sets of the last window of scans for noisy detection
    window: VecDeque<HashSet<Uuid>>,
    /// The slow strikes accrued per rule
    slow_strikes: HashMap<Uuid, u32>,
    /// The rules this worker has quarantined and suppressed writes for
    suppressed: HashSet<Uuid>,
    /// The rules whose exemption this worker confirmed by re-fetch
    ///
    /// This only memoizes exemptions set after the current context was built
    /// (the context's own metadata covers the rest) so the rebuild debounce
    /// window costs at most one re-fetch per exempt rule.
    exempt_seen: HashSet<Uuid>,
    /// The broken rules this worker has already tagged and notified about
    notified_broken: HashSet<Uuid>,
}

impl YaraScannerWorker {
    /// Check whether a rule is exempt from quarantine in this workers view
    ///
    /// The context carries exemptions from build time while the memo carries
    /// ones confirmed by re-fetch since; checking both here keeps exempt
    /// rules out of the noisy/slow candidate sets entirely so a broad exempt
    /// rule never re-enters the quarantine path on every scan.
    ///
    /// # Arguments
    ///
    /// * `rule_id` - The id of the rule to check for an exemption
    fn is_exempt(&self, rule_id: &Uuid) -> bool {
        // check the build time flag first and the re-fetch memo second
        self.context
            .meta
            .get(rule_id)
            .is_some_and(|meta| meta.quarantine_exempt)
            || self.exempt_seen.contains(rule_id)
    }

    /// Quarantine a rule by tagging it, notifying its submitter, and
    /// suppressing its writes until the next context rebuild drops it
    ///
    /// Replicas each track their own noisy windows and slow strikes so a peer
    /// may already have quarantined this rule; the rule entity is re-fetched
    /// and the tag and notification are skipped if any status tag exists.
    ///
    /// # Arguments
    ///
    /// * `rule_id` - The id of the rule to quarantine
    /// * `status` - The status to quarantine this rule under (Noisy or Slow)
    /// * `reason` - The human readable reason for this quarantine
    #[instrument(name = "YaraScannerWorker::quarantine", skip(self, reason))]
    async fn quarantine(&mut self, rule_id: Uuid, status: &str, reason: String) {
        // only quarantine each rule once per context
        if !self.suppressed.insert(rule_id) {
            return;
        }
        // get this rules metadata if its still in our context
        let Some(meta) = self.context.meta.get(&rule_id) else {
            return;
        };
        // both callers pre-filter exempt rules with `is_exempt` before any
        // quarantine call and nothing can flip an exemption between that check
        // and this one (both run on this workers single task), so an exempt
        // rule here is a caller bug; exemptions set after the context was
        // built are still honored by the re-fetch below
        debug_assert!(
            !(meta.quarantine_exempt || self.exempt_seen.contains(&rule_id)),
            "quarantine() called for an exempt yara rule"
        );
        // log that we are quarantining this rule
        event!(
            Level::ERROR,
            msg = "Quarantining yara rule",
            rule = meta.name,
            id = rule_id.to_string(),
            status = status,
            reason = reason,
        );
        // re-fetch this rule so an already tagged rule is not re-announced
        match self.thorium.entities.get(rule_id).await {
            Ok(entity) => {
                // an exemption set after our context was built wins here, even
                // over a racing peer's tag; the admin route cleared any stale
                // tags when the exemption was set
                if let EntityMetadata::YaraRule(rule) = &entity.metadata
                    && rule.quarantine_exempt
                {
                    // remember this exemption so we only re-fetch once per context
                    self.exempt_seen.insert(rule_id);
                    // drop the suppression we just inserted so hit writes keep flowing
                    self.suppressed.remove(&rule_id);
                    // log that this rule is exempt from quarantine
                    event!(
                        Level::INFO,
                        msg = "Skipping quarantine for exempt yara rule",
                        id = rule_id.to_string(),
                        status = status,
                    );
                    return;
                }
                // a peer already tagged this rule so our local suppression is enough
                if context::has_status_tag(&entity.tags, None) {
                    // log that this rule already carries a status tag
                    event!(
                        Level::INFO,
                        msg = "Rule already carries a status tag; skipping tag and notification",
                        id = rule_id.to_string(),
                    );
                    return;
                }
            }
            Err(error) => {
                // fail open to tagging; duplicate tags converge while a missed
                // quarantine would leave the rule untagged everywhere
                event!(
                    Level::WARN,
                    msg = "Failed to re-fetch rule before quarantine",
                    id = rule_id.to_string(),
                    error = error.to_string(),
                );
            }
        }
        // tag this rule so the next context rebuild drops it entirely
        let tag_req = TagRequest::<thorium::models::Entity>::default()
            .groups(meta.groups.clone())
            .add(STATUS_TAG, status);
        if let Err(error) = self.thorium.entities.tag(rule_id, &tag_req).await {
            // log that we failed to tag this rule
            event!(
                Level::ERROR,
                msg = "Failed to tag quarantined rule",
                id = rule_id.to_string(),
                error = error.to_string(),
            );
        }
        // notify this rules submitter about the quarantine
        let req = NotificationRequest::new(reason, NotificationLevel::Error);
        if let Err(error) = self
            .thorium
            .entities
            .create_notification(rule_id, &req, &NotificationParams::default())
            .await
        {
            // log that we failed to notify about this quarantine
            event!(
                Level::ERROR,
                msg = "Failed to create quarantine notification",
                id = rule_id.to_string(),
                error = error.to_string(),
            );
        }
    }

    /// Update the noisy window with a scans matches and quarantine offenders
    ///
    /// # Arguments
    ///
    /// * `matches` - The rule ids that matched the last scanned sample
    async fn update_noisy_window(&mut self, matched: HashSet<Uuid>) {
        // add this scans matched set to the sliding window
        self.window.push_back(matched);
        // bound the window to its configured size
        while self.window.len() > self.conf.noisy_window {
            self.window.pop_front();
        }
        // only evaluate rates once the window is full
        if self.window.len() < self.conf.noisy_window {
            return;
        }
        // count how often each rule matched across the window
        let mut counts: HashMap<Uuid, usize> = HashMap::default();
        for matched in &self.window {
            for rule_id in matched {
                *counts.entry(*rule_id).or_default() += 1;
            }
        }
        // quarantine any rule whose match rate crossed the noisy threshold,
        // skipping already suppressed rules and exempt rules up front
        let window_len = self.window.len() as f64;
        let noisy = counts
            .into_iter()
            .filter(|(rule_id, count)| {
                (*count as f64 / window_len) > self.conf.noisy_threshold
                    && !self.suppressed.contains(rule_id)
                    && !self.is_exempt(rule_id)
            })
            .collect::<Vec<(Uuid, usize)>>();
        for (rule_id, count) in noisy {
            // build the reason for this quarantine
            let reason = format!(
                "Rule quarantined as noisy: it matched {count} of the last {} samples",
                self.window.len()
            );
            // quarantine this noisy rule
            self.quarantine(rule_id, YaraRule::STATUS_NOISY, reason)
                .await;
        }
    }

    /// Accrue slow strikes for profiled rules and quarantine repeat offenders
    ///
    /// # Arguments
    ///
    /// * `slow` - The rules the profiler attributed a slow scan too
    async fn update_slow_strikes(&mut self, slow: Vec<Uuid>) {
        for rule_id in slow {
            // exempt rules never accrue strikes or quarantine
            if self.is_exempt(&rule_id) {
                continue;
            }
            // accrue a strike for this rule
            let strikes = self.slow_strikes.entry(rule_id).or_default();
            *strikes += 1;
            // quarantine rules that keep showing up in slow scans
            if *strikes >= self.conf.slow_strikes && !self.suppressed.contains(&rule_id) {
                // build the reason for this quarantine
                let reason =
                    format!("Rule quarantined as slow: it was profiled in {strikes} slow scans",);
                // quarantine this slow rule
                self.quarantine(rule_id, YaraRule::STATUS_SLOW, reason)
                    .await;
            }
        }
    }

    /// Write everything one scanned sample produced
    ///
    /// # Arguments
    ///
    /// * `scan` - The coalesced scan that was performed
    /// * `matches` - The matches this scan produced keyed by rule id
    /// * `stats` - The stats to track against
    async fn write_scan(
        &mut self,
        scan: &CoalescedScan,
        matches: ScanMatches,
        stats: &mut YaraScannerStats,
    ) -> Result<(), Error> {
        // track which rules matched for the noisy window
        let matched_set = matches.keys().copied().collect::<HashSet<Uuid>>();
        stats.matched = stats.matched.saturating_add(matched_set.len());
        // write each matching rules hit into its partitioned groups
        for (rule_id, matched) in matches {
            // suppressed rules keep counting but never write
            if self.suppressed.contains(&rule_id) {
                continue;
            }
            // a rule missing from the meta map cannot be written safely
            let Some(meta) = self.context.meta.get(&rule_id) else {
                continue;
            };
            // pre filter with our cached view so the dominant cross group case
            // never costs an api call; the server recomputes authoritatively
            // and this cached copy can only under write, never leak
            if write_groups(&meta.groups, &scan.scan_groups, &self.allowances).is_empty() {
                stats.hits_suppressed_by_policy = stats.hits_suppressed_by_policy.saturating_add(1);
                continue;
            }
            // write this hit through the single audited server side route with
            // the raw event groups so a fresher server view still writes right;
            // live scans run for no hunt so their hits carry no decoration
            let resp = self
                .writer
                .write_hit(
                    rule_id,
                    meta,
                    &scan.sha256,
                    matched,
                    &self.context.ruleset_hash,
                    scan.scan_groups.clone(),
                    scan.depth,
                    None,
                )
                .await?;
            // map the servers per cause split onto our stats
            if resp.sample_missing {
                // the sample was deleted mid flight so this hit converged away
                event!(
                    Level::WARN,
                    msg = "Dropped yara hit for a deleted sample",
                    sha256 = &scan.sha256,
                    rule = rule_id.to_string(),
                );
                stats.hits_dropped_missing = stats.hits_dropped_missing.saturating_add(1);
            } else if resp.written.is_empty() {
                // the authoritative server view suppressed what our cached
                // view allowed; surface the lag so cache incidents are visible
                event!(
                    Level::WARN,
                    msg = "Server suppressed a hit our cached view allowed",
                    sha256 = &scan.sha256,
                    rule = rule_id.to_string(),
                    suppressed_rule = resp.suppressed_rule.len(),
                    suppressed_sample = resp.suppressed_sample.len(),
                    suppressed_policy = resp.suppressed_policy.len(),
                );
                stats.hits_suppressed_by_policy = stats.hits_suppressed_by_policy.saturating_add(1);
            } else {
                stats.hits_written = stats.hits_written.saturating_add(1);
            }
        }
        // update the noisy window with this scans matches
        self.update_noisy_window(matched_set).await;
        Ok(())
    }

    /// Tag and notify any rules the context build quarantined as broken
    ///
    /// Create and update validate rules with the real compiler so a stored
    /// rule only goes broken across engine upgrades; each broken rule is
    /// tagged and notified once per process so rebuilds never spam, and rules
    /// already carrying a broken tag (announced by a peer or a prior run) are
    /// never re-tagged or re-notified.
    async fn notify_broken(&mut self) {
        // snapshot the quarantined rules we have not handled yet
        let fresh = self
            .context
            .quarantined
            .iter()
            .filter(|rule| !self.notified_broken.contains(&rule.id))
            .cloned()
            .collect::<Vec<context::QuarantinedRule>>();
        for rule in fresh {
            // only tag and notify each broken rule once per process
            self.notified_broken.insert(rule.id);
            // log that this stored rule no longer compiles
            event!(
                Level::ERROR,
                msg = "Yara rule failed to compile and was quarantined",
                rule = rule.name,
                id = rule.id.to_string(),
                submitter = rule.submitter,
                error = rule.error,
                already_tagged = rule.already_tagged,
            );
            // a rule that already carries a broken tag was announced by a peer
            // or a prior run so never re-tag or re-notify it
            if rule.already_tagged {
                continue;
            }
            // tag this rule as broken so operators and owners can find it
            let tag_req = TagRequest::<thorium::models::Entity>::default()
                .groups(rule.groups.clone())
                .add(STATUS_TAG, YaraRule::STATUS_BROKEN);
            if let Err(error) = self.thorium.entities.tag(rule.id, &tag_req).await {
                // log that we failed to tag this broken rule
                event!(
                    Level::ERROR,
                    msg = "Failed to tag broken rule",
                    id = rule.id.to_string(),
                    error = error.to_string(),
                );
            }
            // notify on this rule about the compile failure
            let msg = format!("This yara rule no longer compiles: {}", rule.error);
            let req = NotificationRequest::new(msg, NotificationLevel::Error);
            if let Err(error) = self
                .thorium
                .entities
                .create_notification(rule.id, &req, &NotificationParams::default())
                .await
            {
                // log that we failed to notify about this broken rule
                event!(
                    Level::ERROR,
                    msg = "Failed to create broken rule notification",
                    id = rule.id.to_string(),
                    error = error.to_string(),
                );
            }
        }
    }

    /// Build the rule groups map the shrink path recomputes against
    fn rule_groups(&self) -> HashMap<Uuid, Vec<String>> {
        // snapshot each rules groups from our context
        self.context
            .meta
            .iter()
            .map(|(id, meta)| (*id, meta.groups.clone()))
            .collect()
    }
}

impl EventWorkerSupportCore for YaraScannerWorker {
    /// The stats for all workers of this kind
    type Stats = YaraScannerStats;

    /// Build this specific kind of worker
    ///
    /// # Arguments
    ///
    /// * `cache` - A cache of all info a worker may need to be created
    /// * `thorium` - A Thorium client
    /// * `worker` - The identity of this event handler process
    async fn new(cache: &EventWorkerCache, thorium: &Arc<Thorium>, worker: &str) -> Self {
        // snapshot the yara settings for this Thorium instance
        let conf = cache.conf.thorium.yara.clone();
        // build the byte budget bounding in flight scan buffers; config load
        // validates budget >= max scan size so a failure here means the conf
        // was built by hand and the safest fallback is one large grant
        let budget = ScanBudget::new(
            conf.scan_buffer_budget,
            conf.small_class_size,
            conf.max_scan_size,
        )
        .unwrap_or_else(|error| {
            // log that we are falling back to a minimal budget
            event!(
                Level::ERROR,
                msg = "Falling back to a minimal scan budget",
                error = error.to_string(),
            );
            // a budget of exactly one large grant always builds
            ScanBudget::new(
                conf.max_scan_size,
                conf.small_class_size,
                conf.max_scan_size,
            )
            .expect("a budget of one large grant must always build")
        });
        // build this worker
        YaraScannerWorker {
            context: cache.yara.clone(),
            allowances: cache.allowances.clone(),
            thorium: thorium.clone(),
            worker: worker.to_owned(),
            writer: YaraHitWriter::new(thorium.clone()),
            conf,
            budget,
            retry_ts: None,
            window: VecDeque::default(),
            slow_strikes: HashMap::default(),
            suppressed: HashSet::default(),
            exempt_seen: HashSet::default(),
            notified_broken: HashSet::default(),
        }
    }

    /// The types of events this worker handles
    #[inline]
    fn event_kinds() -> Vec<EventType> {
        vec![EventType::YaraScannableSample]
    }

    /// The types of cache updates this worker subscribes too
    #[inline]
    fn cache_subscriptions() -> Vec<WorkerCacheUpdateKinds> {
        vec![
            WorkerCacheUpdateKinds::YaraRulesContext,
            WorkerCacheUpdateKinds::GroupAllowances,
        ]
    }

    /// Apply an update to our local worker cache
    ///
    /// # Arguments
    ///
    /// * `update` - The update to apply to this workers cache
    fn apply_cache_update(&mut self, update: WorkerCacheUpdates) {
        match update {
            WorkerCacheUpdates::YaraRulesContext(context) => {
                // swap in the new compiled context
                self.context = context;
                // quarantine state is rebuilt against the new context
                self.window.clear();
                self.slow_strikes.clear();
                self.suppressed.clear();
                // the new context carries current exemptions so the memo of
                // re-fetched ones is stale now
                self.exempt_seen.clear();
            }
            WorkerCacheUpdates::GroupAllowances(allowances) => self.allowances = allowances,
            // this worker does not subscribe to any other updates
            _ => (),
        }
    }

    /// Report our latest stats in an event
    ///
    /// # Arguments
    ///
    /// * `stats` - The new stats to log
    /// * `prior` - The previously reported stats
    #[instrument(name = "EventWorkerSupportCore::stats", skip_all)]
    fn stats(stats: &Self::Stats, prior: &Self::Stats) {
        // only report stats when they changed
        if stats == prior {
            return;
        }
        // report this workers totals
        event!(
            Level::INFO,
            worker = "YaraScannerWorker",
            scanned = stats.scanned,
            matched = stats.matched,
            hits_written = stats.hits_written,
            hits_suppressed_by_policy = stats.hits_suppressed_by_policy,
            hits_dropped_missing = stats.hits_dropped_missing,
            skipped_oversize = stats.skipped_oversize,
            skipped_missing = stats.skipped_missing,
            shrunk = stats.shrunk,
            errors = stats.errors,
        );
    }

    /// Flatten this worker kinds stats into cumulative counters for publishing
    ///
    /// # Arguments
    ///
    /// * `stats` - The stats to flatten
    fn counter_map(stats: &Self::Stats) -> BTreeMap<String, u64> {
        // key each counter by its stats field name
        BTreeMap::from([
            ("scanned".to_owned(), stats.scanned as u64),
            ("matched".to_owned(), stats.matched as u64),
            ("hits_written".to_owned(), stats.hits_written as u64),
            (
                "hits_suppressed_by_policy".to_owned(),
                stats.hits_suppressed_by_policy as u64,
            ),
            (
                "hits_dropped_missing".to_owned(),
                stats.hits_dropped_missing as u64,
            ),
            ("skipped_oversize".to_owned(), stats.skipped_oversize as u64),
            ("skipped_missing".to_owned(), stats.skipped_missing as u64),
            ("shrunk".to_owned(), stats.shrunk as u64),
            ("errors".to_owned(), stats.errors as u64),
        ])
    }
}

impl EventWorkerMutableSupport for YaraScannerWorker {
    /// Process a page of yara scan events
    ///
    /// # Arguments
    ///
    /// * `events` - The events to process
    /// * `stats` - The stats to track/update
    #[instrument(name = "YaraScannerWorker::process", skip_all, fields(events = events.len()))]
    async fn process(
        &mut self,
        events: Vec<Event>,
        stats: &mut YaraScannerStats,
    ) -> Result<(), Error> {
        // tag and notify any freshly quarantined broken rules first
        self.notify_broken().await;
        // coalesce this page down to one scan per sample
        let (scans, stray) = coalesce(events);
        // clear any stray events so they never leak in flight; a failed clear
        // is retryable and must never kill the daemon
        if !stray.is_empty() {
            // clear the stray events we popped from our own in flight keys
            if let Err(error) = self
                .thorium
                .events
                .clear(
                    EventType::YaraScannableSample,
                    &EventIds::from(stray),
                    Some(&self.worker),
                )
                .await
            {
                // log that we failed to clear stray events
                event!(
                    Level::ERROR,
                    msg = "Failed to clear stray events",
                    error = error.to_string(),
                );
            }
        }
        // download and scan every sample concurrently; scans with no newly
        // visible groups (pure revocations) skip the scan phase entirely
        let scan_parallelism = self
            .conf
            .scan_parallelism
            .or_else(|| {
                std::thread::available_parallelism()
                    .ok()
                    .map(std::num::NonZeroUsize::get)
            })
            .unwrap_or(4);
        let outcomes = stream::iter(scans)
            .map(|scan| {
                // clone the shared handles this scan needs
                let thorium = self.thorium.clone();
                let context = self.context.clone();
                let conf = self.conf.clone();
                let budget = self.budget.clone();
                async move {
                    // pure revocations have nothing to download or scan
                    if scan.scan_groups.is_empty() {
                        return (scan, None);
                    }
                    // download and scan this sample
                    let outcome =
                        run_scan(thorium, context, conf, budget, scan.sha256.clone()).await;
                    (scan, Some(outcome))
                }
            })
            .buffer_unordered(scan_parallelism.max(1))
            .collect::<Vec<(CoalescedScan, Option<ScanOutcome>)>>()
            .await;
        // write each scans outcome and track which events we can clear
        let mut cleared = Vec::default();
        let mut any_failed = false;
        for (scan, outcome) in outcomes {
            // handle this scans outcome
            let ok = match outcome {
                // a pure revocation just shrinks below
                None => true,
                Some(ScanOutcome::Scanned { matches, slow }) => {
                    stats.scanned = stats.scanned.saturating_add(1);
                    // accrue slow strikes before writing
                    self.update_slow_strikes(slow).await;
                    // write this scans hits; per sample write
                    // failures leave the events in flight for retry
                    match self.write_scan(&scan, matches, stats).await {
                        Ok(()) => true,
                        Err(error) => {
                            // log that this samples writes failed
                            event!(
                                Level::ERROR,
                                msg = "Failed to write scan results",
                                sha256 = scan.sha256,
                                error = error.to_string(),
                            );
                            false
                        }
                    }
                }
                Some(ScanOutcome::Oversize) => {
                    stats.skipped_oversize = stats.skipped_oversize.saturating_add(1);
                    // record this deliberate skip so it never looks repairable
                    let groups = skip_groups(&scan.scan_groups, &self.allowances);
                    let reason = format!("oversize:{}", self.conf.max_scan_size);
                    if let Err(error) = self
                        .writer
                        .write_skip_marker(&scan.sha256, reason, groups, scan.depth)
                        .await
                    {
                        // log that we failed to record this skip
                        event!(
                            Level::ERROR,
                            msg = "Failed to write skip marker",
                            sha256 = scan.sha256,
                            error = error.to_string(),
                        );
                    }
                    true
                }
                Some(ScanOutcome::Missing) => {
                    stats.skipped_missing = stats.skipped_missing.saturating_add(1);
                    true
                }
                Some(ScanOutcome::Failed(error)) => {
                    stats.errors = stats.errors.saturating_add(1);
                    // log that this sample failed and will be retried
                    event!(
                        Level::ERROR,
                        msg = "Failed to scan sample",
                        sha256 = scan.sha256,
                        error = error.to_string(),
                    );
                    false
                }
            };
            // run the revocation shrink after any scan writes for this sample
            if ok && scan.removed {
                // shrink this samples hits against its current groups
                match self.writer.shrink(&scan.sha256, &self.rule_groups()).await {
                    Ok(()) => stats.shrunk = stats.shrunk.saturating_add(1),
                    Err(error) => {
                        // log that this shrink failed; its events stay in
                        // flight so the shrink is retried
                        event!(
                            Level::ERROR,
                            msg = "Failed to shrink revoked hits",
                            sha256 = scan.sha256,
                            error = error.to_string(),
                        );
                        any_failed = true;
                        continue;
                    }
                }
            }
            // clear this scans events on success; failures stay in flight
            if ok {
                cleared.extend(scan.event_ids);
            } else {
                any_failed = true;
            }
        }
        // clear every event our successful scans covered; a failed clear just
        // leaves them in flight for the retry pass (replay is idempotent since
        // hit identity is deterministic) and must never kill the daemon
        if !cleared.is_empty()
            && let Err(error) = self
                .thorium
                .events
                .clear(
                    EventType::YaraScannableSample,
                    &EventIds::from(cleared),
                    Some(&self.worker),
                )
                .await
        {
            // log that we failed to clear processed events
            event!(
                Level::ERROR,
                msg = "Failed to clear processed events",
                error = error.to_string(),
            );
            // arm the retry timer so the replay happens promptly
            any_failed = true;
        }
        // arm the retry timer if any sample failed
        if any_failed && self.retry_ts.is_none() {
            // retry failed events a few minutes from now
            self.retry_ts = Some(Utc::now() + chrono::Duration::minutes(3));
        }
        Ok(())
    }

    /// Take the time to retry any events this worker left in flight if one is set
    async fn take_retry_ts(&mut self) -> Option<DateTime<Utc>> {
        self.retry_ts.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a yara scan event for coalescing tests
    ///
    /// # Arguments
    ///
    /// * `sha256` - The sha256 this event is for
    /// * `groups` - The groups this event carries
    /// * `trigger` - The trigger kind for this event
    /// * `depth` - The trigger depth for this event
    fn scan_event(sha256: &str, groups: Vec<&str>, trigger: YaraScanTrigger, depth: u8) -> Event {
        Event {
            id: Uuid::new_v4(),
            timestamp: Utc::now(),
            parent: None,
            user: "mcarson".to_owned(),
            data: EventData::YaraScannableSample(thorium::models::YaraSampleScanEvent {
                sha256: sha256.to_owned(),
                groups: groups.into_iter().map(ToOwned::to_owned).collect(),
                trigger,
            }),
            depth,
        }
    }

    /// Coalescing must union only newly visible groups into the scan set
    #[test]
    fn coalesce_unions_added_groups_only() {
        // build events that add and revoke groups for one sample
        let events = vec![
            scan_event("aaa", vec!["a"], YaraScanTrigger::NewSubmission, 0),
            scan_event("aaa", vec!["b"], YaraScanTrigger::GroupsAdded, 0),
            scan_event("aaa", vec!["c"], YaraScanTrigger::GroupsRemoved, 0),
        ];
        // coalesce this page
        let (scans, stray) = coalesce(events);
        assert!(stray.is_empty());
        assert_eq!(scans.len(), 1);
        // the revoked group must never enter the scan write set
        let scan = &scans[0];
        assert!(scan.scan_groups.contains(&"a".to_owned()));
        assert!(scan.scan_groups.contains(&"b".to_owned()));
        assert!(!scan.scan_groups.contains(&"c".to_owned()));
        // the revocation must still be carried for the shrink
        assert!(scan.removed);
        // every event must be covered by this scan
        assert_eq!(scan.event_ids.len(), 3);
    }

    /// Coalescing must carry the deepest trigger depth of its events
    #[test]
    fn coalesce_takes_max_depth() {
        // build events at different depths for one sample
        let events = vec![
            scan_event("aaa", vec!["a"], YaraScanTrigger::NewSubmission, 1),
            scan_event("aaa", vec!["b"], YaraScanTrigger::GroupsAdded, 4),
        ];
        // coalesce this page
        let (scans, _) = coalesce(events);
        // the deepest depth must win
        assert_eq!(scans[0].depth, 4);
    }

    /// Coalescing must keep distinct samples separate
    #[test]
    fn coalesce_single_clear_set_per_sample() {
        // build events for two different samples
        let events = vec![
            scan_event("aaa", vec!["a"], YaraScanTrigger::NewSubmission, 0),
            scan_event("bbb", vec!["a"], YaraScanTrigger::NewSubmission, 0),
        ];
        // coalesce this page
        let (scans, _) = coalesce(events);
        // each sample must get its own scan with its own clear set
        assert_eq!(scans.len(), 2);
        assert!(scans.iter().all(|scan| scan.event_ids.len() == 1));
    }

    /// Stray event kinds must be routed to the clear list, never scanned
    #[test]
    fn coalesce_routes_stray_events_to_clears() {
        // build one yara event and one stray trigger event
        let yara = scan_event("aaa", vec!["a"], YaraScanTrigger::NewSubmission, 0);
        let stray = Event {
            id: Uuid::new_v4(),
            timestamp: Utc::now(),
            parent: None,
            user: "mcarson".to_owned(),
            data: EventData::NewSample {
                groups: vec!["a".to_owned()],
                sample: "aaa".to_owned(),
            },
            depth: 0,
        };
        let stray_id = stray.id;
        // coalesce this page
        let (scans, strays) = coalesce(vec![yara, stray]);
        // the stray event must be cleared, not scanned
        assert_eq!(scans.len(), 1);
        assert_eq!(strays, vec![stray_id]);
    }

    /// The counter map keys every [`YaraScannerStats`] field by its name so the
    /// published counters match the tracing gauges field for field.
    #[test]
    fn scanner_stats_counter_map() {
        // build stats with a distinct value per field
        let stats = YaraScannerStats {
            scanned: 1,
            matched: 2,
            hits_written: 3,
            hits_suppressed_by_policy: 4,
            hits_dropped_missing: 5,
            skipped_oversize: 6,
            skipped_missing: 7,
            shrunk: 8,
            errors: 9,
        };
        // flatten these stats into a counter map
        let map = YaraScannerWorker::counter_map(&stats);
        // every field must appear under its own name with its value
        let expected = [
            ("scanned", 1),
            ("matched", 2),
            ("hits_written", 3),
            ("hits_suppressed_by_policy", 4),
            ("hits_dropped_missing", 5),
            ("skipped_oversize", 6),
            ("skipped_missing", 7),
            ("shrunk", 8),
            ("errors", 9),
        ];
        // check each expected counter is present with its value
        for (name, value) in expected {
            assert_eq!(
                map.get(name),
                Some(&value),
                "{name} must map to its field name"
            );
        }
        // no extra counters may be published
        assert_eq!(
            map.len(),
            expected.len(),
            "the counter map must hold exactly one entry per field"
        );
    }
}
