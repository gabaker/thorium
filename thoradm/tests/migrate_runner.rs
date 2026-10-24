//! Tests of the migration runner driven in-process against Scylla and Redis: checkpoints, workers,
//! attempts, grouped issues, the group limit, re-runs, checks, and the writer lease

mod common;

use async_trait::async_trait;
use common::migrate::World;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use thoradm::migrate::fixture;
use thoradm::migrate::registry::MigrationDef;
use thorium::models::schema::{
    Batch, CheckWait, MigrateError, MigrationRunStatus, MigrationRunner, Outcome, PassCounts,
    PassOutcome, RunContext, RunMode, RunOptions, Slice, WriterLease, check_pass, issue_groups,
    ring_slices, run_pass, verify_pass,
};
use thorium::models::upgrades::{IssueReason, Record};
use tokio_util::sync::CancellationToken;

/// A runner that cancels the run after it has read some batches, to interrupt a pass at a batch
/// boundary
struct CancelAfter {
    /// The migration it wraps
    inner: &'static dyn MigrationRunner,
    /// How many batches to read before cancelling
    after: usize,
    /// How many batches have been read
    seen: AtomicUsize,
    /// The token the run's context holds
    cancel: CancellationToken,
}

impl CancelAfter {
    /// Wrap a migration so the run it is in is cancelled after `after` batches
    ///
    /// # Arguments
    ///
    /// * `def` - The migration to wrap
    /// * `after` - How many batches to read before cancelling
    /// * `cancel` - The token the run's context holds
    fn new(def: &MigrationDef, after: usize, cancel: CancellationToken) -> Self {
        // nothing has been read yet
        CancelAfter {
            inner: def.runner,
            after,
            seen: AtomicUsize::new(0),
            cancel,
        }
    }
}

#[async_trait]
impl MigrationRunner for CancelAfter {
    /// Split the family as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `n` - How many slices to make
    /// * `ctx` - The backends, settings, and mode of this run
    async fn slices(&self, n: u32, ctx: &RunContext) -> Result<Vec<Slice>, MigrateError> {
        // delegate
        self.inner.slices(n, ctx).await
    }

    /// Read a batch, then cancel the run once enough have been read
    ///
    /// # Arguments
    ///
    /// * `slice` - The slice to read
    /// * `after` - The last position done, or the slice's start
    /// * `ctx` - The backends, settings, and mode of this run
    async fn next_batch(
        &self,
        slice: &Slice,
        after: &serde_json::Value,
        ctx: &RunContext,
    ) -> Result<Batch, MigrateError> {
        // the batch is read first, so the run finishes it before it stops
        let batch = self.inner.next_batch(slice, after, ctx).await?;
        if self.seen.fetch_add(1, Ordering::SeqCst) + 1 >= self.after {
            self.cancel.cancel();
        }
        Ok(batch)
    }

    /// Classify as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `record` - The stored record
    /// * `mode` - `Run` or `Check`
    /// * `ctx` - The backends, settings, and mode of this run
    async fn process(
        &self,
        record: &Record,
        mode: RunMode,
        ctx: &RunContext,
    ) -> Result<Outcome, MigrateError> {
        // delegate
        self.inner.process(record, mode, ctx).await
    }

    /// Write as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `record` - The converted record
    /// * `ctx` - The backends, settings, and mode of this run
    async fn write(&self, record: &Record, ctx: &RunContext) -> Result<(), MigrateError> {
        // delegate
        self.inner.write(record, ctx).await
    }

    /// Finish as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `ctx` - The backends, settings, and mode of this run
    async fn finish(&self, ctx: &RunContext) -> Result<(), MigrateError> {
        // delegate
        self.inner.finish(ctx).await
    }

    /// Verify as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `record` - The stored record
    /// * `ctx` - The backends, settings, and mode of this run
    async fn verify(&self, record: &Record, ctx: &RunContext) -> Result<(), IssueReason> {
        // delegate
        self.inner.verify(record, ctx).await
    }

    /// Wait as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `counts` - The counts the check's classification found
    /// * `ctx` - The backends, settings, and mode of this run
    async fn check_wait(
        &self,
        counts: &PassCounts,
        ctx: &RunContext,
    ) -> Result<Option<CheckWait>, MigrateError> {
        // delegate
        self.inner.check_wait(counts, ctx).await
    }
}

/// A runner that breaks the writer lease after its first batch
struct Breaker {
    /// The wrapped migration
    inner: &'static dyn MigrationRunner,
    /// The world, for the Redis pool
    redis: bb8_redis::bb8::Pool<bb8_redis::RedisConnectionManager>,
    /// The namespace
    ns: String,
    /// How many batches were read
    seen: AtomicUsize,
}
#[async_trait]
impl MigrationRunner for Breaker {
    /// Split the family as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `n` - How many slices to make
    /// * `ctx` - The backends, settings, and mode of this run
    async fn slices(&self, n: u32, ctx: &RunContext) -> Result<Vec<Slice>, MigrateError> {
        self.inner.slices(n, ctx).await
    }
    /// Read a batch as the wrapped migration does, then break the run's lease after the first
    ///
    /// # Arguments
    ///
    /// * `slice` - The slice to read
    /// * `after` - The last position done, or the slice's start
    /// * `ctx` - The backends, settings, and mode of this run
    async fn next_batch(
        &self,
        slice: &Slice,
        after: &serde_json::Value,
        ctx: &RunContext,
    ) -> Result<Batch, MigrateError> {
        let batch = self.inner.next_batch(slice, after, ctx).await?;
        // another writer breaks the lease after the first batch
        if self.seen.fetch_add(1, Ordering::SeqCst) == 0 {
            let _taken = WriterLease::acquire(&self.redis, &self.ns, "manual", true)
                .await
                .unwrap();
            std::mem::forget(_taken);
        }
        Ok(batch)
    }
    /// Classify as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `record` - The stored record
    /// * `mode` - `Run` or `Check`
    /// * `ctx` - The backends, settings, and mode of this run
    async fn process(
        &self,
        record: &Record,
        mode: RunMode,
        ctx: &RunContext,
    ) -> Result<Outcome, MigrateError> {
        self.inner.process(record, mode, ctx).await
    }
    /// Write as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `record` - The converted record
    /// * `ctx` - The backends, settings, and mode of this run
    async fn write(&self, record: &Record, ctx: &RunContext) -> Result<(), MigrateError> {
        self.inner.write(record, ctx).await
    }
    /// Finish as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `ctx` - The backends, settings, and mode of this run
    async fn finish(&self, ctx: &RunContext) -> Result<(), MigrateError> {
        self.inner.finish(ctx).await
    }
}

/// Run a pass of a fixture migration to the end, with its lease released after
///
/// # Arguments
///
/// * `world` - The test's world
/// * `def` - The migration
/// * `opts` - How to run it
/// * `batch_rows` - The rows of each batch
async fn run(
    world: &World,
    def: &MigrationDef,
    opts: &RunOptions,
    batch_rows: usize,
) -> PassOutcome {
    // a lease of the test's own, so a stale one never blocks it
    let lease = world.lease("test").await;
    let ctx = world.ctx(RunMode::Run, batch_rows);
    let out = run_pass(
        def.runner,
        &World::spec(def),
        opts,
        &ctx,
        &world.store,
        &lease,
    )
    .await
    .expect("the pass runs");
    lease.release().await;
    out
}

/// Options for `slices` slices
///
/// # Arguments
///
/// * `slices` - How many slices
fn slices(slices: u32) -> RunOptions {
    // no object or reason filter
    RunOptions {
        slices,
        ..RunOptions::default()
    }
}

/// A layout 1 body that parses as neither layout, with a distinct code per `n`
///
/// # Arguments
///
/// * `n` - Which distinct group the body belongs to
fn poison(n: usize) -> String {
    // the fixture turns this into the reason `invalid_value:poison<n>`
    format!("poison:{n}")
}

/// A checkpoint is written after every committed batch, holding the position, the counts, and the
/// lease's fencing token, and a later run resumes from it
#[tokio::test]
async fn a_checkpoint_follows_every_committed_batch() {
    // seed 300 rows and cancel after three batches of 50
    let world = World::new("mr01").await;
    world.seed_valid("a", 300).await;
    let cancel = CancellationToken::new();
    let runner = CancelAfter::new(&fixture::THEME, 3, cancel.clone());
    let mut ctx = world.ctx(RunMode::Run, 50);
    ctx.cancel = cancel;
    let lease = world.lease("test").await;
    let out = run_pass(
        &runner,
        &World::spec(&fixture::THEME),
        &slices(1),
        &ctx,
        &world.store,
        &lease,
    )
    .await
    .expect("the pass runs");
    assert!(out.interrupted);
    // the pass is still running, and its worker's checkpoint covers the three batches
    let row = world
        .store
        .pass_row("fixture")
        .await
        .unwrap()
        .expect("a pass row");
    assert_eq!(row.status, MigrationRunStatus::Running);
    let (workers, _) = world.store.rows("fixture", &row.attempt).await.unwrap();
    assert_eq!(workers.len(), 1);
    let checkpoint = &workers[0].checkpoint;
    assert_eq!(
        (checkpoint.counts.read, checkpoint.counts.written),
        (150, 150)
    );
    assert!(!checkpoint.done);
    assert!(checkpoint.position.is_i64(), "{:?}", checkpoint.position);
    assert_eq!(workers[0].lease.as_deref(), Some(lease.token()));
    lease.release().await;
    // a second run resumes the same attempt and finishes
    let out = run(&world, &fixture::THEME, &slices(1), 50).await;
    assert!(out.resumed);
    assert_eq!(out.attempt, row.attempt);
    assert_eq!(out.status, MigrationRunStatus::Done);
    assert_eq!(out.counts.read, 300);
    assert!(world.rows().await.iter().all(|(_, _, layout)| *layout == 2));
    assert_eq!(world.marker().await.as_deref(), Some("2"));
    world.drop_keyspace().await;
}

/// A checkpoint recorded for a different fused chain restarts the pass from the beginning, with
/// the same final data as an uninterrupted run
#[tokio::test]
async fn another_chain_restarts_the_pass() {
    // interrupt a run of the longer chain after one batch
    let world = World::new("mr05").await;
    world.seed_valid("a", 200).await;
    let cancel = CancellationToken::new();
    let runner = CancelAfter::new(&fixture::CHAIN, 1, cancel.clone());
    let mut ctx = world.ctx(RunMode::Run, 50);
    ctx.cancel = cancel;
    let lease = world.lease("test").await;
    let first = run_pass(
        &runner,
        &World::spec(&fixture::CHAIN),
        &slices(1),
        &ctx,
        &world.store,
        &lease,
    )
    .await
    .expect("the pass runs");
    lease.release().await;
    assert!(first.interrupted);
    // the same pass run as the shorter chain doesn't resume: it is a new attempt over every row
    let second = run(&world, &fixture::THEME, &slices(1), 50).await;
    assert!(!second.resumed);
    assert_ne!(second.attempt, first.attempt);
    assert_eq!(second.counts.read, 200);
    assert_eq!(second.status, MigrationRunStatus::Done);
    assert!(
        world
            .rows()
            .await
            .iter()
            .all(|(_, body, layout)| *layout == 2 && body.contains("Dark"))
    );
    world.drop_keyspace().await;
}

/// Unrecognized objects and rejected writes are left untouched and recorded as groups keyed by
/// pass, transform, reason, and worker
#[tokio::test]
async fn bad_objects_are_left_untouched_and_recorded() {
    // two kinds of unreadable body, a rejected write, and a good row
    let world = World::new("mr06").await;
    world
        .seed(&[
            ("bad-json".into(), "{not json".into()),
            ("bad-type".into(), "{\"name\":5}".into()),
            ("bad-type2".into(), "{\"name\":6}".into()),
            ("reject-me".into(), "{\"name\":\"r\"}".into()),
            ("good".into(), "{\"name\":\"g\"}".into()),
        ])
        .await;
    let out = run(&world, &fixture::THEME, &slices(1), 100).await;
    assert_eq!(out.status, MigrationRunStatus::DoneWithIssues);
    // the groups carry their class and counts
    let by_reason = |reason: &str| {
        out.groups
            .iter()
            .find(|group| group.reason == reason)
            .unwrap_or_else(|| panic!("no group {reason}: {:?}", out.groups))
            .clone()
    };
    assert_eq!(by_reason("malformed_json:body").count, 1);
    assert_eq!(by_reason("bad_type:body.name").count, 2);
    let rejected = by_reason("write_failed:rejected");
    assert_eq!(rejected.count, 1);
    assert_eq!(rejected.class.as_str(), "error");
    // the stored rows have their keys, the attempt, and the times
    let (_, issues) = world.store.rows("fixture", &out.attempt).await.unwrap();
    assert_eq!(issues.len(), 3);
    for issue in &issues {
        assert_eq!(issue.transform, "fixture-theme");
        assert_eq!(issue.worker, "w00");
        assert_eq!(issue.attempt, out.attempt);
        assert!(issue.first_seen.is_some() && issue.last_seen.is_some());
    }
    // the bad rows are untouched, and the good row converted
    let rows = world.rows().await;
    let body = |id: &str| rows.iter().find(|row| row.0 == id).unwrap().1.clone();
    assert_eq!(body("bad-json"), "{not json");
    assert_eq!(body("reject-me"), "{\"name\":\"r\"}");
    assert!(body("good").contains("Dark"));
    world.drop_keyspace().await;
}

/// `--objects` processes only the named objects, crediting the group whose samples hold a fixed
/// one and crediting no group for a key in none
#[tokio::test]
async fn objects_runs_credit_the_group_that_held_them() {
    // three objects in one group, then two of them fixed by hand and one plain row added
    let world = World::new("mr07").await;
    world
        .seed(&[
            ("x1".into(), "{\"name\":1}".into()),
            ("x2".into(), "{\"name\":2}".into()),
            ("x3".into(), "{\"name\":3}".into()),
        ])
        .await;
    let first = run(&world, &fixture::THEME, &slices(1), 100).await;
    assert_eq!(first.groups.len(), 1);
    assert_eq!(first.groups[0].count, 3);
    world
        .seed(&[("later".into(), "{\"name\":\"l\"}".into())])
        .await;
    for id in ["x1", "x2"] {
        world
            .session()
            .query_unpaged(
                format!(
                    "UPDATE {}.mig_fixture SET body = '{{\"name\":\"fixed\"}}' WHERE id = '{id}'",
                    world.ns
                ),
                (),
            )
            .await
            .unwrap();
    }
    // name one fixed object and the unflagged row
    let lease = world.lease("test").await;
    let ctx = world.ctx(RunMode::Run, 100);
    let opts = RunOptions {
        slices: 1,
        objects: vec!["x1".into(), "later".into()],
        reason: None,
    };
    let out = run_pass(
        fixture::THEME.runner,
        &World::spec(&fixture::THEME),
        &opts,
        &ctx,
        &world.store,
        &lease,
    )
    .await
    .expect("the objects run");
    lease.release().await;
    // the fixed object left its group; the other fixed one wasn't named, so it is untouched
    let group = &out.groups[0];
    assert_eq!((group.count, group.resolved), (2, 1));
    assert!(!group.samples.contains("x1") && group.samples.contains("x2"));
    let rows = world.rows().await;
    let layout = |id: &str| rows.iter().find(|row| row.0 == id).unwrap().2;
    assert_eq!((layout("x1"), layout("x2"), layout("later")), (2, 1, 2));
    // the pass wasn't ended by the run
    assert_eq!(
        world.status("fixture").await,
        Some(MigrationRunStatus::DoneWithIssues)
    );
    world.drop_keyspace().await;
}

/// `migrate verify` loads every object through the migration's own deserialization
#[tokio::test]
async fn verify_loads_every_migrated_object() {
    // a migrated table verifies clean, and a stale row is reported with its reason
    let world = World::new("mr08").await;
    world.seed_valid("a", 20).await;
    run(&world, &fixture::THEME, &slices(2), 10).await;
    let ctx = world.ctx(RunMode::Check, 10);
    let clean = verify_pass(fixture::THEME.runner, &slices(2), &ctx)
        .await
        .unwrap();
    assert_eq!((clean.checked, clean.failures.len()), (20, 0));
    world
        .seed(&[("stale".into(), "{\"name\":\"s\"}".into())])
        .await;
    let dirty = verify_pass(fixture::THEME.runner, &slices(2), &ctx)
        .await
        .unwrap();
    assert_eq!(dirty.checked, 21);
    assert_eq!(dirty.failures.get("missing_field:body.theme"), Some(&1));
    world.drop_keyspace().await;
}

/// A run whose every group holds at most `group_max` objects ends `Done` with issues and
/// advances the marker; a larger group ends `Failed` without advancing it
#[tokio::test]
async fn groups_within_the_limit_finish_and_larger_ones_fail() {
    // five bad objects in one group is exactly the limit
    let world = World::new("mr10a").await;
    world.seed_valid("a", 40).await;
    world
        .seed(
            &(0..5)
                .map(|n| (format!("bad-{n}"), "{\"name\":5}".to_owned()))
                .collect::<Vec<_>>(),
        )
        .await;
    let out = run(&world, &fixture::THEME, &slices(1), 100).await;
    assert_eq!(out.status, MigrationRunStatus::DoneWithIssues);
    assert_eq!(world.marker().await.as_deref(), Some("2"));
    world.drop_keyspace().await;
    // six is over it, so the marker stays unset
    let world = World::new("mr10b").await;
    world.seed_valid("a", 40).await;
    world
        .seed(
            &(0..6)
                .map(|n| (format!("bad-{n}"), "{\"name\":5}".to_owned()))
                .collect::<Vec<_>>(),
        )
        .await;
    let out = run(&world, &fixture::THEME, &slices(1), 100).await;
    assert_eq!(out.status, MigrationRunStatus::Failed);
    assert_eq!(world.marker().await, None);
    world.drop_keyspace().await;
}

/// With a group limit of 0 a single issue fails the run, and the catalog's limit for the
/// strictest family is honored
#[tokio::test]
async fn a_zero_limit_fails_on_one_issue() {
    // one bad object under the strict migration
    let world = World::new("mr32").await;
    world.seed_valid("a", 10).await;
    world.seed(&[("bad".into(), "{\"name\":5}".into())]).await;
    let out = run(&world, &fixture::STRICT, &slices(1), 100).await;
    assert_eq!(out.status, MigrationRunStatus::Failed);
    assert_eq!(out.groups[0].count, 1);
    assert_eq!(world.marker().await, None);
    world.drop_keyspace().await;
}

/// Each worker writes only rows under its own name, and a run interrupted and resumed ends with the
/// counts an uninterrupted one has
#[tokio::test]
async fn workers_write_only_their_own_rows_and_a_resumed_run_ends_with_the_same_counts() {
    // a table big enough that every one of four workers has rows, with bad rows among them
    let world = World::new("mr28").await;
    world.seed_valid("a", 400).await;
    let bad = (0..20)
        .map(|n| (format!("bad-{n}"), "{\"name\":5}".to_owned()))
        .collect::<Vec<_>>();
    world.seed(&bad).await;
    // an interrupted run
    let cancel = CancellationToken::new();
    let runner = CancelAfter::new(&fixture::THEME, 16, cancel.clone());
    let mut ctx = world.ctx(RunMode::Run, 20);
    ctx.cancel = cancel;
    let lease = world.lease("test").await;
    let mut spec = World::spec(&fixture::THEME);
    spec.group_max = 100;
    let first = run_pass(&runner, &spec, &slices(4), &ctx, &world.store, &lease)
        .await
        .unwrap();
    lease.release().await;
    assert!(first.interrupted);
    let (workers, issues) = world.store.rows("fixture", &first.attempt).await.unwrap();
    assert!(workers.iter().all(|worker| worker.worker.starts_with('w')));
    assert!(issues.iter().all(|issue| issue.worker.starts_with('w')));
    // the last run finishes the pass
    let lease = world.lease("test").await;
    let ctx = world.ctx(RunMode::Run, 20);
    let second = run_pass(
        fixture::THEME.runner,
        &spec,
        &slices(4),
        &ctx,
        &world.store,
        &lease,
    )
    .await
    .unwrap();
    lease.release().await;
    assert_eq!(second.attempt, first.attempt);
    // the resumed run ends with the counts an uninterrupted run has
    let control = World::new("mr28c").await;
    control.seed_valid("a", 400).await;
    control.seed(&bad).await;
    let mut control_spec = World::spec(&fixture::THEME);
    control_spec.group_max = 100;
    let lease = control.lease("test").await;
    let cctx = control.ctx(RunMode::Run, 20);
    let uninterrupted = run_pass(
        fixture::THEME.runner,
        &control_spec,
        &slices(4),
        &cctx,
        &control.store,
        &lease,
    )
    .await
    .unwrap();
    lease.release().await;
    assert_eq!(second.counts, uninterrupted.counts);
    assert_eq!(
        second
            .groups
            .iter()
            .map(|group| (group.reason.clone(), group.count))
            .collect::<Vec<_>>(),
        uninterrupted
            .groups
            .iter()
            .map(|group| (group.reason.clone(), group.count))
            .collect::<Vec<_>>()
    );
    world.drop_keyspace().await;
    control.drop_keyspace().await;
}

/// A batch never writes an issue row's resolved count or its acceptance, which only `--objects`,
/// `--reason`, and the run's own acceptance marking write
#[tokio::test]
async fn batches_never_write_resolved_counts_or_acceptance() {
    // one worker, interrupted after it has recorded a group
    let world = World::new("mr28b").await;
    world.seed_valid("a", 400).await;
    let bad = |prefix: &str, count: usize| {
        (0..count)
            .map(|n| (format!("{prefix}-{n}"), "{\"name\":5}".to_owned()))
            .collect::<Vec<_>>()
    };
    world.seed(&bad("bad", 20)).await;
    let cancel = CancellationToken::new();
    let runner = CancelAfter::new(&fixture::THEME, 10, cancel.clone());
    let mut ctx = world.ctx(RunMode::Run, 20);
    ctx.cancel = cancel;
    let lease = world.lease("test").await;
    let mut spec = World::spec(&fixture::THEME);
    spec.group_max = 1000;
    let first = run_pass(&runner, &spec, &slices(1), &ctx, &world.store, &lease)
        .await
        .unwrap();
    lease.release().await;
    assert!(first.interrupted);
    let (_, issues) = world.store.rows("fixture", &first.attempt).await.unwrap();
    let flagged = issues.first().expect("an issue row").clone();
    // a hand edit of the two things a batch must leave alone, and more bad rows of the same group
    world
        .session()
        .query_unpaged(
            format!(
                "UPDATE {}.schema_migration_issues SET resolved_count = 7, accepted = true \
                 WHERE migration = 'fixture' AND transform = '{}' AND reason = '{}' AND worker = '{}'",
                world.ns, flagged.transform, flagged.reason, flagged.worker
            ),
            (),
        )
        .await
        .unwrap();
    world.seed(&bad("late", 200)).await;
    // a resumed run, interrupted again, rewrites the group's count in its batches
    let cancel = CancellationToken::new();
    let runner = CancelAfter::new(&fixture::THEME, 3, cancel.clone());
    let mut ctx = world.ctx(RunMode::Run, 20);
    ctx.cancel = cancel;
    let lease = world.lease("test").await;
    let again = run_pass(&runner, &spec, &slices(1), &ctx, &world.store, &lease)
        .await
        .unwrap();
    lease.release().await;
    assert_eq!(again.attempt, first.attempt);
    let (_, issues) = world.store.rows("fixture", &again.attempt).await.unwrap();
    let after = issues
        .iter()
        .find(|row| (&row.transform, &row.reason) == (&flagged.transform, &flagged.reason))
        .unwrap();
    assert!(
        after.count > flagged.count,
        "{} vs {}",
        after.count,
        flagged.count
    );
    assert_eq!((after.resolved_count, after.accepted), (7, true));
    world.drop_keyspace().await;
}

/// A group of 50 objects found by one worker keeps exactly 20 sample keys and a count of 50
#[tokio::test]
async fn a_group_keeps_twenty_samples_and_its_whole_count() {
    // fifty bad objects, one worker, and a limit that lets them finish
    let world = World::new("mr29").await;
    let bad = (0..50)
        .map(|n| (format!("bad-{n:02}"), "{\"name\":5}".to_owned()))
        .collect::<Vec<_>>();
    world.seed(&bad).await;
    let lease = world.lease("test").await;
    let mut spec = World::spec(&fixture::THEME);
    spec.group_max = 100;
    let ctx = world.ctx(RunMode::Run, 100);
    let out = run_pass(
        fixture::THEME.runner,
        &spec,
        &slices(1),
        &ctx,
        &world.store,
        &lease,
    )
    .await
    .unwrap();
    lease.release().await;
    let (_, issues) = world.store.rows("fixture", &out.attempt).await.unwrap();
    assert_eq!(
        (issues.len(), issues[0].count, issues[0].samples.len()),
        (1, 50, 20)
    );
    world.drop_keyspace().await;
}

/// The ids of rows whose tokens fall in each of the slices of the ring
///
/// # Arguments
///
/// * `world` - The test's world
/// * `slices` - How many slices
/// * `per_slice` - How many ids to pick in each
async fn ids_per_slice(world: &World, slices: u32, per_slice: usize) -> Vec<Vec<String>> {
    // read the token of many candidate ids
    let candidates = (0..400)
        .map(|n| (format!("c-{n}"), "{\"name\":5}".to_owned()))
        .collect::<Vec<_>>();
    world.seed(&candidates).await;
    let rows = world
        .session()
        .query_unpaged(
            format!("SELECT token(id), id FROM {}.mig_fixture", world.ns),
            (),
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap();
    let tokens = rows
        .rows::<(i64, String)>()
        .unwrap()
        .map(|row| row.unwrap())
        .filter(|(_, id)| id.starts_with("c-"))
        .collect::<Vec<_>>();
    // keep `per_slice` per slice and drop the rest
    let ring = ring_slices(slices, &[]);
    let mut picked = vec![Vec::new(); ring.len()];
    for (token, id) in &tokens {
        let at = ring
            .iter()
            .position(|slice| {
                slice.start.as_i64().unwrap() <= *token && *token <= slice.end.as_i64().unwrap()
            })
            .unwrap();
        if picked[at].len() < per_slice {
            picked[at].push(id.clone());
        } else {
            world
                .session()
                .query_unpaged(
                    format!("DELETE FROM {}.mig_fixture WHERE id = '{id}'", world.ns),
                    (),
                )
                .await
                .unwrap();
        }
    }
    picked
}

/// A worker whose own group goes over the limit writes its rows and checkpoint and stops, the
/// others stop at their next batch, and the run ends `Failed` without advancing the marker
#[tokio::test]
async fn one_workers_group_over_the_limit_stops_the_run() {
    // four workers, many valid rows, and enough bad ones that some worker's own group reaches six
    let world = World::new("mr30").await;
    world.seed_valid("a", 2000).await;
    let bad = (0..60)
        .map(|n| (format!("bad-{n}"), "{\"name\":5}".to_owned()))
        .collect::<Vec<_>>();
    world.seed(&bad).await;
    let out = run(&world, &fixture::THEME, &slices(4), 25).await;
    assert_eq!(out.status, MigrationRunStatus::Failed);
    assert_eq!(world.marker().await, None);
    // the stopping worker's rows are written, and at least one worker never reached its end
    let (workers, issues) = world.store.rows("fixture", &out.attempt).await.unwrap();
    assert!(issues.iter().any(|issue| issue.count >= 6));
    assert!(workers.iter().any(|worker| !worker.checkpoint.done));
    assert!(
        out.counts.read < 2060,
        "every worker read everything: {:?}",
        out.counts
    );
    world.drop_keyspace().await;
}

/// A group of 8 spread over four workers, two each, fails the run at the end-of-run sum although no
/// worker's own count exceeds the limit
#[tokio::test]
async fn a_group_spread_over_workers_fails_at_the_end() {
    // two bad objects in each of four slices
    let world = World::new("mr31").await;
    world.seed_valid("a", 100).await;
    let picked = ids_per_slice(&world, 4, 2).await;
    assert!(picked.iter().all(|ids| ids.len() == 2), "{picked:?}");
    let out = run(&world, &fixture::THEME, &slices(4), 50).await;
    assert_eq!(out.status, MigrationRunStatus::Failed);
    assert_eq!(out.groups.iter().map(|group| group.count).sum::<u64>(), 8);
    let (_, issues) = world.store.rows("fixture", &out.attempt).await.unwrap();
    assert!(issues.iter().all(|issue| issue.count == 2), "{issues:?}");
    world.drop_keyspace().await;
}

/// `--reason` after the group's objects are fixed writes what converts, leaves other groups
/// untouched, recounts every group seen, and ends the pass
#[tokio::test]
async fn a_reason_run_rewrites_what_now_converts_and_recounts() {
    // two groups, then the first group's objects fixed
    let world = World::new("mr34").await;
    world.seed_valid("a", 10).await;
    world
        .seed(&[
            ("p1".into(), "{\"nme\":1}".into()),
            ("p2".into(), "{\"nme\":2}".into()),
            ("q1".into(), "{\"name\":5}".into()),
        ])
        .await;
    let first = run(&world, &fixture::THEME, &slices(1), 100).await;
    assert_eq!(first.status, MigrationRunStatus::DoneWithIssues);
    for id in ["p1", "p2"] {
        world
            .session()
            .query_unpaged(
                format!(
                    "UPDATE {}.mig_fixture SET body = '{{\"name\":\"ok\"}}' WHERE id = '{id}'",
                    world.ns
                ),
                (),
            )
            .await
            .unwrap();
    }
    let opts = RunOptions {
        slices: 1,
        reason: Some("missing_field:body.name".into()),
        ..RunOptions::default()
    };
    let lease = world.lease("test").await;
    let ctx = world.ctx(RunMode::Run, 100);
    let out = run_pass(
        fixture::THEME.runner,
        &World::spec(&fixture::THEME),
        &opts,
        &ctx,
        &world.store,
        &lease,
    )
    .await
    .unwrap();
    lease.release().await;
    // the fixed group is empty with its objects counted as resolved; the other is untouched
    let missing = out
        .groups
        .iter()
        .find(|group| group.reason == "missing_field:body.name")
        .unwrap();
    assert_eq!((missing.count, missing.resolved), (0, 2));
    let bad_type = out
        .groups
        .iter()
        .find(|group| group.reason == "bad_type:body.name")
        .unwrap();
    assert_eq!(bad_type.count, 1);
    let rows = world.rows().await;
    assert!(
        rows.iter()
            .filter(|row| row.0.starts_with('p'))
            .all(|row| row.2 == 2)
    );
    assert_eq!(
        rows.iter().find(|row| row.0 == "q1").unwrap().1,
        "{\"name\":5}"
    );
    // the pass ended, within its limit, and the marker advanced
    assert_eq!(out.status, MigrationRunStatus::DoneWithIssues);
    assert_eq!(world.marker().await.as_deref(), Some("2"));
    world.drop_keyspace().await;
}

/// A check over 30 groups lists the ten largest with ties broken by code, carries no sample keys,
/// lists the rows it would rewrite, and its JSON fits in 4 KiB
#[tokio::test]
async fn a_check_report_lists_ten_groups_and_stays_small() {
    // thirty groups with counts 1 to 30, two of them tied
    let world = World::new("mr35").await;
    world.seed_valid("a", 50).await;
    let mut rows = Vec::new();
    for group in 0..30_usize {
        let size = if group == 29 { 29 } else { group + 1 };
        for member in 0..size {
            rows.push((format!("g{group:02}-{member:02}"), poison(group)));
        }
    }
    world.seed(&rows).await;
    let ctx = world.ctx(RunMode::Check, 50);
    let run = check_pass(
        fixture::THEME.runner,
        &World::spec(&fixture::THEME),
        &slices(4),
        &ctx,
    )
    .await
    .unwrap();
    let report = &run.report;
    assert_eq!(report.reasons.len(), 10);
    assert_eq!(report.more, 20);
    // sorted by count, ties by code: the two groups of 29 are poison28 and poison29
    let codes = report
        .reasons
        .iter()
        .map(|reason| reason.code.as_str())
        .take(3)
        .collect::<Vec<_>>();
    assert_eq!(
        codes,
        [
            "invalid_value:poison28",
            "invalid_value:poison29",
            "invalid_value:poison27"
        ]
    );
    assert_eq!(report.counts.migratable, 50);
    assert_eq!(report.rows.get("fixture"), Some(&50));
    assert!(report.estimated_seconds.is_some());
    let json = serde_json::to_string(report).unwrap();
    assert!(json.len() < 4096, "{} bytes", json.len());
    assert!(!json.contains("samples"));
    // a check writes nothing: the rows are as seeded
    assert!(
        world
            .rows()
            .await
            .iter()
            .all(|row| row.2 == 1 || row.1.starts_with("poison"))
    );
    world.drop_keyspace().await;
}

/// While one writer holds the lease another is refused naming the holder, and a worker whose lease
/// was broken stops at its next batch
#[tokio::test]
async fn a_broken_lease_stops_the_worker_at_its_next_batch() {
    // a held lease refuses a second holder by name
    let world = World::new("mr40").await;
    world.seed_valid("a", 200).await;
    let held = world.lease("job-a").await;
    let refused = WriterLease::acquire(&world.backends.redis, &world.ns, "manual", false).await;
    let message = refused.err().expect("refused").to_string();
    assert!(message.contains("job-a"), "{message}");
    // a run under the held lease is stopped when another writer breaks it after one batch
    let breaker = Breaker {
        inner: fixture::THEME.runner,
        redis: world.backends.redis.clone(),
        ns: world.ns.clone(),
        seen: AtomicUsize::new(0),
    };
    let ctx = world.ctx(RunMode::Run, 20);
    let result = run_pass(
        &breaker,
        &World::spec(&fixture::THEME),
        &slices(1),
        &ctx,
        &world.store,
        &held,
    )
    .await;
    assert!(
        matches!(result, Err(MigrateError::LeaseLost)),
        "{:?}",
        result.map(|out| out.status)
    );
    // the stopped worker wrote nothing after the break: only the first batch's rows converted
    let converted = world.rows().await.iter().filter(|row| row.2 == 2).count();
    assert_eq!(converted, 20);
    world.drop_keyspace().await;
}

/// Running a migration again after its pass ended starts a new attempt over every object, and a
/// pass restarted with fewer slices counts no row of the old slices
#[tokio::test]
async fn a_finished_pass_starts_a_new_attempt() {
    // a finished pass, then every row put back in layout 1
    let world = World::new("mr41").await;
    world.seed_valid("a", 40).await;
    let first = run(&world, &fixture::THEME, &slices(8), 10).await;
    assert_eq!(first.status, MigrationRunStatus::Done);
    world
        .session()
        .query_unpaged(
            format!("UPDATE {0}.mig_fixture SET body = '{{\"name\":\"back\"}}', layout = 1 WHERE id IN ({1})", world.ns,
                (0..40).map(|n| format!("'a-{n}'")).collect::<Vec<_>>().join(",")),
            (),
        )
        .await
        .unwrap();
    // the second run is a new attempt, and processes every object
    let second = run(&world, &fixture::THEME, &slices(4), 10).await;
    assert!(!second.resumed);
    assert_ne!(second.attempt, first.attempt);
    assert_eq!(second.counts.read, 40);
    assert_eq!(second.counts.written, 40);
    // no row of the earlier attempt's eight workers is counted
    let (workers, _) = world.store.rows("fixture", &second.attempt).await.unwrap();
    let names = workers
        .iter()
        .map(|worker| worker.worker.clone())
        .collect::<BTreeSet<_>>();
    let expected = ["w00", "w01", "w02", "w03"]
        .into_iter()
        .map(String::from)
        .collect::<BTreeSet<_>>();
    assert_eq!(names, expected);
    // the earlier attempt's rows are deleted from the pass's partition, not merely ignored
    let raw = world
        .session()
        .query_unpaged(
            format!(
                "SELECT worker, attempt FROM {}.schema_migration_issues WHERE migration = 'fixture'",
                world.ns
            ),
            (),
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap();
    let stored = raw
        .rows::<(String, String)>()
        .unwrap()
        .map(|row| row.unwrap())
        .collect::<Vec<_>>();
    assert_eq!(stored.len(), 4, "{stored:?}");
    assert!(
        stored.iter().all(|(_, attempt)| *attempt == second.attempt),
        "{stored:?}"
    );
    world.drop_keyspace().await;
}

/// A worker's batch at the 100-group cap with long sample keys is accepted whole, a worker that
/// reaches 101 distinct groups stops the run as a systematic failure, and 300 groups across
/// workers checkpoint every batch
#[tokio::test]
async fn many_groups_checkpoint_in_one_partition_batch() {
    // 100 groups of 20 objects with 256-byte keys in one batch
    let world = World::new("mr42a").await;
    let key = |group: usize, member: usize| format!("{group:03}-{member:02}-{}", "k".repeat(250));
    let mut rows = Vec::new();
    for group in 0..100 {
        for member in 0..20 {
            rows.push((key(group, member), poison(group)));
        }
    }
    world.seed(&rows).await;
    let mut spec = World::spec(&fixture::THEME);
    spec.group_max = 1000;
    let lease = world.lease("test").await;
    let ctx = world.ctx(RunMode::Run, 5000);
    let out = run_pass(
        fixture::THEME.runner,
        &spec,
        &slices(1),
        &ctx,
        &world.store,
        &lease,
    )
    .await
    .unwrap();
    lease.release().await;
    assert_eq!(out.groups.len(), 100);
    // a batch of this shape holds 100 groups of 20 samples of about 256 bytes: about 512,000
    // bytes, well under the 1 MiB at which a multi-partition batch is refused
    assert!(
        out.largest_batch > 400_000 && out.largest_batch < 1_000_000,
        "{}",
        out.largest_batch
    );
    world.drop_keyspace().await;
    // 101 distinct groups in one worker fail the run as systematic
    let world = World::new("mr42b").await;
    world
        .seed(
            &(0..101)
                .map(|n| (format!("g{n}"), poison(n)))
                .collect::<Vec<_>>(),
        )
        .await;
    let mut spec = World::spec(&fixture::THEME);
    spec.group_max = 1000;
    let lease = world.lease("test").await;
    let ctx = world.ctx(RunMode::Run, 200);
    let out = run_pass(
        fixture::THEME.runner,
        &spec,
        &slices(1),
        &ctx,
        &world.store,
        &lease,
    )
    .await
    .unwrap();
    lease.release().await;
    assert_eq!(out.status, MigrationRunStatus::Failed);
    assert!(
        out.failure
            .as_deref()
            .is_some_and(|message| message.contains("distinct issue groups"))
    );
    world.drop_keyspace().await;
    // 300 groups over four workers checkpoint every batch
    let world = World::new("mr42c").await;
    world
        .seed(
            &(0..300)
                .map(|n| (format!("g{n}"), poison(n)))
                .collect::<Vec<_>>(),
        )
        .await;
    let mut spec = World::spec(&fixture::THEME);
    spec.group_max = 1000;
    let lease = world.lease("test").await;
    let ctx = world.ctx(RunMode::Run, 10);
    let out = run_pass(
        fixture::THEME.runner,
        &spec,
        &slices(4),
        &ctx,
        &world.store,
        &lease,
    )
    .await
    .unwrap();
    lease.release().await;
    assert_eq!(out.groups.len(), 300);
    assert_eq!(out.status, MigrationRunStatus::DoneWithIssues);
    world.drop_keyspace().await;
}

/// For 1, 2, 3, and 8 slices the ranges are contiguous and disjoint, cover the ring, and are
/// processed in token order within each slice
#[tokio::test]
async fn slices_cover_the_ring_in_order() {
    // the ring splits contiguously for each slice count
    for n in [1_u32, 2, 3, 8] {
        let ring = ring_slices(n, &[]);
        assert_eq!(u32::try_from(ring.len()), Ok(n));
        assert_eq!(ring[0].start.as_i64(), Some(i64::MIN));
        assert_eq!(ring[ring.len() - 1].end.as_i64(), Some(i64::MAX));
        for pair in ring.windows(2) {
            assert_eq!(
                i128::from(pair[1].start.as_i64().unwrap()),
                i128::from(pair[0].end.as_i64().unwrap()) + 1
            );
        }
    }
    // every row of a table is read exactly once, in ascending token order within its slice
    let world = World::new("mr43").await;
    world.seed_valid("a", 300).await;
    let ctx = world.ctx(RunMode::Check, 7);
    for n in [1_u32, 2, 3, 8] {
        let slices = fixture::THEME.runner.slices(n, &ctx).await.unwrap();
        let mut total = 0;
        for slice in &slices {
            let mut position = serde_json::Value::Null;
            let mut last = i64::MIN;
            loop {
                let batch = fixture::THEME
                    .runner
                    .next_batch(slice, &position, &ctx)
                    .await
                    .unwrap();
                total += batch.records.len();
                // the position after a batch never moves backwards
                if let Some(next) = batch.next.as_i64() {
                    assert!(next >= last, "{n} slices: {next} < {last}");
                    last = next;
                }
                position = batch.next;
                if batch.done {
                    break;
                }
            }
        }
        assert_eq!(total, 300, "{n} slices");
    }
    world.drop_keyspace().await;
}

/// Every migration in the registry that thoradm can run goes through the runner: interrupted
/// mid-run it resumes, it reports per-pass counts, and an unreadable object becomes a group row
/// with its reason code
#[tokio::test]
async fn every_registered_migration_runs_through_the_runner() {
    // the hand-written migration has no declared layout and still resumes and flags issues
    let world = World::new("mr23").await;
    let rows = (0..30)
        .map(|n| {
            format!(
                "INSERT INTO {}.{} (id, body) VALUES ('p{n}', '{}')",
                world.ns,
                fixture::PLAIN_TABLE,
                if n == 7 { "garbage" } else { "old" }
            )
        })
        .collect::<Vec<_>>();
    for cql in rows {
        world.session().query_unpaged(cql, ()).await.unwrap();
    }
    let cancel = CancellationToken::new();
    let runner = CancelAfter::new(&fixture::PLAIN, 1, cancel.clone());
    let mut ctx = world.ctx(RunMode::Run, 10);
    ctx.cancel = cancel;
    let lease = world.lease("test").await;
    let spec = World::spec(&fixture::PLAIN);
    let first = run_pass(&runner, &spec, &slices(1), &ctx, &world.store, &lease)
        .await
        .unwrap();
    lease.release().await;
    assert!(first.interrupted);
    let second = run(&world, &fixture::PLAIN, &slices(1), 10).await;
    assert!(second.resumed);
    assert_eq!(second.status, MigrationRunStatus::DoneWithIssues);
    assert_eq!(second.counts.read, 30);
    assert_eq!(second.counts.written, 29);
    let groups = issue_groups(&world.store, "fixture-plain").await.unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].reason, "invalid_value:body");
    world.drop_keyspace().await;
}

/// A runner whose reads and writes fail with a timeout a set number of times first
struct Flaky {
    /// The migration it wraps
    inner: &'static dyn MigrationRunner,
    /// How many more reads fail
    reads: AtomicUsize,
    /// How many more writes fail
    writes: AtomicUsize,
    /// How many failures were injected
    injected: AtomicUsize,
}

impl Flaky {
    /// Take one failure from a budget, if it has any left
    ///
    /// # Arguments
    ///
    /// * `budget` - The failures left
    fn fail(&self, budget: &AtomicUsize) -> bool {
        // count down without going below zero
        let mut left = budget.load(Ordering::SeqCst);
        let failed = loop {
            if left == 0 {
                break false;
            }
            match budget.compare_exchange(left, left - 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => break true,
                Err(now) => left = now,
            }
        };
        if failed {
            self.injected.fetch_add(1, Ordering::SeqCst);
        }
        failed
    }
}

#[async_trait]
impl MigrationRunner for Flaky {
    /// Split the family as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `n` - How many slices to make
    /// * `ctx` - The backends, settings, and mode of this run
    async fn slices(&self, n: u32, ctx: &RunContext) -> Result<Vec<Slice>, MigrateError> {
        self.inner.slices(n, ctx).await
    }
    /// Read a batch as the wrapped migration does, failing reads with a timeout while the budget
    /// lasts
    ///
    /// # Arguments
    ///
    /// * `slice` - The slice to read
    /// * `after` - The last position done, or the slice's start
    /// * `ctx` - The backends, settings, and mode of this run
    async fn next_batch(
        &self,
        slice: &Slice,
        after: &serde_json::Value,
        ctx: &RunContext,
    ) -> Result<Batch, MigrateError> {
        if self.fail(&self.reads) {
            return Err(MigrateError::Backend("a read timed out".into()));
        }
        self.inner.next_batch(slice, after, ctx).await
    }
    /// Classify as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `record` - The stored record
    /// * `mode` - `Run` or `Check`
    /// * `ctx` - The backends, settings, and mode of this run
    async fn process(
        &self,
        record: &Record,
        mode: RunMode,
        ctx: &RunContext,
    ) -> Result<Outcome, MigrateError> {
        self.inner.process(record, mode, ctx).await
    }
    /// Write as the wrapped migration does, failing writes with a timeout while the budget lasts
    ///
    /// # Arguments
    ///
    /// * `record` - The converted record
    /// * `ctx` - The backends, settings, and mode of this run
    async fn write(&self, record: &Record, ctx: &RunContext) -> Result<(), MigrateError> {
        if self.fail(&self.writes) {
            return Err(MigrateError::Backend("a write timed out".into()));
        }
        self.inner.write(record, ctx).await
    }
    /// Finish as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `ctx` - The backends, settings, and mode of this run
    async fn finish(&self, ctx: &RunContext) -> Result<(), MigrateError> {
        self.inner.finish(ctx).await
    }
}

/// Timeouts are retried with fewer writes in flight and never become issues, while a
/// deterministically rejected write is still recorded as `write_failed:rejected`
#[tokio::test]
async fn timeouts_are_retried_and_a_rejection_is_recorded() {
    // a table where every write may time out a few times, with one row that is always rejected
    let world = World::new("mr04a").await;
    world.seed_valid("a", 300).await;
    world
        .seed(&[("reject-it".into(), "{\"name\":\"r\"}".into())])
        .await;
    let flaky = Flaky {
        inner: fixture::THEME.runner,
        reads: AtomicUsize::new(3),
        writes: AtomicUsize::new(40),
        injected: AtomicUsize::new(0),
    };
    let lease = world.lease("test").await;
    let ctx = world.ctx(RunMode::Run, 50);
    let out = run_pass(
        &flaky,
        &World::spec(&fixture::THEME),
        &slices(2),
        &ctx,
        &world.store,
        &lease,
    )
    .await
    .expect("the run retries through the timeouts");
    lease.release().await;
    // the timeouts happened, and none of those objects is an issue
    assert_eq!(flaky.injected.load(Ordering::SeqCst), 43);
    assert_eq!(out.status, MigrationRunStatus::DoneWithIssues);
    assert_eq!(out.groups.len(), 1);
    assert_eq!(out.groups[0].reason, "write_failed:rejected");
    assert_eq!(out.counts.written, 300);
    assert!(
        world
            .rows()
            .await
            .iter()
            .filter(|row| row.0.starts_with("a-"))
            .all(|row| row.2 == 2)
    );
    world.drop_keyspace().await;
}

/// A run whose Scylla connections are reset and refused partway resumes through them and ends
/// with every object converted and none recorded as an issue
#[tokio::test]
async fn dropped_connections_are_retried_without_issues() {
    // a proxy in front of Scylla, and a run that goes through it
    let world = World::new("mr04b").await;
    world.seed_valid("a", 600).await;
    let node: std::net::SocketAddr = world.conf.scylla.nodes[0].parse().unwrap();
    let proxy = common::proxy::FaultProxy::start(node).await.unwrap();
    let mut conf = world.conf.clone();
    conf.scylla.nodes = vec![proxy.listen.to_string()];
    let translator = common::proxy::scylla_translator(world.session(), node, proxy.listen).await;
    thoradm::shared::scylla::route_through(
        &proxy.listen.to_string(),
        std::sync::Arc::new(translator),
    );
    let session = std::sync::Arc::new(thoradm::shared::scylla::get_client(&conf).await.unwrap());
    let mut backends = world.backends.clone();
    backends.scylla = session.clone();
    let store = thorium::models::schema::PassStore::prepare(session, &world.ns)
        .await
        .unwrap();
    // connections reset after a little traffic, and new ones are refused for a moment
    proxy
        .set_rules(vec![common::proxy::ProxyRule {
            from_connection: None,
            after_bytes: 30_000,
            upstream: true,
            action: common::proxy::ProxyAction::Reset,
            times: Some(6),
        }])
        .await;
    let mut ctx = world.ctx(RunMode::Run, 50);
    ctx.backends = backends;
    ctx.limits.retries = 30;
    let opened = proxy.connections();
    let lease = world.lease("test").await;
    let out = run_pass(
        fixture::THEME.runner,
        &World::spec(&fixture::THEME),
        &slices(2),
        &ctx,
        &store,
        &lease,
    )
    .await
    .expect("the run resumes through the resets");
    lease.release().await;
    assert_eq!(out.status, MigrationRunStatus::Done);
    assert!(out.groups.is_empty(), "{:?}", out.groups);
    assert!(world.rows().await.iter().all(|row| row.2 == 2));
    // the driver had to open new connections after the resets
    assert!(
        proxy.connections() > opened,
        "{} connections, {opened} before",
        proxy.connections()
    );
    world.drop_keyspace().await;
}

/// A pass whose connection is reset exactly as it records how it ended retries that write and
/// ends with the status the work earned
#[tokio::test]
async fn a_reset_while_recording_the_end_of_a_pass_is_retried() {
    // a proxy in front of Scylla, and a small table so the pass is short
    let world = World::new("mr04d").await;
    world.seed_valid("a", 50).await;
    let node: std::net::SocketAddr = world.conf.scylla.nodes[0].parse().unwrap();
    let proxy = common::proxy::FaultProxy::start(node).await.unwrap();
    let mut conf = world.conf.clone();
    conf.scylla.nodes = vec![proxy.listen.to_string()];
    let translator = common::proxy::scylla_translator(world.session(), node, proxy.listen).await;
    thoradm::shared::scylla::route_through(
        &proxy.listen.to_string(),
        std::sync::Arc::new(translator),
    );
    let session = std::sync::Arc::new(thoradm::shared::scylla::get_client(&conf).await.unwrap());
    let mut backends = world.backends.clone();
    backends.scylla = session.clone();
    let store = thorium::models::schema::PassStore::prepare(session, &world.ns)
        .await
        .unwrap();
    // only the write that ends the pass carries the report's field names, so the reset lands there
    proxy.reset_on(Some(b"skipped_total".to_vec()));
    let mut ctx = world.ctx(RunMode::Run, 50);
    ctx.backends = backends;
    ctx.limits.retries = 5;
    let lease = world.lease("test").await;
    let out = run_pass(
        fixture::THEME.runner,
        &World::spec(&fixture::THEME),
        &slices(2),
        &ctx,
        &store,
        &lease,
    )
    .await
    .expect("the write that ends the pass is retried");
    lease.release().await;
    // the reset fired, and the pass still ended as the work earned
    assert!(!proxy.waiting(), "the reset never fired");
    assert_eq!(out.status, MigrationRunStatus::Done);
    assert!(out.groups.is_empty(), "{:?}", out.groups);
    world.drop_keyspace().await;
}

/// A run whose Scylla connections stall for longer than the driver's request timeout backs off,
/// retries on connections it opens afterwards, and ends with every object converted and none
/// recorded as an issue
///
/// A frozen node and a stalled socket look the same to the client, so the proxy stands in for
/// pausing the node.
#[tokio::test]
async fn a_stalled_scylla_is_retried_through() {
    // a proxy in front of Scylla, and a table large enough to be mid-run when the stall begins
    let world = World::new("mr04c").await;
    world.seed_valid("a", 8000).await;
    let node: std::net::SocketAddr = world.conf.scylla.nodes[0].parse().unwrap();
    let proxy = common::proxy::FaultProxy::start(node).await.unwrap();
    let mut conf = world.conf.clone();
    conf.scylla.nodes = vec![proxy.listen.to_string()];
    let translator = common::proxy::scylla_translator(world.session(), node, proxy.listen).await;
    thoradm::shared::scylla::route_through(
        &proxy.listen.to_string(),
        std::sync::Arc::new(translator),
    );
    let session = std::sync::Arc::new(thoradm::shared::scylla::get_client(&conf).await.unwrap());
    let mut backends = world.backends.clone();
    backends.scylla = session.clone();
    let store = thorium::models::schema::PassStore::prepare(session, &world.ns)
        .await
        .unwrap();
    // every connection of the pool stalls once, for longer than the request timeout, after a
    // little traffic
    proxy
        .set_rules(vec![common::proxy::ProxyRule {
            from_connection: None,
            after_bytes: 30_000,
            upstream: true,
            action: common::proxy::ProxyAction::Stall(std::time::Duration::from_secs(40)),
            times: Some(4),
        }])
        .await;
    let mut ctx = world.ctx(RunMode::Run, 50);
    ctx.backends = backends;
    ctx.limits.retries = 60;
    ctx.limits.backoff = std::time::Duration::from_millis(500);
    let lease = world.lease("test").await;
    let out = run_pass(
        fixture::THEME.runner,
        &World::spec(&fixture::THEME),
        &slices(2),
        &ctx,
        &store,
        &lease,
    )
    .await
    .expect("the run retries through the stall");
    lease.release().await;
    // nothing became an issue, and every object converted
    assert_eq!(out.status, MigrationRunStatus::Done);
    assert!(out.groups.is_empty(), "{:?}", out.groups);
    assert!(world.rows().await.iter().all(|row| row.2 == 2));
    // the stall happened mid-run; the number of connections the driver opens depends on the
    // shard count and on whether it replaces a stalled one, so it is not asserted
    let stalls = proxy.firings().await;
    assert!(stalls >= 1, "the proxy never stalled a connection");
    world.drop_keyspace().await;
}

/// A check classifies every object however many distinct groups it finds, and reports the ten
/// largest, since a partial report must never stand for the whole
#[tokio::test]
async fn a_check_never_stops_at_the_group_cap() {
    // 400 objects, each its own group, in batches of 50
    let world = World::new("mr35c").await;
    let rows = (0..400)
        .map(|n| (format!("p-{n}"), poison(n)))
        .collect::<Vec<_>>();
    world.seed(&rows).await;
    let ctx = world.ctx(RunMode::Check, 50);
    let run = check_pass(
        fixture::THEME.runner,
        &World::spec(&fixture::THEME),
        &slices(1),
        &ctx,
    )
    .await
    .expect("the check finishes");
    assert_eq!(run.report.counts.read, 400);
    assert_eq!(run.report.counts.unrecognized, 400);
    assert_eq!((run.report.reasons.len(), run.report.more), (10, 390));
    world.drop_keyspace().await;
}

/// Running again with the same slices after a pass ended `Done` or `Done` with issues starts a new
/// attempt that processes every object
#[tokio::test]
async fn a_rerun_with_the_same_slices_after_the_pass_ended_is_a_new_attempt() {
    for bad in [0_usize, 2] {
        // a finished pass, then every valid row put back in layout 1
        let world = World::new(&format!("mr41s{bad}")).await;
        world.seed_valid("a", 20).await;
        let bad_rows = (0..bad)
            .map(|n| (format!("bad-{n}"), "{\"name\":5}".to_owned()))
            .collect::<Vec<_>>();
        world.seed(&bad_rows).await;
        let first = run(&world, &fixture::THEME, &slices(2), 10).await;
        assert_ne!(first.status, MigrationRunStatus::Failed);
        for n in 0..20 {
            world
                .session()
                .query_unpaged(
                    format!(
                        "UPDATE {}.mig_fixture SET body = '{{\"name\":\"back\"}}', layout = 1 \
                         WHERE id = 'a-{n}'",
                        world.ns
                    ),
                    (),
                )
                .await
                .unwrap();
        }
        // the same slicing doesn't resume the ended pass
        let second = run(&world, &fixture::THEME, &slices(2), 10).await;
        assert!(!second.resumed, "{bad} bad rows");
        assert_ne!(second.attempt, first.attempt);
        assert_eq!(second.counts.read, 20 + bad as u64);
        assert_eq!(second.counts.written, 20);
        world.drop_keyspace().await;
    }
}

/// A runner that fails its fourth read for good, so no end-of-run write follows
struct FailsAtFour {
    /// The migration it wraps
    inner: &'static dyn MigrationRunner,
    /// How many batches were read
    seen: AtomicUsize,
}

#[async_trait]
impl MigrationRunner for FailsAtFour {
    /// Split the family as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `n` - How many slices to make
    /// * `ctx` - The backends, settings, and mode of this run
    async fn slices(&self, n: u32, ctx: &RunContext) -> Result<Vec<Slice>, MigrateError> {
        self.inner.slices(n, ctx).await
    }
    /// Read a batch, failing the fourth
    ///
    /// # Arguments
    ///
    /// * `slice` - The slice to read
    /// * `after` - The last position done, or the slice's start
    /// * `ctx` - The backends, settings, and mode of this run
    async fn next_batch(
        &self,
        slice: &Slice,
        after: &serde_json::Value,
        ctx: &RunContext,
    ) -> Result<Batch, MigrateError> {
        if self.seen.fetch_add(1, Ordering::SeqCst) == 3 {
            return Err(MigrateError::Systematic("the fourth read fails".to_owned()));
        }
        self.inner.next_batch(slice, after, ctx).await
    }
    /// Classify as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `record` - The stored record
    /// * `mode` - `Run` or `Check`
    /// * `ctx` - The backends, settings, and mode of this run
    async fn process(
        &self,
        record: &Record,
        mode: RunMode,
        ctx: &RunContext,
    ) -> Result<Outcome, MigrateError> {
        self.inner.process(record, mode, ctx).await
    }
    /// Write as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `record` - The converted record
    /// * `ctx` - The backends, settings, and mode of this run
    async fn write(&self, record: &Record, ctx: &RunContext) -> Result<(), MigrateError> {
        self.inner.write(record, ctx).await
    }
    /// Finish as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `ctx` - The backends, settings, and mode of this run
    async fn finish(&self, ctx: &RunContext) -> Result<(), MigrateError> {
        self.inner.finish(ctx).await
    }
}

/// A batch that found issues is checkpointed with them even when the checkpoint interval is long
#[tokio::test]
async fn an_issue_bearing_batch_always_checkpoints() {
    // every row is bad, so every batch finds issues; the fourth read fails
    let world = World::new("mr01i").await;
    let rows = (0..300)
        .map(|n| (format!("bad-{n}"), "{\"name\":5}".to_owned()))
        .collect::<Vec<_>>();
    world.seed(&rows).await;
    let runner = FailsAtFour {
        inner: fixture::THEME.runner,
        seen: AtomicUsize::new(0),
    };
    let mut ctx = world.ctx(RunMode::Run, 50);
    ctx.limits.checkpoint_every = std::time::Duration::from_secs(3600);
    let mut spec = World::spec(&fixture::THEME);
    spec.group_max = 1000;
    let lease = world.lease("test").await;
    let out = run_pass(&runner, &spec, &slices(1), &ctx, &world.store, &lease)
        .await
        .unwrap();
    lease.release().await;
    // the three batches that were read before the failure all reached the issue rows
    let stored: u64 = out.groups.iter().map(|group| group.count).sum();
    assert_eq!(stored, 150, "{:?}", out.groups);
    world.drop_keyspace().await;
}

/// Rows of another attempt are ignored by the readers
#[tokio::test]
async fn rows_of_another_attempt_are_ignored() {
    // a worker row and an issue row left by an earlier attempt in the pass's partition
    let world = World::new("mr41r").await;
    world
        .store
        .begin_attempt("k", "current", "h", None, None, None)
        .await
        .unwrap();
    for cql in [
        format!(
            "INSERT INTO {0}.schema_migration_issues (migration, transform, reason, worker, attempt, \
             class, count, samples) VALUES ('k', 't', 'r', 'w00', 'old', 'unrecognized', 3, {{'x'}})",
            world.ns
        ),
        format!(
            "INSERT INTO {0}.schema_migration_issues (migration, transform, reason, worker, attempt, \
             checkpoint) VALUES ('k', '~worker', '~', 'w00', 'old', '{{\"slices\":1}}')",
            world.ns
        ),
    ] {
        world.session().query_unpaged(cql, ()).await.unwrap();
    }
    let (workers, issues) = world.store.rows("k", "current").await.unwrap();
    assert!(workers.is_empty() && issues.is_empty());
    let (workers, issues) = world.store.rows("k", "old").await.unwrap();
    assert_eq!((workers.len(), issues.len()), (1, 1));
    world.drop_keyspace().await;
}

/// A run that is cancelled writes the checkpoint of the batches finished since its last write
#[tokio::test]
async fn cancel_flushes_the_checkpoint() {
    // a long checkpoint interval, so only the first batch is written on schedule
    let world = World::new("mr02f").await;
    world.seed_valid("a", 300).await;
    let cancel = CancellationToken::new();
    let runner = CancelAfter::new(&fixture::THEME, 4, cancel.clone());
    let mut ctx = world.ctx(RunMode::Run, 50);
    ctx.limits.checkpoint_every = std::time::Duration::from_secs(3600);
    ctx.cancel = cancel;
    let lease = world.lease("test").await;
    let out = run_pass(
        &runner,
        &World::spec(&fixture::THEME),
        &slices(1),
        &ctx,
        &world.store,
        &lease,
    )
    .await
    .unwrap();
    lease.release().await;
    assert!(out.interrupted);
    // the four batches classified before the stop are all in the checkpoint
    let row = world.store.pass_row("fixture").await.unwrap().unwrap();
    let (workers, _) = world.store.rows("fixture", &row.attempt).await.unwrap();
    assert_eq!(workers[0].checkpoint.counts.read, 200);
    world.drop_keyspace().await;
}

/// A runner that waits inside one record, so another writer can take over meanwhile
struct Gate {
    /// The migration it wraps
    inner: &'static dyn MigrationRunner,
    /// How many records were classified
    seen: AtomicUsize,
    /// Told when the wait begins
    reached: std::sync::Arc<tokio::sync::Notify>,
    /// Waited on until the takeover is done
    release: std::sync::Arc<tokio::sync::Notify>,
}

#[async_trait]
impl MigrationRunner for Gate {
    /// Split the family as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `n` - How many slices to make
    /// * `ctx` - The backends, settings, and mode of this run
    async fn slices(&self, n: u32, ctx: &RunContext) -> Result<Vec<Slice>, MigrateError> {
        self.inner.slices(n, ctx).await
    }
    /// Read a batch as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `slice` - The slice to read
    /// * `after` - The last position done, or the slice's start
    /// * `ctx` - The backends, settings, and mode of this run
    async fn next_batch(
        &self,
        slice: &Slice,
        after: &serde_json::Value,
        ctx: &RunContext,
    ) -> Result<Batch, MigrateError> {
        self.inner.next_batch(slice, after, ctx).await
    }
    /// Classify, waiting at the 120th record
    ///
    /// # Arguments
    ///
    /// * `record` - The stored record
    /// * `mode` - `Run` or `Check`
    /// * `ctx` - The backends, settings, and mode of this run
    async fn process(
        &self,
        record: &Record,
        mode: RunMode,
        ctx: &RunContext,
    ) -> Result<Outcome, MigrateError> {
        if self.seen.fetch_add(1, Ordering::SeqCst) + 1 == 120 {
            self.reached.notify_one();
            self.release.notified().await;
        }
        self.inner.process(record, mode, ctx).await
    }
    /// Write as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `record` - The converted record
    /// * `ctx` - The backends, settings, and mode of this run
    async fn write(&self, record: &Record, ctx: &RunContext) -> Result<(), MigrateError> {
        self.inner.write(record, ctx).await
    }
    /// Finish as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `ctx` - The backends, settings, and mode of this run
    async fn finish(&self, ctx: &RunContext) -> Result<(), MigrateError> {
        self.inner.finish(ctx).await
    }
}

/// A holder whose lease was taken over while its batch was in flight writes no checkpoint after
/// the takeover finished the pass
#[tokio::test]
async fn a_stale_holders_batch_is_not_written_after_a_takeover() {
    // holder A stalls inside a batch; B breaks the lease and finishes the pass
    let world = World::new("mr40s").await;
    world.seed_valid("v", 400).await;
    let lease_a = world.lease("a").await;
    let reached = std::sync::Arc::new(tokio::sync::Notify::new());
    let gate = std::sync::Arc::new(tokio::sync::Notify::new());
    let gated = Gate {
        inner: fixture::THEME.runner,
        seen: AtomicUsize::new(0),
        reached: reached.clone(),
        release: gate.clone(),
    };
    let spec = World::spec(&fixture::THEME);
    let opts = slices(1);
    let ctx_a = world.ctx(RunMode::Run, 50);
    let run_a = run_pass(&gated, &spec, &opts, &ctx_a, &world.store, &lease_a);
    let takeover = async {
        reached.notified().await;
        let lease_b = WriterLease::acquire(&world.backends.redis, &world.ns, "b", true)
            .await
            .unwrap();
        let ctx_b = world.ctx(RunMode::Run, 50);
        let b = run_pass(
            fixture::THEME.runner,
            &spec,
            &opts,
            &ctx_b,
            &world.store,
            &lease_b,
        )
        .await
        .unwrap();
        gate.notify_one();
        b
    };
    let (a_result, b) = tokio::join!(run_a, takeover);
    assert_eq!(b.status, MigrationRunStatus::Done);
    assert!(matches!(a_result, Err(MigrateError::LeaseLost)));
    // the pass is as the takeover left it
    let row = world.store.pass_row(&spec.key).await.unwrap().unwrap();
    let (workers, _) = world.store.rows(&spec.key, &row.attempt).await.unwrap();
    assert!(workers[0].checkpoint.done);
    world.drop_keyspace().await;
}

/// A holder that already holds the lease takes it again at once, as a retried pod of the same Job
/// does, while any other holder is refused
#[tokio::test]
async fn a_holder_takes_its_own_lease_again() {
    // a lease held by one Job's pod
    let world = World::new("mr40h").await;
    let first = world.lease("job-1").await;
    // the retried pod of the same Job takes it, and the earlier pod's token is no longer current
    let again = WriterLease::acquire(&world.backends.redis, &world.ns, "job-1", false)
        .await
        .expect("the same holder takes it again");
    assert!(again.is_current().await.unwrap());
    assert!(!first.is_current().await.unwrap());
    // another holder is still refused, naming the holder
    let other = WriterLease::acquire(&world.backends.redis, &world.ns, "job-2", false).await;
    assert!(other.err().unwrap().to_string().contains("job-1"));
    world.drop_keyspace().await;
}

/// A check asks the migration for its notes: the check summary counts the warnings without any
/// name, and the whole report keeps the note for a hand-run check to print
#[tokio::test]
async fn a_check_carries_the_migrations_warnings_but_no_names() {
    // a migration whose report always gets one warning note
    let world = World::new("mr35n").await;
    world.seed_valid("a", 5).await;
    let ctx = world.ctx(RunMode::Check, 10);
    let run = check_pass(
        fixture::NOTES.runner,
        &World::spec(&fixture::NOTES),
        &slices(1),
        &ctx,
    )
    .await
    .unwrap();
    // the stored summary has the count and no names
    let summary = run.report.summary.as_ref().expect("a summary");
    assert!(summary.warnings >= 1);
    let json = serde_json::to_string(&run.report).unwrap();
    assert!(
        !json.contains("item-name") && !json.contains("needs a look"),
        "{json}"
    );
    assert!(json.len() < 4096);
    // the note itself is for a hand-run check
    assert_eq!(run.applied.notes.len(), 1);
    assert_eq!(run.applied.notes[0].item, "item-name");
    world.drop_keyspace().await;
}
