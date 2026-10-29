//! Handles hunt specific redis state
//!
//! Hunt entities themselves live in the shared entities tables; this module
//! owns the per lane checkpoint sequence keys the chunk chains commit through
//! and the short lived per hunt persist mutex that serializes entity writers.

use chrono::Utc;
use redis::cmd;
use uuid::Uuid;

use super::keys::HuntKeys;
use crate::models::{Event, EventType};
use crate::utils::{ApiError, Shared};
use crate::{conn, internal_err, query, serialize};

/// How long a lane's checkpoint sequence key lives without a refresh
///
/// Checkpoints refresh this on every commit so only abandoned hunts let it
/// expire; a lost key is reseeded from the further along of the entity's last
/// persisted sequence and the committing chunk's own sequence at the cost of
/// at most one replayed, idempotent chunk. This mirrors the 7 day lifetime
/// paged list cursors get.
pub const LANE_SEQ_TTL_SECS: u64 = 604_800;

/// How long a hunt's persist mutex may be held before it expires
///
/// A persist is one entity read and one entity write so this is generous; the
/// expiry only exists so a crashed API replica can never wedge a hunt's
/// writers forever.
const PERSIST_LOCK_TTL_MS: u64 = 30_000;

/// How many times to retry acquiring a hunt's persist mutex before giving up
///
/// Contention is bounded by the hunt's lane count plus a cancel or delete, so
/// 50 attempts at 100ms covers seconds of sustained contention before a 500.
const PERSIST_LOCK_ATTEMPTS: u32 = 50;

/// How long to sleep between persist mutex acquire attempts
const PERSIST_LOCK_RETRY_MS: u64 = 100;

/// Atomically advance one lane's checkpoint sequence and emit the next chunk
///
/// The entity row is last-write-wins so a sequence check there would let two
/// API replicas serving a duplicated checkpoint both pass and fork the lane
/// chain permanently; this script is the one authoritative committer. Exactly
/// one caller wins each sequence number: the winner advances the sequence and
/// enqueues the next chunk event in the same atomic step, losers emit nothing.
///
/// If the sequence key was lost it is reseeded at the further along of the
/// entity's persisted sequence and the committing chunk's own sequence - a
/// chunk event carrying seq N is proof the chain legitimately reached N, and
/// the persisted view can lag it, so seeding at a lagging floor would reject
/// the live head and strand the lane.
///
/// Keys: the lane's sequence key and the engine's pending event queue.
/// Args: the submitted sequence, the entity's persisted sequence (the reseed
/// floor), the serialized next chunk event (an empty string emits nothing),
/// the score to enqueue the next event at, and the sequence key TTL.
///
/// Returns `{1, next seq}` when this commit won and `{0, stored seq}` when it
/// lost.
static COMMIT_LANE_SCRIPT: std::sync::LazyLock<redis::Script> = std::sync::LazyLock::new(|| {
    redis::Script::new(
        r#"
        local stored = redis.call('get', KEYS[1])
        if not stored then
            stored = math.max(tonumber(ARGV[2]), tonumber(ARGV[1]))
            redis.call('set', KEYS[1], stored, 'EX', ARGV[5])
        end
        stored = tonumber(stored)
        if stored ~= tonumber(ARGV[1]) then
            return {0, stored}
        end
        local next = stored + 1
        redis.call('set', KEYS[1], next, 'EX', ARGV[5])
        if ARGV[3] ~= '' then
            redis.call('zadd', KEYS[2], ARGV[4], ARGV[3])
        end
        return {1, next}
        "#,
    )
});

/// Atomically release a hunt persist mutex only while we still hold it
///
/// Keys: the hunt's persist mutex key.
/// Args: the token this holder acquired the mutex with.
///
/// Returns 1 if the mutex was ours and was released and 0 otherwise.
static RELEASE_PERSIST_SCRIPT: std::sync::LazyLock<redis::Script> =
    std::sync::LazyLock::new(|| {
        redis::Script::new(
            r#"
            if redis.call('get', KEYS[1]) == ARGV[1] then
                redis.call('del', KEYS[1])
                return 1
            end
            return 0
            "#,
        )
    });

/// The outcome of one lane checkpoint compare-and-set
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneCommit {
    /// Whether this commit won the compare-and-set
    pub won: bool,
    /// The sequence the lane expects next after this call
    ///
    /// On a win this is the submitted sequence plus one; on a loss it is the
    /// authoritative stored sequence the submitted one failed to match.
    pub stored: u64,
}

/// A held per hunt persist mutex
///
/// This is deliberately not a drop guard: releases go back to redis so they
/// must be awaited explicitly through [`unlock_persist`].
#[derive(Debug)]
pub struct HuntPersistLock {
    /// The key this mutex lives at
    key: String,
    /// The token proving this holder owns the mutex
    token: String,
}

/// Seed the checkpoint sequence keys for every lane of a new hunt
///
/// # Arguments
///
/// * `hunt` - The hunt's entity id
/// * `lanes` - The number of lanes to seed sequence keys for
/// * `shared` - Shared Thorium objects
#[tracing::instrument(name = "db::hunts::seed_lane_seqs", skip(shared), err(Debug))]
pub async fn seed_lane_seqs(hunt: &Uuid, lanes: u32, shared: &Shared) -> Result<(), ApiError> {
    // build a pipeline seeding every lane's sequence key
    let mut pipe = redis::pipe();
    // seed each lane's sequence at 0 since the first chunk commits seq 0
    for lane in 0..lanes {
        // build the key for this lane's sequence
        let key = HuntKeys::lane_seq(hunt, lane, shared);
        // seed this lane's sequence with the shared TTL
        pipe.cmd("set")
            .arg(key)
            .arg(0)
            .arg("EX")
            .arg(LANE_SEQ_TTL_SECS);
    }
    // execute our pipeline
    let _: () = pipe.query_async(conn!(shared)).await?;
    Ok(())
}

/// Try to commit one lane checkpoint and emit the next chunk event atomically
///
/// # Arguments
///
/// * `hunt` - The hunt's entity id
/// * `lane` - The lane index this checkpoint advances
/// * `seq` - The sequence this checkpoint is committing: the CAS term
/// * `floor` - The entity's persisted sequence to reseed a lost key from
/// * `kind` - The engine's chunk event kind naming the queue to emit into
/// * `next_event` - The next chunk event to emit on a win if the lane continues
/// * `pacing_secs` - How many seconds past now to score the next event at
/// * `shared` - Shared Thorium objects
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(name = "db::hunts::commit_lane", skip(next_event, shared), err(Debug))]
pub async fn commit_lane(
    hunt: &Uuid,
    lane: u32,
    seq: u64,
    floor: u64,
    kind: EventType,
    next_event: Option<&Event>,
    pacing_secs: u64,
    shared: &Shared,
) -> Result<LaneCommit, ApiError> {
    // build the keys for this lane's sequence and the engine's pending queue
    let seq_key = HuntKeys::lane_seq(hunt, lane, shared);
    let queue_key = super::keys::EventKeys::queue(kind, shared);
    // serialize the next chunk event; an empty string emits nothing
    let serialized = match next_event {
        Some(event) => serialize!(event),
        None => String::new(),
    };
    // score the next event in the future so the pop maturity floor paces it
    let pacing = i64::try_from(pacing_secs).unwrap_or(i64::MAX);
    let score = Utc::now().timestamp().saturating_add(pacing);
    // run the compare-and-set and the emission in one atomic script
    let (won, stored): (i64, u64) = COMMIT_LANE_SCRIPT
        .key(&seq_key)
        .key(&queue_key)
        .arg(seq)
        .arg(floor)
        .arg(serialized)
        .arg(score)
        .arg(LANE_SEQ_TTL_SECS)
        .invoke_async(conn!(shared))
        .await?;
    Ok(LaneCommit {
        won: won == 1,
        stored,
    })
}

/// Acquire a hunt's persist mutex, retrying briefly on contention
///
/// # Arguments
///
/// * `hunt` - The hunt's entity id
/// * `shared` - Shared Thorium objects
#[tracing::instrument(name = "db::hunts::lock_persist", skip(shared), err(Debug))]
pub async fn lock_persist(hunt: &Uuid, shared: &Shared) -> Result<HuntPersistLock, ApiError> {
    // build the key this hunt's persist mutex lives at
    let key = HuntKeys::persist(hunt, shared);
    // mint a token proving our ownership of the mutex
    let token = Uuid::new_v4().to_string();
    // try to acquire the mutex for a bounded number of attempts
    for _ in 0..PERSIST_LOCK_ATTEMPTS {
        // try to set the mutex only if nobody else holds it
        let acquired: Option<String> = query!(
            cmd("set")
                .arg(&key)
                .arg(&token)
                .arg("NX")
                .arg("PX")
                .arg(PERSIST_LOCK_TTL_MS),
            shared
        )
        .await?;
        // a set reply means the mutex is ours
        if acquired.is_some() {
            return Ok(HuntPersistLock { key, token });
        }
        // someone else holds the mutex so wait briefly and retry
        tokio::time::sleep(std::time::Duration::from_millis(PERSIST_LOCK_RETRY_MS)).await;
    }
    // sustained contention past the bound is a server side problem
    internal_err!(format!(
        "Timed out acquiring the persist mutex for hunt {hunt}"
    ))
}

/// Check whether a hunt persist mutex is still held by us
///
/// Writers call this right before their entity write so an expired mutex is
/// detected while the write can still be abandoned instead of clobbering a
/// peer's update.
///
/// # Arguments
///
/// * `lock` - The persist mutex to check
/// * `shared` - Shared Thorium objects
#[tracing::instrument(name = "db::hunts::persist_lock_held", skip_all, err(Debug))]
pub async fn persist_lock_held(lock: &HuntPersistLock, shared: &Shared) -> Result<bool, ApiError> {
    // get the current token at the mutex key if any
    let current: Option<String> = query!(cmd("get").arg(&lock.key), shared).await?;
    // the mutex is still ours only while it holds our token
    Ok(current.as_deref() == Some(lock.token.as_str()))
}

/// Release a hunt's persist mutex if we still hold it
///
/// Returns whether the mutex was still ours at release; a false return means
/// the mutex expired mid hold and another writer may have interleaved, so the
/// caller should treat its write as potentially clobbered.
///
/// # Arguments
///
/// * `lock` - The persist mutex to release
/// * `shared` - Shared Thorium objects
#[tracing::instrument(name = "db::hunts::unlock_persist", skip_all, err(Debug))]
pub async fn unlock_persist(lock: HuntPersistLock, shared: &Shared) -> Result<bool, ApiError> {
    // release the mutex only while it still holds our token
    let released: i64 = RELEASE_PERSIST_SCRIPT
        .key(&lock.key)
        .arg(&lock.token)
        .invoke_async(conn!(shared))
        .await?;
    Ok(released == 1)
}
