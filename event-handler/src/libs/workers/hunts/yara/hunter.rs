//! Walk hunts over historical samples one paced chunk at a time
//!
//! The yara hunt worker consumes `YaraHuntChunk` events: each event advances
//! one lane of one hunt by a single bounded slice of the corpus listing. The
//! worker loads the hunt, drops stale or terminal chains, compiles the hunt's
//! frozen rule subset through a per worker LRU, scans the slice under a hunt
//! dedicated byte budget, writes hits through the audited server side route,
//! and commits through the checkpoint route whose compare-and-set emits the
//! next chunk - chains collapse on duplication, they never fork.
//!
//! Error taxonomy invariant (mirroring the scan worker): per item failures
//! count and continue, per chunk failures leave the event in flight for the
//! retry pass, and an `Err` escaping `process` kills the whole daemon via the
//! controllers `handle.await??`, so it is reserved for unrecoverable states.

use chrono::prelude::*;
use futures::StreamExt;
use futures::stream;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use thorium::conf::{Hunts, Yara};
use thorium::models::{
    EntityMetadata, Event, EventData, EventIds, EventType, FileListOpts, Hunt, HuntCheckpoint,
    HuntChunkEvent, HuntCounters, SampleListLine,
};
use thorium::{Error, Thorium};
use tracing::{Level, event, instrument};
use uuid::Uuid;

use super::budget::ScanBudget;
use super::subsets::{SubsetLru, build_subset};
use super::writer::YaraHitWriter;
use super::{ScanOutcome, run_scan};
use crate::libs::EventWorkerCache;
use crate::libs::workers::{
    EventWorkerMutableSupport, EventWorkerSupportCore, WorkerCacheUpdateKinds, WorkerCacheUpdates,
};

/// The stats for a yara hunt worker
#[derive(Default, Debug, Clone, Eq, PartialEq)]
pub struct YaraHuntStats {
    /// The total number of chunks processed to a checkpoint attempt
    pub chunks: usize,
    /// The total number of corpus list lines walked
    pub listed: usize,
    /// The total number of samples scanned
    pub scanned: usize,
    /// The total number of samples that matched at least one rule
    pub matched: usize,
    /// The total number of hits the server reported written or merged
    pub hits_written: usize,
    /// The total number of hit writes the server suppressed whole
    pub hits_suppressed: usize,
    /// The total number of written hits whose hunt vanished before decoration
    pub hits_undecorated: usize,
    /// The total number of samples skipped for exceeding the scan cap
    pub skipped_oversize: usize,
    /// The total number of samples that no longer existed
    pub skipped_missing: usize,
    /// The total number of per item errors recorded
    pub errors: usize,
    /// The total number of checkpoints that won their compare-and-set
    pub checkpoints_won: usize,
    /// The total number of checkpoints that lost their compare-and-set
    pub checkpoints_lost: usize,
    /// The total number of chains dropped for stale or terminal hunts
    pub chains_dropped: usize,
}

impl std::ops::AddAssign for YaraHuntStats {
    fn add_assign(&mut self, rhs: Self) {
        self.chunks = self.chunks.saturating_add(rhs.chunks);
        self.listed = self.listed.saturating_add(rhs.listed);
        self.scanned = self.scanned.saturating_add(rhs.scanned);
        self.matched = self.matched.saturating_add(rhs.matched);
        self.hits_written = self.hits_written.saturating_add(rhs.hits_written);
        self.hits_suppressed = self.hits_suppressed.saturating_add(rhs.hits_suppressed);
        self.hits_undecorated = self.hits_undecorated.saturating_add(rhs.hits_undecorated);
        self.skipped_oversize = self.skipped_oversize.saturating_add(rhs.skipped_oversize);
        self.skipped_missing = self.skipped_missing.saturating_add(rhs.skipped_missing);
        self.errors = self.errors.saturating_add(rhs.errors);
        self.checkpoints_won = self.checkpoints_won.saturating_add(rhs.checkpoints_won);
        self.checkpoints_lost = self.checkpoints_lost.saturating_add(rhs.checkpoints_lost);
        self.chains_dropped = self.chains_dropped.saturating_add(rhs.chains_dropped);
    }
}

/// What to do with a chunk event after processing it
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkOutcome {
    /// This chunk was handled or deliberately dropped so clear its event
    Clear,
    /// This chunk hit a transient failure so leave its event in flight
    Retry,
}

/// Why a popped chunk should be dropped without scanning anything
///
/// # Arguments
///
/// * `hunt` - The hunt this chunk advances
/// * `chunk` - The chunk event being checked
fn chunk_drop_reason(hunt: &Hunt, chunk: &HuntChunkEvent) -> Option<&'static str> {
    // terminal hunts observe cancellation and completion lazily here
    if !hunt.status.is_active() {
        return Some("hunt is terminal");
    }
    // a lane index past the hunts lanes can never commit
    let Some(lane) = hunt.lanes.get(chunk.lane as usize) else {
        return Some("lane does not exist");
    };
    // a sequence behind the lanes persisted floor already committed; dropping
    // it here collapses duplicate deliveries without a wasted scan
    if chunk.seq < lane.seq {
        return Some("stale chain sequence");
    }
    None
}

/// Compute the newest bound to resume a lane's listing from without a cursor
///
/// Resumes overlap one second past the newest unprocessed instant because the
/// corpus page pull is strict on both ends: without the overlap, items sharing
/// the exact `current` timestamp that a page cut split could be skipped
/// forever. The overlap is capped at the lane's own start so a fresh lane
/// (where `current == start`) never lists items outside its time shard.
///
/// # Arguments
///
/// * `current` - The upload timestamp of the oldest line this lane processed
/// * `lane_start` - The newest timestamp this lane covers
fn resume_start(current: DateTime<Utc>, lane_start: DateTime<Utc>) -> DateTime<Utc> {
    (current + chrono::Duration::seconds(1)).min(lane_start)
}

/// The corpus slice one chunk walked
struct ChunkSlice {
    /// The corpus list lines in this slice, newest to oldest
    lines: Vec<SampleListLine>,
    /// The server side cursor to resume from if more lines remain
    cursor: Option<Uuid>,
}

/// A single yara hunt worker
pub struct YaraHuntWorker {
    /// A service account client for Thorium
    thorium: Arc<Thorium>,
    /// The identity of this event handler process that owns our popped events
    worker: String,
    /// The single audited writer for hunt hits
    writer: YaraHitWriter,
    /// The live yara scan settings (size caps, timeouts, parallelism)
    yara_conf: Yara,
    /// The hunt chassis settings (chunk size, error sample cap)
    hunts_conf: Hunts,
    /// The hunt dedicated byte budget bounding in flight scan buffers
    budget: ScanBudget,
    /// The per worker cache of compiled rule subsets
    subsets: SubsetLru,
    /// The timestamp to retry failed events at if any failed
    retry_ts: Option<DateTime<Utc>>,
}

impl YaraHuntWorker {
    /// Process one hunt chunk event end to end
    ///
    /// # Arguments
    ///
    /// * `chunk` - The chunk event to process
    /// * `stats` - The stats to track/update
    #[instrument(
        name = "YaraHuntWorker::process_chunk",
        skip(self, stats),
        fields(hunt = chunk.hunt.to_string(), lane = chunk.lane, seq = chunk.seq)
    )]
    async fn process_chunk(
        &mut self,
        chunk: &HuntChunkEvent,
        stats: &mut YaraHuntStats,
    ) -> ChunkOutcome {
        // load this chunks hunt; deletion is observed lazily here
        let entity = match self.thorium.entities.get(chunk.hunt).await {
            Ok(entity) => entity,
            Err(error) => {
                // a deleted hunt ends its chains exactly like a cancel
                if error.status() == Some(reqwest::StatusCode::NOT_FOUND) {
                    // log that this hunt no longer exists
                    event!(Level::INFO, msg = "Hunt no longer exists; dropping chain");
                    stats.chains_dropped += 1;
                    return ChunkOutcome::Clear;
                }
                // anything else is transient so retry this chunk later
                event!(
                    Level::ERROR,
                    msg = "Failed to load hunt",
                    error = error.to_string(),
                );
                return ChunkOutcome::Retry;
            }
        };
        // make sure this entity actually is a hunt
        let EntityMetadata::Hunt(hunt) = &entity.metadata else {
            // a chunk event for a non hunt entity can never commit
            event!(
                Level::ERROR,
                msg = "Chunk event targets a non hunt entity",
                kind = entity.kind.as_str(),
            );
            return ChunkOutcome::Clear;
        };
        // drop terminal, missing lane, and stale sequence chains
        if let Some(reason) = chunk_drop_reason(hunt, chunk) {
            // log why this chain is being dropped
            event!(Level::INFO, msg = "Dropping hunt chain", reason = reason);
            stats.chains_dropped += 1;
            return ChunkOutcome::Clear;
        }
        // this lane exists per the drop check above
        let lane = &hunt.lanes[chunk.lane as usize];
        // load this hunts compiled rule subset through our LRU
        let subset_key = SubsetLru::key(&hunt.rules);
        if self.subsets.get(&subset_key).is_none() {
            // fetch and compile this hunts frozen subset
            let built = match build_subset(&self.thorium, &hunt.rules).await {
                Ok(built) => built,
                Err(error) => {
                    // a transient fetch or compile failure must never shrink
                    // or fail the hunt; retry this chunk later
                    event!(
                        Level::ERROR,
                        msg = "Failed to build hunt rule subset",
                        error = error.to_string(),
                    );
                    return ChunkOutcome::Retry;
                }
            };
            // log this build so operators can watch subset churn
            event!(
                Level::INFO,
                msg = "Compiled hunt rule subset",
                rules = built.context.meta.len(),
                missing_rules = built.missing_rules,
                quarantined = built.context.quarantined.len(),
                ruleset_hash = built.context.ruleset_hash,
            );
            self.subsets.insert(subset_key.clone(), built);
        }
        // a snapshot that loads to zero usable rules can never produce work;
        // every id resolved definitively (deleted or uncompilable) so this is
        // a terminal failure, not a transient one
        let zero_rules = self
            .subsets
            .peek(&subset_key)
            .is_none_or(|subset| subset.context.meta.is_empty());
        if zero_rules {
            // build the terminal failure checkpoint for this lane
            let checkpoint = HuntCheckpoint {
                lane: chunk.lane,
                seq: chunk.seq,
                current: lane.current,
                cursor: None,
                exhausted: false,
                failed: Some(
                    "the frozen rule snapshot loaded to 0 usable rules (deleted or uncompilable)"
                        .to_owned(),
                ),
                counters: HuntCounters::default(),
            };
            return self.checkpoint(chunk, checkpoint, stats).await;
        }
        // walk one bounded slice of this lanes corpus
        let slice = match self.list_slice(hunt, chunk).await {
            Ok(slice) => slice,
            Err(error) => {
                // listing failures are chunk level; retry this chunk later
                event!(
                    Level::ERROR,
                    msg = "Failed to list a corpus slice",
                    error = error.to_string(),
                );
                return ChunkOutcome::Retry;
            }
        };
        // scan this slice and fold the outcomes into counter deltas
        let Some(counters) = self
            .scan_slice(chunk.hunt, hunt, &subset_key, &slice, stats)
            .await
        else {
            // the subset vanished mid chunk; retry rather than commit a slice
            // that was never scanned
            return ChunkOutcome::Retry;
        };
        // the slice is newest to oldest so the last line is our new progress;
        // an empty slice keeps the lanes persisted progress unchanged
        let current = slice
            .lines
            .last()
            .map_or(lane.current, |line| line.uploaded);
        // the server returns no cursor once this lanes listing is exhausted
        let exhausted = slice.cursor.is_none();
        // commit this chunk and emit the next link in this lanes chain
        let checkpoint = HuntCheckpoint {
            lane: chunk.lane,
            seq: chunk.seq,
            current,
            cursor: slice.cursor,
            exhausted,
            failed: None,
            counters,
        };
        self.checkpoint(chunk, checkpoint, stats).await
    }

    /// List one bounded slice of a lane's corpus
    ///
    /// The lane's live server cursor is resumed when one exists; on a cursor
    /// loss (expiry or redis loss) the listing restarts from the lane's
    /// persisted progress plus the one second overlap.
    ///
    /// # Arguments
    ///
    /// * `hunt` - The hunt whose corpus is being walked
    /// * `chunk` - The chunk event naming the lane to walk
    #[instrument(name = "YaraHuntWorker::list_slice", skip_all, err(Debug))]
    async fn list_slice(&self, hunt: &Hunt, chunk: &HuntChunkEvent) -> Result<ChunkSlice, Error> {
        // this lane exists per the callers drop check
        let lane = &hunt.lanes[chunk.lane as usize];
        // get this hunts corpus bounds
        let bounds = hunt.corpus.bounds();
        // size this slice by the configured chunk size
        let chunk_size = usize::try_from(self.hunts_conf.chunk_size).unwrap_or(500);
        // build the listing opts for this slice; both the page size and the
        // limit must be the chunk size since the client sends their minimum
        let mut opts = FileListOpts::default()
            .page_size(chunk_size)
            .limit(chunk_size)
            .start(resume_start(lane.current, lane.start))
            .end(lane.end)
            .groups(bounds.groups.clone());
        // apply the corpus tag filters
        for (key, values) in &bounds.tags {
            for value in values {
                opts.tag_ref(key, value);
            }
        }
        // apply case insensitive tag matching when the corpus asked for it
        if bounds.tags_case_insensitive {
            opts = opts.tags_case_insensitive();
        }
        // resume this lanes live server cursor when one exists
        let with_cursor = match lane.cursor {
            Some(cursor_id) => {
                // try to resume the live cursor first
                match self
                    .thorium
                    .files
                    .list(&opts.clone().cursor(cursor_id))
                    .await
                {
                    Ok(cursor) => Some(cursor),
                    Err(error) => {
                        // a missing cursor restarts from the persisted
                        // progress plus the one second overlap
                        if error.status() == Some(reqwest::StatusCode::NOT_FOUND) {
                            // log that this lanes cursor was lost
                            event!(
                                Level::WARN,
                                msg = "Lane cursor was lost; restarting from lane progress",
                                cursor = cursor_id.to_string(),
                            );
                            None
                        } else {
                            return Err(error);
                        }
                    }
                }
            }
            None => None,
        };
        // fall back to a fresh listing when no live cursor served this slice
        let cursor = match with_cursor {
            Some(cursor) => cursor,
            None => self.thorium.files.list(&opts).await?,
        };
        // unpack this slice and the cursor to resume from
        Ok(ChunkSlice {
            lines: cursor.data,
            cursor: cursor.id,
        })
    }

    /// Scan one corpus slice and write its hits
    ///
    /// Per item failures count and continue. The only `None` return is the
    /// should-be-impossible case of the just loaded subset missing from the
    /// cache, which the caller maps to a retry so an unscanned slice is never
    /// committed.
    ///
    /// # Arguments
    ///
    /// * `hunt_id` - The entity id of the hunt this slice belongs to
    /// * `hunt` - The hunt this slice belongs to
    /// * `subset_key` - The LRU key of this hunts compiled subset
    /// * `slice` - The corpus slice to scan
    /// * `stats` - The stats to track/update
    #[instrument(name = "YaraHuntWorker::scan_slice", skip_all, fields(lines = slice.lines.len()))]
    async fn scan_slice(
        &self,
        hunt_id: Uuid,
        hunt: &Hunt,
        subset_key: &str,
        slice: &ChunkSlice,
        stats: &mut YaraHuntStats,
    ) -> Option<HuntCounters> {
        // start this chunks counter deltas with the lines we walked
        let mut counters = HuntCounters {
            listed: slice.lines.len() as u64,
            ..HuntCounters::default()
        };
        stats.listed += slice.lines.len();
        // dedupe this slice by sha256 so duplicate submissions scan once
        let mut seen: HashSet<&str> = HashSet::with_capacity(slice.lines.len());
        let unique: Vec<String> = slice
            .lines
            .iter()
            .filter(|line| seen.insert(line.sha256.as_str()))
            .map(|line| line.sha256.clone())
            .collect();
        // this subset is warm per the callers load
        let Some(subset) = self.subsets.peek(subset_key) else {
            // an evicted-just-loaded subset should be impossible; report it so
            // the caller retries instead of committing an unscanned slice
            event!(Level::ERROR, msg = "A just loaded subset was not cached");
            return None;
        };
        // resolve how many samples to scan concurrently
        let parallelism = self
            .yara_conf
            .scan_parallelism
            .or_else(|| {
                std::thread::available_parallelism()
                    .ok()
                    .map(std::num::NonZeroUsize::get)
            })
            .unwrap_or(4);
        // download and scan every unique sample concurrently
        let outcomes = stream::iter(unique)
            .map(|sha256| {
                // clone the shared handles this scan needs
                let thorium = self.thorium.clone();
                let context = subset.context.clone();
                let conf = self.yara_conf.clone();
                let budget = self.budget.clone();
                async move {
                    // download and scan this sample
                    let outcome = run_scan(thorium, context, conf, budget, sha256.clone()).await;
                    (sha256, outcome)
                }
            })
            .buffer_unordered(parallelism.max(1))
            .collect::<Vec<(String, ScanOutcome)>>()
            .await;
        // write each scans hits and fold its outcome into our deltas
        for (sha256, outcome) in outcomes {
            match outcome {
                ScanOutcome::Scanned { matches, .. } => {
                    counters.scanned += 1;
                    stats.scanned += 1;
                    // samples with no matches have nothing to write
                    if matches.is_empty() {
                        continue;
                    }
                    counters.matched += 1;
                    stats.matched += 1;
                    // write each matched rules hit through the audited route
                    for (rule_id, matched) in matches {
                        // get this rules scan time metadata
                        let Some(meta) = subset.context.meta.get(&rule_id) else {
                            // a namespace outside our subset is a bug; skip it
                            event!(
                                Level::ERROR,
                                msg = "Matched rule is not in this hunts subset",
                                rule = rule_id.to_string(),
                            );
                            continue;
                        };
                        // write this hit with the corpus groups as the event
                        // groups, depth 0 so hit tags land at depth 1, and
                        // this hunts id so the server decorates the hit; the
                        // server recomputes the write groups authoritatively
                        match self
                            .writer
                            .write_hit(
                                rule_id,
                                meta,
                                &sha256,
                                matched,
                                &subset.context.ruleset_hash,
                                hunt.corpus.bounds().groups.clone(),
                                0,
                                Some(hunt_id),
                            )
                            .await
                        {
                            Ok(resp) => {
                                // a write into any group counts as a finding
                                if resp.written.is_empty() {
                                    counters.suppressed += 1;
                                    stats.hits_suppressed += 1;
                                } else {
                                    counters.written += 1;
                                    stats.hits_written += 1;
                                    // a vanished hunt lands its hit without
                                    // decoration; surface the skip so this
                                    // workers stats stay truthful about the
                                    // HuntId browse record
                                    if resp.hunt_missing {
                                        event!(
                                            Level::WARN,
                                            msg = "Hunt vanished; hit written undecorated",
                                            sha256 = sha256,
                                            rule = rule_id.to_string(),
                                            hunt = hunt_id.to_string(),
                                        );
                                        stats.hits_undecorated += 1;
                                    }
                                }
                            }
                            Err(error) => {
                                // a hit write failure is a per item error; the
                                // lane moves on and the counters record it
                                event!(
                                    Level::ERROR,
                                    msg = "Failed to write hunt hit",
                                    sha256 = sha256,
                                    rule = rule_id.to_string(),
                                    error = error.to_string(),
                                );
                                record_error(
                                    &mut counters,
                                    stats,
                                    self.hunts_conf.max_error_samples,
                                    format!("hit write failed for {sha256}: {error}"),
                                );
                            }
                        }
                    }
                }
                ScanOutcome::Oversize => {
                    counters.skipped += 1;
                    stats.skipped_oversize += 1;
                }
                ScanOutcome::Missing => {
                    counters.missing += 1;
                    stats.skipped_missing += 1;
                }
                ScanOutcome::Failed(error) => {
                    // a failed download or scan is a per item error; a bad
                    // item never stalls a lane
                    event!(
                        Level::ERROR,
                        msg = "Failed to scan hunt sample",
                        sha256 = sha256,
                        error = error.to_string(),
                    );
                    record_error(
                        &mut counters,
                        stats,
                        self.hunts_conf.max_error_samples,
                        format!("scan failed for {sha256}: {error}"),
                    );
                }
            }
        }
        Some(counters)
    }

    /// Commit one chunks checkpoint and map the outcome onto this chunks fate
    ///
    /// # Arguments
    ///
    /// * `chunk` - The chunk event being committed
    /// * `checkpoint` - The checkpoint to commit
    /// * `stats` - The stats to track/update
    #[instrument(name = "YaraHuntWorker::checkpoint", skip(self, checkpoint, stats))]
    async fn checkpoint(
        &self,
        chunk: &HuntChunkEvent,
        checkpoint: HuntCheckpoint,
        stats: &mut YaraHuntStats,
    ) -> ChunkOutcome {
        // commit this checkpoint through the audited route
        match self
            .thorium
            .entities
            .checkpoint_hunt(chunk.hunt, &checkpoint)
            .await
        {
            Ok(resp) => {
                // this chunk committed and its chain continues or finalizes
                stats.chunks += 1;
                stats.checkpoints_won += 1;
                // log this commit so lane progress is observable
                event!(
                    Level::INFO,
                    msg = "Committed hunt checkpoint",
                    status = resp.status.to_string(),
                    emitted = resp.emitted,
                    exhausted = checkpoint.exhausted,
                );
                ChunkOutcome::Clear
            }
            Err(error) => match error.status() {
                // a lost compare-and-set means a duplicate already committed
                // this sequence; drop our stale chain
                Some(reqwest::StatusCode::CONFLICT) => {
                    stats.chunks += 1;
                    stats.checkpoints_lost += 1;
                    event!(Level::INFO, msg = "Checkpoint lost its CAS; dropping chain");
                    ChunkOutcome::Clear
                }
                // a 404 is ambiguous mid rolling upgrade: an old API without
                // the checkpoint route must not kill a live chain, so retry;
                // a genuinely deleted hunt clears at the hunt load instead
                Some(reqwest::StatusCode::NOT_FOUND) => {
                    event!(
                        Level::WARN,
                        msg = "Checkpoint route 404'd; retrying this chunk",
                        error = error.to_string(),
                    );
                    ChunkOutcome::Retry
                }
                // any other 4xx is deterministic and would retry forever
                Some(status) if status.is_client_error() => {
                    event!(
                        Level::ERROR,
                        msg = "Checkpoint rejected; dropping chain",
                        status = status.as_u16(),
                        error = error.to_string(),
                    );
                    ChunkOutcome::Clear
                }
                // transient server or network failures retry
                _ => {
                    event!(
                        Level::ERROR,
                        msg = "Failed to commit checkpoint",
                        error = error.to_string(),
                    );
                    ChunkOutcome::Retry
                }
            },
        }
    }
}

/// Record one bounded per item error sample
///
/// # Arguments
///
/// * `counters` - The chunk counter deltas to record into
/// * `stats` - The stats to track/update
/// * `cap` - The max number of error samples to keep in one chunk
/// * `sample` - The error sample to record
fn record_error(
    counters: &mut HuntCounters,
    stats: &mut YaraHuntStats,
    cap: usize,
    sample: String,
) {
    // count this error
    counters.errors += 1;
    stats.errors += 1;
    // keep a bounded sample of it for the hunts persisted counters
    if counters.error_samples.len() < cap {
        counters.error_samples.push(sample);
    }
}

impl EventWorkerSupportCore for YaraHuntWorker {
    /// The stats for all workers of this kind
    type Stats = YaraHuntStats;

    /// Build this specific kind of worker
    ///
    /// # Arguments
    ///
    /// * `cache` - A cache of all info a worker may need to be created
    /// * `thorium` - A Thorium client
    /// * `worker` - The identity of this event handler process
    async fn new(cache: &EventWorkerCache, thorium: &Arc<Thorium>, worker: &str) -> Self {
        // snapshot the live yara and hunt settings for this Thorium instance
        let yara_conf = cache.conf.thorium.yara.clone();
        let hunts_conf = cache.conf.thorium.hunts.clone();
        // build the hunt dedicated byte budget; config load validates the hunt
        // budget >= the live max scan size so a failure here means the conf
        // was built by hand and the safest fallback is one large grant
        let budget = ScanBudget::new(
            hunts_conf.yara.scan_buffer_budget,
            yara_conf.small_class_size,
            yara_conf.max_scan_size,
        )
        .unwrap_or_else(|error| {
            // log that we are falling back to a minimal budget
            event!(
                Level::ERROR,
                msg = "Falling back to a minimal hunt scan budget",
                error = error.to_string(),
            );
            // a budget of exactly one large grant always builds
            ScanBudget::new(
                yara_conf.max_scan_size,
                yara_conf.small_class_size,
                yara_conf.max_scan_size,
            )
            .expect("a budget of one large grant must always build")
        });
        // size the compiled subset cache from the hunt config
        let subsets = SubsetLru::new(hunts_conf.yara.ruleset_lru_size);
        // build this worker
        YaraHuntWorker {
            thorium: thorium.clone(),
            worker: worker.to_owned(),
            writer: YaraHitWriter::new(thorium.clone()),
            yara_conf,
            hunts_conf,
            budget,
            subsets,
            retry_ts: None,
        }
    }

    /// The types of events this worker handles
    #[inline]
    fn event_kinds() -> Vec<EventType> {
        vec![EventType::YaraHuntChunk]
    }

    /// The max number of events this worker pops per page
    ///
    /// A chunk is minutes of scanning, chunks in a page process sequentially,
    /// and claimed events age toward the in flight backstop while they queue -
    /// so the page is one chunk: the lease matches the work unit, a SIGTERM
    /// quiesce replays at most one chunk, and cancels are observed between
    /// chunks.
    #[inline]
    fn pop_limit() -> usize {
        1
    }

    /// The types of cache updates this worker subscribes too
    #[inline]
    fn cache_subscriptions() -> Vec<WorkerCacheUpdateKinds> {
        vec![WorkerCacheUpdateKinds::YaraRulesContext]
    }

    /// Apply an update to our local worker cache
    ///
    /// # Arguments
    ///
    /// * `update` - The update to apply to this workers cache
    fn apply_cache_update(&mut self, update: WorkerCacheUpdates) {
        // this worker only subscribes to rule context updates
        if let WorkerCacheUpdates::YaraRulesContext(context) = update {
            // evict only the compiled subsets this rule change touched so mid
            // hunt edits land without wiping every warm build on the periodic
            // full rebuild backstop
            self.subsets.evict_changed(&context.meta);
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
            worker = "YaraHuntWorker",
            chunks = stats.chunks,
            listed = stats.listed,
            scanned = stats.scanned,
            matched = stats.matched,
            hits_written = stats.hits_written,
            hits_suppressed = stats.hits_suppressed,
            hits_undecorated = stats.hits_undecorated,
            skipped_oversize = stats.skipped_oversize,
            skipped_missing = stats.skipped_missing,
            errors = stats.errors,
            checkpoints_won = stats.checkpoints_won,
            checkpoints_lost = stats.checkpoints_lost,
            chains_dropped = stats.chains_dropped,
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
            ("chunks".to_owned(), stats.chunks as u64),
            ("listed".to_owned(), stats.listed as u64),
            ("scanned".to_owned(), stats.scanned as u64),
            ("matched".to_owned(), stats.matched as u64),
            ("hits_written".to_owned(), stats.hits_written as u64),
            ("hits_suppressed".to_owned(), stats.hits_suppressed as u64),
            ("hits_undecorated".to_owned(), stats.hits_undecorated as u64),
            ("skipped_oversize".to_owned(), stats.skipped_oversize as u64),
            ("skipped_missing".to_owned(), stats.skipped_missing as u64),
            ("errors".to_owned(), stats.errors as u64),
            ("checkpoints_won".to_owned(), stats.checkpoints_won as u64),
            ("checkpoints_lost".to_owned(), stats.checkpoints_lost as u64),
            ("chains_dropped".to_owned(), stats.chains_dropped as u64),
        ])
    }
}

impl EventWorkerMutableSupport for YaraHuntWorker {
    /// Process a page of hunt chunk events
    ///
    /// # Arguments
    ///
    /// * `events` - The events to process
    /// * `stats` - The stats to track/update
    #[instrument(name = "YaraHuntWorker::process", skip_all, fields(events = events.len()))]
    async fn process(
        &mut self,
        events: Vec<Event>,
        stats: &mut YaraHuntStats,
    ) -> Result<(), Error> {
        // track the events to clear and whether any chunk failed
        let mut cleared = Vec::with_capacity(events.len());
        let mut any_failed = false;
        // process this pages chunks one at a time; chunks are minutes of work
        // so sequential processing keeps budget pressure and replay bounded
        for event in events {
            // only hunt chunk events belong on this queue
            let EventData::YaraHuntChunk(chunk) = &event.data else {
                // clear this stray event so it never leaks in flight
                event!(
                    Level::ERROR,
                    msg = "Clearing a stray non chunk event",
                    event = event.id.to_string(),
                );
                cleared.push(event.id);
                continue;
            };
            // process this chunk end to end
            match self.process_chunk(chunk, stats).await {
                // this chunk was handled or deliberately dropped
                ChunkOutcome::Clear => cleared.push(event.id),
                // this chunk hit a transient failure so leave it in flight
                ChunkOutcome::Retry => any_failed = true,
            }
        }
        // clear every event our chunks covered; a failed clear just leaves
        // them in flight for the retry pass (replay collapses at the CAS) and
        // must never kill the daemon
        if !cleared.is_empty()
            && let Err(error) = self
                .thorium
                .events
                .clear(
                    EventType::YaraHuntChunk,
                    &EventIds::from(cleared),
                    Some(&self.worker),
                )
                .await
        {
            // log that we failed to clear processed events
            event!(
                Level::ERROR,
                msg = "Failed to clear processed chunk events",
                error = error.to_string(),
            );
            // arm the retry timer so the replay happens promptly
            any_failed = true;
        }
        // arm the retry timer if any chunk failed
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
    use thorium::models::{
        HuntCorpus, HuntCorpusBounds, HuntEngine, HuntLane, HuntSelector, HuntStatus,
    };

    /// Build a hunt with one lane for drop decision tests
    ///
    /// # Arguments
    ///
    /// * `status` - The status to set on this hunt
    /// * `lane_seq` - The persisted sequence floor for the hunts one lane
    fn test_hunt(status: HuntStatus, lane_seq: u64) -> Hunt {
        // build a one hour corpus window
        let start = Utc::now();
        let end = start - chrono::Duration::hours(1);
        Hunt {
            engine: HuntEngine::Yara,
            selector: HuntSelector::All,
            rules: vec![Uuid::new_v4()],
            excluded_quarantined: 0,
            excluded_foreign: 0,
            corpus: HuntCorpus::Files(HuntCorpusBounds {
                groups: vec!["corn".to_owned()],
                start,
                end,
                tags: std::collections::HashMap::default(),
                tags_case_insensitive: false,
            }),
            status,
            lanes: vec![HuntLane {
                start,
                current: start,
                end,
                seq: lane_seq,
                cursor: None,
                exhausted: false,
            }],
            counters: HuntCounters::default(),
            failure: None,
        }
    }

    /// Build a chunk event for drop decision tests
    ///
    /// # Arguments
    ///
    /// * `lane` - The lane index this chunk advances
    /// * `seq` - The sequence this chunk would commit
    fn test_chunk(lane: u32, seq: u64) -> HuntChunkEvent {
        HuntChunkEvent {
            hunt: Uuid::new_v4(),
            lane,
            seq,
        }
    }

    /// Live chains with current sequences must never be dropped
    #[test]
    fn chunk_drop_allows_live_chains() {
        // a running hunt whose lane expects this exact sequence
        let hunt = test_hunt(HuntStatus::Running, 3);
        assert_eq!(chunk_drop_reason(&hunt, &test_chunk(0, 3)), None);
        // a sequence ahead of the persisted floor is the live head after a
        // lost persist and must still process
        assert_eq!(chunk_drop_reason(&hunt, &test_chunk(0, 4)), None);
        // created hunts process their first chunks
        let created = test_hunt(HuntStatus::Created, 0);
        assert_eq!(chunk_drop_reason(&created, &test_chunk(0, 0)), None);
    }

    /// Terminal hunts, missing lanes, and stale sequences must drop
    #[test]
    fn chunk_drop_matrix() {
        // every terminal status drops its chains
        for status in [
            HuntStatus::Cancelled,
            HuntStatus::Completed,
            HuntStatus::Failed,
        ] {
            let hunt = test_hunt(status, 0);
            assert!(chunk_drop_reason(&hunt, &test_chunk(0, 0)).is_some());
        }
        // a lane index past the hunts lanes drops
        let hunt = test_hunt(HuntStatus::Running, 0);
        assert!(chunk_drop_reason(&hunt, &test_chunk(1, 0)).is_some());
        // a sequence behind the persisted floor already committed
        let hunt = test_hunt(HuntStatus::Running, 5);
        assert!(chunk_drop_reason(&hunt, &test_chunk(0, 4)).is_some());
    }

    /// Lane resumes must overlap one second but never leave the lane
    #[test]
    fn resume_start_overlap_and_clamp() {
        // build a lane window
        let lane_start = Utc::now();
        // a fresh lane (current == start) must list from exactly its start
        assert_eq!(resume_start(lane_start, lane_start), lane_start);
        // a mid lane resume must overlap one second past its progress
        let current = lane_start - chrono::Duration::seconds(30);
        assert_eq!(
            resume_start(current, lane_start),
            current + chrono::Duration::seconds(1)
        );
        // a resume within a second of the lane start must clamp to the start
        let near_start = lane_start - chrono::Duration::milliseconds(500);
        assert_eq!(resume_start(near_start, lane_start), lane_start);
    }

    /// Hunt stats must fold together with saturating adds
    #[test]
    fn hunt_stats_add_assign() {
        // build two stats values with distinct fields
        let mut total = YaraHuntStats {
            chunks: 1,
            listed: usize::MAX,
            ..YaraHuntStats::default()
        };
        let delta = YaraHuntStats {
            chunks: 2,
            listed: 10,
            checkpoints_lost: 1,
            hits_undecorated: 4,
            ..YaraHuntStats::default()
        };
        // fold the delta into the totals
        total += delta;
        // counts must add and saturate instead of wrapping
        assert_eq!(total.chunks, 3);
        assert_eq!(total.listed, usize::MAX);
        assert_eq!(total.checkpoints_lost, 1);
        assert_eq!(total.hits_undecorated, 4);
    }

    /// The counter map keys every [`YaraHuntStats`] field by its name so the
    /// published counters match the tracing gauges field for field.
    #[test]
    fn hunt_stats_counter_map() {
        // build stats with a distinct value per field
        let stats = YaraHuntStats {
            chunks: 1,
            listed: 2,
            scanned: 3,
            matched: 4,
            hits_written: 5,
            hits_suppressed: 6,
            hits_undecorated: 7,
            skipped_oversize: 8,
            skipped_missing: 9,
            errors: 10,
            checkpoints_won: 11,
            checkpoints_lost: 12,
            chains_dropped: 13,
        };
        // flatten these stats into a counter map
        let map = YaraHuntWorker::counter_map(&stats);
        // every field must appear under its own name with its value
        let expected = [
            ("chunks", 1),
            ("listed", 2),
            ("scanned", 3),
            ("matched", 4),
            ("hits_written", 5),
            ("hits_suppressed", 6),
            ("hits_undecorated", 7),
            ("skipped_oversize", 8),
            ("skipped_missing", 9),
            ("errors", 10),
            ("checkpoints_won", 11),
            ("checkpoints_lost", 12),
            ("chains_dropped", 13),
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
