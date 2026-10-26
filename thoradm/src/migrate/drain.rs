//! The `drain-reactions` check
//!
//! Reactions that are still `Created` or `Started` hold queued jobs and running workers shaped for
//! the stage path, which the tool path can't read, so the conversions must not start while any
//! exist. This migration only checks: it reads Redis, writes nothing, never deletes a reaction,
//! and asks the operator to wait while any reaction is in flight or any stage-path worker is
//! still registered. The admin lets them finish or deletes the stuck ones with
//! `thorctl reactions delete` while the previous API still runs.

use async_trait::async_trait;
use thorium::models::schema::{
    Batch, CheckWait, MigrateError, MigrationRunner, Outcome, PassCounts, RunContext, RunMode,
    Slice,
};
use thorium::models::upgrades::Record;

use super::registry::{Backend, MigrationDef, Support};

/// The id of this migration
pub const ID: &str = "drain-reactions";

/// How long the operator waits before it runs the check again, in seconds
pub const RETRY_AFTER_SECS: u64 = 15;

/// The statuses a reaction holds while its work is unfinished
const IN_FLIGHT: [&str; 2] = ["Created", "Started"];

/// Whether a stored reaction status is one of an unfinished reaction
///
/// A status is stored as a JSON string, so the quotes are dropped before it is compared.
///
/// # Arguments
///
/// * `stored` - The `status` field of a reaction's data hash
#[must_use]
pub fn is_in_flight(stored: &str) -> bool {
    // the quotes around a JSON string aren't part of the status
    let status = stored.trim().trim_matches('"');
    IN_FLIGHT.contains(&status)
}

/// Build the text that says what the step waits for and how to clear it
///
/// The text holds counts only: it reaches the cluster's status, so it never names a reaction, a
/// group, or an argument.
///
/// # Arguments
///
/// * `reactions` - How many reactions are `Created` or `Started`
/// * `workers` - How many stage-path workers are still registered
#[must_use]
pub fn wait_reason(reactions: u64, workers: u64) -> Option<String> {
    // nothing in flight means nothing to wait for
    if reactions == 0 && workers == 0 {
        return None;
    }
    // say what is still in flight
    let mut found = Vec::new();
    if reactions > 0 {
        found.push(format!(
            "{reactions} reaction(s) are still Created or Started"
        ));
    }
    if workers > 0 {
        found.push(format!(
            "{workers} stage-path worker(s) are still registered"
        ));
    }
    // and how the admin clears it
    Some(format!(
        "{}. The conversion waits until none remain, with Thorium running normally. Stop \
         submitting reactions, let them finish, or delete a stuck one with `thorctl reactions \
         delete` while the previous API is still running; list them with `thorctl reactions get \
         --group <group> --pipeline <pipeline> --status started` (and `created`). Workers exit on \
         their own once they find no job.",
        found.join(" and ")
    ))
}

/// A Redis error as a migration error
///
/// # Arguments
///
/// * `err` - The error
fn redis_error(err: impl std::fmt::Display) -> MigrateError {
    // every Redis failure may pass on a retry
    MigrateError::Backend(err.to_string())
}

/// List the keys of a Redis family
///
/// # Arguments
///
/// * `ctx` - The run's context
/// * `pattern` - The key pattern to scan for
async fn scan(ctx: &RunContext, pattern: &str) -> Result<Vec<String>, MigrateError> {
    // walk the keyspace until the cursor returns to the start
    let mut conn = ctx.backends.redis.get().await.map_err(redis_error)?;
    let mut keys = Vec::new();
    let mut cursor = 0_u64;
    loop {
        let (next, found): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(pattern)
            .arg("COUNT")
            .arg(500)
            .query_async(&mut *conn)
            .await
            .map_err(redis_error)?;
        keys.extend(found);
        cursor = next;
        if cursor == 0 {
            break;
        }
    }
    Ok(keys)
}

/// Count the reactions that are still `Created` or `Started`
///
/// # Arguments
///
/// * `ctx` - The run's context
async fn count_in_flight(ctx: &RunContext) -> Result<u64, MigrateError> {
    // read only the status field of each reaction's data hash
    let keys = scan(ctx, &format!("{}:reaction_data:*", ctx.backends.namespace)).await?;
    let mut conn = ctx.backends.redis.get().await.map_err(redis_error)?;
    let mut count = 0;
    for key in keys {
        let status: Option<String> = redis::cmd("HGET")
            .arg(&key)
            .arg("status")
            .query_async(&mut *conn)
            .await
            .map_err(redis_error)?;
        if status.as_deref().is_some_and(is_in_flight) {
            count += 1;
        }
    }
    Ok(count)
}

/// Count the registered workers the tool path can't read: those without a tool instance
///
/// # Arguments
///
/// * `ctx` - The run's context
async fn count_stage_workers(ctx: &RunContext) -> Result<u64, MigrateError> {
    // a worker the previous release registered has no `tool_instance` field
    let keys = scan(ctx, &format!("{}:worker_data:*", ctx.backends.namespace)).await?;
    let mut conn = ctx.backends.redis.get().await.map_err(redis_error)?;
    let mut count = 0;
    for key in keys {
        let has_instance: bool = redis::cmd("HEXISTS")
            .arg(&key)
            .arg("tool_instance")
            .query_async(&mut *conn)
            .await
            .map_err(redis_error)?;
        if !has_instance {
            count += 1;
        }
    }
    Ok(count)
}

/// The runner of the `drain-reactions` check, which converts and writes nothing
pub struct DrainReactions;

#[async_trait]
impl MigrationRunner for DrainReactions {
    /// Make one empty slice, since the check reads its own keys
    ///
    /// # Arguments
    ///
    /// * `n` - How many slices were asked for
    /// * `ctx` - The run's context
    async fn slices(&self, n: u32, ctx: &RunContext) -> Result<Vec<Slice>, MigrateError> {
        // one slice is enough for a check that classifies no record
        let _ = (n, ctx);
        Ok(vec![Slice {
            index: 0,
            start: serde_json::Value::Null,
            end: serde_json::Value::Null,
        }])
    }

    /// Read no records
    ///
    /// # Arguments
    ///
    /// * `slice` - The slice to read
    /// * `after` - The last position done
    /// * `ctx` - The run's context
    async fn next_batch(
        &self,
        slice: &Slice,
        after: &serde_json::Value,
        ctx: &RunContext,
    ) -> Result<Batch, MigrateError> {
        // the counts are read when the check decides whether to wait, not record by record
        let _ = (slice, ctx);
        Ok(Batch {
            records: Vec::new(),
            next: after.clone(),
            done: true,
        })
    }

    /// Leave a record as it is
    ///
    /// # Arguments
    ///
    /// * `record` - The stored record
    /// * `mode` - `Run` or `Check`
    /// * `ctx` - The run's context
    async fn process(
        &self,
        record: &Record,
        mode: RunMode,
        ctx: &RunContext,
    ) -> Result<Outcome, MigrateError> {
        // nothing is ever converted
        let _ = (record, mode, ctx);
        Ok(Outcome::Current)
    }

    /// Nothing to record once every slice is done
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run's context
    async fn finish(&self, ctx: &RunContext) -> Result<(), MigrateError> {
        // the check leaves nothing behind
        let _ = ctx;
        Ok(())
    }

    /// Wait while a reaction is in flight or a stage-path worker is registered
    ///
    /// # Arguments
    ///
    /// * `counts` - What the check classified, which is nothing
    /// * `ctx` - The run's context
    async fn check_wait(
        &self,
        counts: &PassCounts,
        ctx: &RunContext,
    ) -> Result<Option<CheckWait>, MigrateError> {
        // count what is still in flight, from Redis only
        let _ = counts;
        let reactions = count_in_flight(ctx).await?;
        let workers = count_stage_workers(ctx).await?;
        // wait with the counts and the way to clear them, or let the step skip
        Ok(wait_reason(reactions, workers).map(|reason| CheckWait {
            reason,
            retry_after_secs: RETRY_AFTER_SECS,
        }))
    }
}

/// The migration's registry entry: checked, never run, since it only waits
pub const DRAIN_REACTIONS: MigrationDef = MigrationDef {
    id: ID,
    backend: Backend::Redis,
    support: Support::CheckOnly,
    runner: &DrainReactions,
    pass: ID,
    chain: &[ID],
    group_max: 5,
    families: &["reactions", "workers"],
    has_views: false,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a created or started reaction is in flight, quoted or not
    #[test]
    fn only_unfinished_statuses_are_in_flight() {
        // the statuses of unfinished reactions, as stored (a JSON string) and bare
        for status in ["\"Created\"", "\"Started\"", "Created", "Started"] {
            assert!(is_in_flight(status), "{status}");
        }
        // finished reactions, and text that isn't a status, are not
        for status in [
            "\"Completed\"",
            "\"Failed\"",
            "Completed",
            "\"created\"",
            "",
        ] {
            assert!(!is_in_flight(status), "{status}");
        }
    }

    /// Nothing in flight means no wait
    #[test]
    fn nothing_in_flight_does_not_wait() {
        assert_eq!(wait_reason(0, 0), None);
    }

    /// The wait names the counts and the command that clears a reaction, and nothing else
    #[test]
    fn the_wait_names_the_counts_and_the_delete_command() {
        // reactions alone
        let reason = wait_reason(130, 0).expect("a wait");
        assert!(reason.contains("130 reaction(s)"), "{reason}");
        assert!(
            !reason.contains("worker(s) are still registered"),
            "{reason}"
        );
        assert!(reason.contains("thorctl reactions delete"), "{reason}");
        // workers alone
        let reason = wait_reason(0, 2).expect("a wait");
        assert!(reason.contains("2 stage-path worker(s)"), "{reason}");
        assert!(!reason.contains("reaction(s) are still"), "{reason}");
        // both, in one reason
        let reason = wait_reason(3, 4).expect("a wait");
        assert!(
            reason.contains("3 reaction(s)") && reason.contains("4 stage-path"),
            "{reason}"
        );
    }
}
