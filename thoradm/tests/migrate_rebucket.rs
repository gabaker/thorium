//! Tests of the `rebucket-<data type>` migrations: rows move to the bucket the configured size
//! gives, the census is rebuilt, the size is recorded, and an interrupted run resumes

mod common;

use chrono::{DateTime, Duration, TimeZone, Utc};
use common::migrate::World;
use std::collections::{BTreeMap, BTreeSet};
use thoradm::migrate::registry::{MIGRATIONS, MigrationDef};
use thorium::models::schema::{
    Bucket, MigrateError, MigrationRunStatus, RunMode, RunOptions, run_pass,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// The size rows are written at
const OLD_SIZE: u16 = 100;

/// The size the rows are moved to
const NEW_SIZE: u16 = 1000;

/// A runner that cancels the run after a few batches
struct Stopper {
    /// The migration
    inner: &'static dyn thorium::models::schema::MigrationRunner,
    /// The token to cancel
    cancel: CancellationToken,
    /// Batches read
    seen: std::sync::atomic::AtomicUsize,
}
#[async_trait::async_trait]
impl thorium::models::schema::MigrationRunner for Stopper {
    /// Split the family as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `n` - How many slices to make
    /// * `ctx` - The backends, settings, and mode of this run
    async fn slices(
        &self,
        n: u32,
        ctx: &thorium::models::schema::RunContext,
    ) -> Result<Vec<thorium::models::schema::Slice>, MigrateError> {
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
        slice: &thorium::models::schema::Slice,
        after: &serde_json::Value,
        ctx: &thorium::models::schema::RunContext,
    ) -> Result<thorium::models::schema::Batch, MigrateError> {
        let batch = self.inner.next_batch(slice, after, ctx).await?;
        if self.seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 3 {
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
        record: &thorium::models::upgrades::Record,
        mode: RunMode,
        ctx: &thorium::models::schema::RunContext,
    ) -> Result<thorium::models::schema::Outcome, MigrateError> {
        self.inner.process(record, mode, ctx).await
    }
    /// Write as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `record` - The converted record
    /// * `ctx` - The backends, settings, and mode of this run
    async fn write(
        &self,
        record: &thorium::models::upgrades::Record,
        ctx: &thorium::models::schema::RunContext,
    ) -> Result<(), MigrateError> {
        self.inner.write(record, ctx).await
    }
    /// Finish as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `ctx` - The backends, settings, and mode of this run
    async fn finish(&self, ctx: &thorium::models::schema::RunContext) -> Result<(), MigrateError> {
        self.inner.finish(ctx).await
    }
}

/// Find a registered migration
///
/// # Arguments
///
/// * `id` - The migration's id
fn def(id: &str) -> &'static MigrationDef {
    // the registry holds every rebucket migration
    MIGRATIONS
        .iter()
        .find(|def| def.id == id)
        .expect("registered")
}

/// The time of the nth row: spread so old and new buckets differ and rows share buckets
///
/// # Arguments
///
/// * `n` - The row's number
fn at(n: i64) -> DateTime<Utc> {
    // a March morning, a row every 37 seconds
    Utc.with_ymd_and_hms(2026, 3, 1, 8, 0, 0).unwrap() + Duration::seconds(n * 37)
}

/// The bucket a time has at a size
///
/// # Arguments
///
/// * `at` - The time
/// * `size` - The partition size
fn bucket(at: DateTime<Utc>, size: u16) -> i32 {
    // the same computation the API makes
    i32::try_from(Bucket::of(at, 2026, size).unwrap().get()).unwrap()
}

/// Add a member to a census key, as the API does when it writes a row
///
/// # Arguments
///
/// * `world` - The test's world
/// * `key` - The census key
/// * `bucket` - The bucket
async fn census_add(world: &World, key: &str, bucket: i32) {
    // member and score are both the bucket
    let mut conn = world.backends.redis.get().await.unwrap();
    let _: () = redis::cmd("ZADD")
        .arg(key)
        .arg(bucket)
        .arg(bucket)
        .query_async(&mut *conn)
        .await
        .unwrap();
}

/// A census key's members, sorted
///
/// # Arguments
///
/// * `world` - The test's world
/// * `key` - The census key
async fn members(world: &World, key: &str) -> Vec<i32> {
    // read the whole sorted set
    let mut conn = world.backends.redis.get().await.unwrap();
    let mut found: Vec<i32> = redis::cmd("ZRANGE")
        .arg(key)
        .arg(0)
        .arg(-1)
        .query_async(&mut *conn)
        .await
        .unwrap();
    found.sort_unstable();
    found
}

/// Insert `count` samples across two groups at the old size, with their census
///
/// # Arguments
///
/// * `world` - The test's world
/// * `count` - How many rows
async fn seed_files(world: &World, count: i64) {
    // each row lands in the bucket the old size gives, and in the census
    let insert = world
        .session()
        .prepare(format!(
            "INSERT INTO {}.samples_list (group, year, bucket, uploaded, id, sha256, sha1, md5, \
             name, description, submitter, origin) VALUES (?, 2026, ?, ?, ?, 's', 's', 's', null, null, 'u', null)",
            world.ns
        ))
        .await
        .unwrap();
    for n in 0..count {
        let group = if n % 2 == 0 { "ga" } else { "gb" };
        let time = at(n);
        let old = bucket(time, OLD_SIZE);
        world
            .session()
            .execute_unpaged(&insert, (group, old, time, Uuid::new_v4()))
            .await
            .unwrap();
        census_add(
            world,
            &format!("{}:census:samples:stream:{group}:2026", world.ns),
            old,
        )
        .await;
    }
}

/// Every samples row as `(group, bucket, uploaded)`
///
/// # Arguments
///
/// * `world` - The test's world
async fn file_rows(world: &World) -> Vec<(String, i32, DateTime<Utc>)> {
    // a full scan of the small table
    let rows = world
        .session()
        .query_unpaged(
            format!(
                "SELECT group, bucket, uploaded FROM {}.samples_list",
                world.ns
            ),
            (),
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap();
    rows.rows::<(String, i32, DateTime<Utc>)>()
        .unwrap()
        .map(|row| row.unwrap())
        .collect()
}

/// Run a rebucket migration at the new size
///
/// # Arguments
///
/// * `world` - The test's world
/// * `id` - The migration
/// * `census_batches` - Stop the census after this many batches
/// * `batch_rows` - The rows of each batch
async fn rebucket(
    world: &World,
    id: &str,
    census_batches: Option<usize>,
    batch_rows: usize,
) -> Result<thorium::models::schema::PassOutcome, MigrateError> {
    // the config says the new size
    let def = def(id);
    let mut ctx = world.ctx(RunMode::Run, batch_rows);
    for size in ctx.backends.partition_sizes.values_mut() {
        *size = NEW_SIZE;
    }
    ctx.limits.census_batches = census_batches;
    ctx.limits.scan_count = 5;
    let lease = world.lease("test").await;
    let opts = RunOptions {
        slices: 2,
        ..RunOptions::default()
    };
    let out = run_pass(
        def.runner,
        &World::spec(def),
        &opts,
        &ctx,
        &world.store,
        &lease,
    )
    .await;
    lease.release().await;
    out
}

/// The recorded value of a setting
///
/// # Arguments
///
/// * `world` - The test's world
/// * `id` - The config row's id
async fn recorded(world: &World, id: &str) -> Option<String> {
    // a config row's value column
    let rows = world
        .session()
        .query_unpaged(
            format!(
                "SELECT value FROM {}.schema_migrations WHERE kind = 'config' AND id = '{id}'",
                world.ns
            ),
            (),
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap();
    rows.maybe_first_row::<(Option<String>,)>()
        .unwrap()
        .and_then(|row| row.0)
}

/// Rows written at one size, the size changed, then `rebucket-files`: every row is in the bucket the
/// new size gives, none is left behind, the census holds only new buckets, the size is recorded,
/// and updates, deletes, and listings find the rows as they do on fresh data
#[tokio::test]
async fn rebucket_files_moves_rows_and_rebuilds_the_census() {
    // 300 rows in two groups at the old size, with a stale census
    let world = World::new_full("mr25f").await;
    seed_files(&world, 300).await;
    let out = rebucket(&world, "rebucket-files", None, 40)
        .await
        .expect("the pass runs");
    assert_eq!(out.status, MigrationRunStatus::Done);
    assert_eq!(out.counts.written, 300);
    // every row is in its new bucket, and none is left in an old one
    let rows = file_rows(&world).await;
    assert_eq!(rows.len(), 300);
    assert!(
        rows.iter()
            .all(|(_, b, time)| *b == bucket(*time, NEW_SIZE))
    );
    // the census holds exactly the new buckets, no old-size bucket
    for group in ["ga", "gb"] {
        let expected = rows
            .iter()
            .filter(|(g, _, _)| g == group)
            .map(|(_, b, _)| *b)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let found = members(
            &world,
            &format!("{}:census:samples:stream:{group}:2026", world.ns),
        )
        .await;
        assert_eq!(found, expected, "{group}");
    }
    // the record holds the new size
    assert_eq!(
        recorded(&world, "partition_size.files").await.as_deref(),
        Some("1000")
    );
    // a delete by the new bucket finds a row, and a listing of its partition holds it
    let (group, b, time) = rows[0].clone();
    let listed = world
        .session()
        .query_unpaged(
            format!(
                "SELECT id FROM {}.samples_list WHERE group = ? AND year = 2026 AND bucket = ?",
                world.ns
            ),
            (&group, b),
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap()
        .rows_num();
    assert!(listed >= 1);
    world
        .session()
        .query_unpaged(
            format!(
                "DELETE FROM {}.samples_list WHERE group = ? AND year = 2026 AND bucket = ? AND uploaded = ?",
                world.ns
            ),
            (&group, bucket(time, NEW_SIZE), time),
        )
        .await
        .unwrap();
    assert_eq!(file_rows(&world).await.len(), 299);
    // running it again moves nothing
    let again = rebucket(&world, "rebucket-files", None, 40).await.unwrap();
    assert_eq!(again.counts.written, 0);
    world.drop_keyspace().await;
}

/// A rebucket interrupted mid-pass, or mid-census in either phase, resumes and ends as an
/// uninterrupted one does
#[tokio::test]
async fn an_interrupted_rebucket_resumes_in_every_phase() {
    // the rows pass stopped after a few batches
    let world = World::new_full("mr25r").await;
    seed_files(&world, 300).await;
    let cancel = CancellationToken::new();
    let mut ctx = world.ctx(RunMode::Run, 20);
    for size in ctx.backends.partition_sizes.values_mut() {
        *size = NEW_SIZE;
    }
    ctx.cancel = cancel.clone();
    let lease = world.lease("test").await;
    let spec = World::spec(def("rebucket-files"));
    // cancel once a few rows moved, by a runner that wraps the migration's own
    let stopper = Stopper {
        inner: def("rebucket-files").runner,
        cancel,
        seen: 0.into(),
    };
    let opts = RunOptions {
        slices: 2,
        ..RunOptions::default()
    };
    let first = run_pass(&stopper, &spec, &opts, &ctx, &world.store, &lease)
        .await
        .unwrap();
    lease.release().await;
    assert!(first.interrupted);
    let moved = file_rows(&world)
        .await
        .iter()
        .filter(|(_, b, time)| *b == bucket(*time, NEW_SIZE))
        .count();
    assert!(moved > 0 && moved < 300, "{moved}");
    // the census stopped after its first delete step, then after the delete phase, then mid-add
    for budget in [1_usize, 3, 4] {
        let outcome = rebucket(&world, "rebucket-files", Some(budget), 20).await;
        assert!(
            matches!(outcome, Err(MigrateError::Cancelled)),
            "budget {budget}: {:?}",
            outcome.map(|out| out.status)
        );
        // the census checkpoint is kept for the next run
        let mut conn = world.backends.redis.get().await.unwrap();
        let state: Option<String> = redis::cmd("GET")
            .arg(format!("{}:rebucket:files:census", world.ns))
            .query_async(&mut *conn)
            .await
            .unwrap();
        assert!(state.is_some(), "budget {budget}");
    }
    // a last run finishes the rows, the census, and the record
    let done = rebucket(&world, "rebucket-files", None, 20)
        .await
        .expect("the pass resumes");
    assert_eq!(done.status, MigrationRunStatus::Done);
    let rows = file_rows(&world).await;
    assert!(
        rows.iter()
            .all(|(_, b, time)| *b == bucket(*time, NEW_SIZE))
    );
    let expected: BTreeMap<String, Vec<i32>> = ["ga", "gb"]
        .into_iter()
        .map(|group| {
            (
                group.to_owned(),
                rows.iter()
                    .filter(|(g, _, _)| g == group)
                    .map(|(_, b, _)| *b)
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect(),
            )
        })
        .collect();
    for (group, buckets) in expected {
        assert_eq!(
            members(
                &world,
                &format!("{}:census:samples:stream:{group}:2026", world.ns)
            )
            .await,
            buckets
        );
    }
    assert_eq!(
        recorded(&world, "partition_size.files").await.as_deref(),
        Some("1000")
    );
    let mut conn = world.backends.redis.get().await.unwrap();
    let state: Option<String> = redis::cmd("GET")
        .arg(format!("{}:rebucket:files:census", world.ns))
        .query_async(&mut *conn)
        .await
        .unwrap();
    assert!(
        state.is_none(),
        "the census checkpoint is cleared when the rebuild ends"
    );
    world.drop_keyspace().await;
}

/// Entities and associations move too, and the associations census holds both the source and the
/// target key of each row
#[tokio::test]
#[expect(clippy::too_many_lines, reason = "a scenario test reads top to bottom")]
async fn rebucket_entities_and_associations_rebuild_both_census_keys() {
    // entities and associations at the old size, each in the census
    let world = World::new_full("mr25e").await;
    let entity = world
        .session()
        .prepare(format!(
            "INSERT INTO {}.entities (kind, group, year, bucket, created, id, name, submitter, kind_data, description, image) \
             VALUES ('\"Device\"', 'ge', 2026, ?, ?, ?, 'n', 'u', '{{}}', null, null)",
            world.ns
        ))
        .await
        .unwrap();
    let assoc = world
        .session()
        .prepare(format!(
            "INSERT INTO {}.associations (group, year, bucket, source, created, target, direction, kind, submitter, \
             extra_source, extra_target, submissions_source, submissions_other) \
             VALUES ('ge', 2026, ?, 'src-1', ?, ?, 'To', 'Related', 'u', null, null, null, null)",
            world.ns
        ))
        .await
        .unwrap();
    for n in 0..60 {
        let time = at(n);
        let old = bucket(time, OLD_SIZE);
        world
            .session()
            .execute_unpaged(&entity, (old, time, Uuid::new_v4()))
            .await
            .unwrap();
        world
            .session()
            .execute_unpaged(&assoc, (old, time, format!("tgt-{n}")))
            .await
            .unwrap();
        census_add(
            &world,
            &format!("{}:census:entities:stream:ge:2026", world.ns),
            old,
        )
        .await;
        census_add(
            &world,
            &format!("{}:census:associations:stream:ge:src-1:2026", world.ns),
            old,
        )
        .await;
        census_add(
            &world,
            &format!("{}:census:associations:stream:ge:tgt-{n}:2026", world.ns),
            old,
        )
        .await;
    }
    let entities = rebucket(&world, "rebucket-entities", None, 25)
        .await
        .unwrap();
    let associations = rebucket(&world, "rebucket-associations", None, 25)
        .await
        .unwrap();
    assert_eq!(
        (entities.counts.written, associations.counts.written),
        (60, 60)
    );
    // every row has its new bucket
    let new_buckets: BTreeSet<i32> = (0..60).map(|n| bucket(at(n), NEW_SIZE)).collect();
    let rows = world
        .session()
        .query_unpaged(
            format!("SELECT bucket, created FROM {}.associations", world.ns),
            (),
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap();
    for row in rows.rows::<(i32, DateTime<Utc>)>().unwrap() {
        let (b, time) = row.unwrap();
        assert_eq!(b, bucket(time, NEW_SIZE));
    }
    // the entities census, the source key, and every target key hold only new buckets
    let expected = new_buckets.iter().copied().collect::<Vec<_>>();
    assert_eq!(
        members(
            &world,
            &format!("{}:census:entities:stream:ge:2026", world.ns)
        )
        .await,
        expected
    );
    assert_eq!(
        members(
            &world,
            &format!("{}:census:associations:stream:ge:src-1:2026", world.ns)
        )
        .await,
        expected
    );
    for n in 0..60 {
        assert_eq!(
            members(
                &world,
                &format!("{}:census:associations:stream:ge:tgt-{n}:2026", world.ns)
            )
            .await,
            vec![bucket(at(n), NEW_SIZE)],
            "target {n}"
        );
    }
    world.drop_keyspace().await;
}

/// Tags move to the bucket their own tag type's size gives, and the case-insensitive census is
/// rebuilt too
#[tokio::test]
async fn rebucket_tags_uses_each_tag_types_size() {
    // file tags written at the old size, and repo tags already at their configured size
    let world = World::new_full("mr25t").await;
    let tag = world
        .session()
        .prepare(format!(
            "INSERT INTO {}.tags (type, group, year, bucket, key, value, uploaded, item, key_lower, value_lower) \
             VALUES (?, 'gt', 2026, ?, 'Key', 'Value', ?, ?, 'key', 'value')",
            world.ns
        ))
        .await
        .unwrap();
    for n in 0..50 {
        let time = at(n);
        world
            .session()
            .execute_unpaged(
                &tag,
                ("Files", bucket(time, OLD_SIZE), time, format!("f{n}")),
            )
            .await
            .unwrap();
        world
            .session()
            .execute_unpaged(
                &tag,
                ("Repos", bucket(time, NEW_SIZE), time, format!("r{n}")),
            )
            .await
            .unwrap();
        census_add(
            &world,
            &format!("{}:census:tags:stream:Files:gt:Key:Value:2026", world.ns),
            bucket(time, OLD_SIZE),
        )
        .await;
    }
    let out = rebucket(&world, "rebucket-tags", None, 30).await.unwrap();
    // only the file tags moved
    assert_eq!(out.counts.written, 50, "{:?}", out.counts);
    assert!(out.counts.current >= 50, "{:?}", out.counts);
    let rows = world
        .session()
        .query_unpaged(
            format!("SELECT bucket, uploaded FROM {}.tags", world.ns),
            (),
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap();
    for row in rows.rows::<(i32, DateTime<Utc>)>().unwrap() {
        let (b, time) = row.unwrap();
        assert_eq!(b, bucket(time, NEW_SIZE));
    }
    let expected = (0..50)
        .map(|n| bucket(at(n), NEW_SIZE))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    assert_eq!(
        members(
            &world,
            &format!("{}:census:tags:stream:Files:gt:Key:Value:2026", world.ns)
        )
        .await,
        expected
    );
    assert_eq!(
        members(
            &world,
            &format!(
                "{}:census:tags_case_insensitive:stream:Files:gt:key:value:2026",
                world.ns
            )
        )
        .await,
        expected
    );
    assert_eq!(
        recorded(&world, "partition_size.tags.files")
            .await
            .as_deref(),
        Some("1000")
    );
    world.drop_keyspace().await;
}

/// A census checkpoint left behind by an abandoned earlier run must not decide what a later
/// rebucket does: it belongs to the attempt that wrote it
#[tokio::test]
async fn a_stale_census_checkpoint_does_not_skip_a_later_rebuild() {
    // an earlier run left a checkpoint at the end of the census, under another attempt
    let world = World::new_full("mr25s").await;
    seed_files(&world, 100).await;
    let mut conn = world.backends.redis.get().await.unwrap();
    let _: () = redis::cmd("SET")
        .arg(format!("{}:rebucket:files:census", world.ns))
        .arg("{\"attempt\":\"another\",\"progress\":{\"phase\":\"done\"}}")
        .query_async(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    let out = rebucket(&world, "rebucket-files", None, 20).await.unwrap();
    assert_eq!(out.status, MigrationRunStatus::Done);
    // the census holds exactly the new buckets, not the old ones
    let rows = file_rows(&world).await;
    for group in ["ga", "gb"] {
        let expected = rows
            .iter()
            .filter(|(g, _, _)| g == group)
            .map(|(_, b, _)| *b)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let key = format!("{}:census:samples:stream:{group}:2026", world.ns);
        assert_eq!(members(&world, &key).await, expected, "{group}");
    }
    world.drop_keyspace().await;
}

/// The exported token scan reads every row of a declared table, a batch at a time
#[tokio::test]
async fn scan_table_visits_every_row_of_a_declared_table() {
    // sixty entities named n0 to n59
    let world = World::new_full("mr25q").await;
    let entity = world
        .session()
        .prepare(format!(
            "INSERT INTO {}.entities (kind, group, year, bucket, created, id, name, submitter, kind_data, \
             description, image) VALUES ('\"Tool\"', 'gq', 2026, ?, ?, ?, ?, 'u', '{{}}', null, null)",
            world.ns
        ))
        .await
        .unwrap();
    for n in 0..60 {
        let time = at(n);
        world
            .session()
            .execute_unpaged(
                &entity,
                (
                    bucket(time, OLD_SIZE),
                    time,
                    Uuid::new_v4(),
                    format!("n{n}"),
                ),
            )
            .await
            .unwrap();
    }
    // collect the names over several batches
    let ctx = world.ctx(RunMode::Check, 17);
    let mut names = BTreeSet::new();
    let mut batches = 0;
    thorium::models::schema::scan_table(&ctx, "entities", &["name"], |records| {
        batches += 1;
        names.extend(records.iter().filter_map(|record| record.text("name")));
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(
        names,
        (0..60).map(|n| format!("n{n}")).collect::<BTreeSet<_>>()
    );
    assert!(batches > 1, "{batches}");
    world.drop_keyspace().await;
}

/// Moving rows prepares one insert and one delete per table however many rows move, so each moved
/// row costs two requests rather than a prepare beside each execute
#[tokio::test]
async fn moves_prepare_each_statement_once() {
    // 300 rows in two groups at the old size
    let world = World::new_full("mr25p").await;
    seed_files(&world, 300).await;
    let out = rebucket(&world, "rebucket-files", None, 40)
        .await
        .expect("the pass runs");
    assert_eq!(out.counts.written, 300);
    // the one table's insert and delete were each prepared once, not once per row
    assert_eq!(
        thorium::models::schema::prepared_statements(&world.ns),
        2,
        "every moved row prepared its statements again"
    );
    world.drop_keyspace().await;
}
