//! Tests of the `provenance-to-legacy-tools` migration: provenance rows of the three kinds that
//! keep tool names move those names from `tools` to `legacy_tools` inside `kind_data`, nothing
//! else about a row changes, a check writes nothing, a rerun finds nothing to do, and an
//! interrupted run resumes

mod common;

use chrono::{DateTime, Duration, TimeZone, Utc};
use common::migrate::World;
use scylla::value::CqlValue;
use std::collections::BTreeMap;
use thoradm::migrate::registry::{MIGRATIONS, MigrationDef};
use thorium::client::migrations::{ProvenanceClass, ProvenanceRow, classify};
use thorium::models::EntityMetadata;
use thorium::models::schema::{
    Batch, Bucket, CheckRun, MigrateError, MigrationRunStatus, MigrationRunner, Outcome,
    PassOutcome, RunContext, RunMode, RunOptions, Slice, check_pass, run_pass,
};
use thorium::models::upgrades::{IssueReason, Record};
use uuid::Uuid;

/// The migration under test
fn def() -> &'static MigrationDef {
    // the migration is registered like every other
    MIGRATIONS
        .iter()
        .find(|def| def.id == "provenance-to-legacy-tools")
        .expect("provenance-to-legacy-tools is registered")
}

/// The time of the nth row: a row every 37 seconds from a March morning
///
/// # Arguments
///
/// * `n` - The row's number
fn at(n: i64) -> DateTime<Utc> {
    // spread the rows so they land in different buckets
    Utc.with_ymd_and_hms(2026, 3, 1, 8, 0, 0).unwrap() + Duration::seconds(n * 37)
}

/// One stored entity row as the tests compare it
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stored {
    /// The `kind` column, as JSON text
    kind: String,
    /// The `group` column
    group: String,
    /// The `name` column
    name: String,
    /// The `submitter` column
    submitter: String,
    /// The `kind_data` column
    kind_data: String,
}

/// Insert one entity row in the table the API reads
///
/// # Arguments
///
/// * `world` - The test world
/// * `n` - The row's number, which sets its creation time
/// * `kind` - The entity kind's name
/// * `group` - The row's group
/// * `kind_data` - The stored kind data
async fn seed(world: &World, n: i64, kind: &str, group: &str, kind_data: &str) -> Uuid {
    // the bucket the configured size gives, like any row the API writes
    let created = at(n);
    let size = world.backends.partition_sizes["entities"];
    let bucket = i32::try_from(Bucket::of(created, 2026, size).unwrap().get()).unwrap();
    let id = Uuid::new_v4();
    let insert = world
        .session()
        .prepare(format!(
            "INSERT INTO {}.entities (kind, group, year, bucket, created, id, name, submitter, \
             kind_data, description, image) VALUES (?, ?, 2026, ?, ?, ?, ?, 'io', ?, null, null)",
            world.ns
        ))
        .await
        .expect("the insert prepares");
    // the kind column holds the kind as JSON text
    world
        .session()
        .execute_unpaged(
            &insert,
            (
                format!("\"{kind}\""),
                group,
                bucket,
                created,
                id,
                format!("row-{n}"),
                kind_data,
            ),
        )
        .await
        .expect("a row inserts");
    id
}

/// Every entity row by id
///
/// # Arguments
///
/// * `world` - The test world
async fn stored(world: &World) -> BTreeMap<Uuid, Stored> {
    // a full scan of the small table
    let rows = world
        .session()
        .query_unpaged(
            format!(
                "SELECT id, kind, group, name, submitter, kind_data FROM {}.entities",
                world.ns
            ),
            (),
        )
        .await
        .expect("the table reads")
        .into_rows_result()
        .expect("rows");
    rows.rows::<(Uuid, String, String, String, String, String)>()
        .expect("typed rows")
        .map(|row| {
            let (id, kind, group, name, submitter, kind_data) = row.expect("a row");
            (
                id,
                Stored {
                    kind,
                    group,
                    name,
                    submitter,
                    kind_data,
                },
            )
        })
        .collect()
}

/// Every row of the by-id view, with its kind data
///
/// # Arguments
///
/// * `world` - The test world
async fn view(world: &World) -> BTreeMap<Uuid, String> {
    // the view follows the table's non-key column
    let rows = world
        .session()
        .query_unpaged(
            format!("SELECT id, kind_data FROM {}.entities_by_id", world.ns),
            (),
        )
        .await
        .expect("the view reads")
        .into_rows_result()
        .expect("rows");
    rows.rows::<(Uuid, String)>()
        .expect("typed rows")
        .map(|row| row.expect("a row"))
        .collect()
}

/// Re-serialize a stored row the way the API reads it
///
/// # Arguments
///
/// * `kind_data` - The stored kind data
fn shim(kind_data: &str) -> String {
    // read through the API's own deserialization and write the typed value back
    let metadata = serde_json::from_str::<EntityMetadata>(kind_data).unwrap();
    serde_json::to_string(&metadata).unwrap()
}

/// The rows a test seeds, by what each is
struct Seeded {
    /// Names only in `tools`
    names: Uuid,
    /// A tool instance id only
    ids: Uuid,
    /// A name and a tool instance id, in another group and kind
    mixed: Uuid,
    /// Names already in `legacy_tools`
    moved: Uuid,
    /// Kind data that doesn't parse
    broken: Uuid,
    /// A kind that holds no provenance
    other: Uuid,
    /// A process tree with a name, in the second group
    tree: Uuid,
}

/// Seed one row of each shape the migration meets
///
/// # Arguments
///
/// * `world` - The test world
async fn seed_all(world: &World) -> Seeded {
    // a tool instance id stored as a string
    let instance = Uuid::new_v4();
    Seeded {
        names: seed(
            world,
            0,
            "FileSystem",
            "ga",
            r#"{"FileSystem":{"sha256":"abc","tools":["binwalk","7zip"]}}"#,
        )
        .await,
        ids: seed(
            world,
            1,
            "FileSystem",
            "ga",
            &format!(r#"{{"FileSystem":{{"sha256":"abc","tools":["{instance}"]}}}}"#),
        )
        .await,
        mixed: seed(
            world,
            2,
            "DecompiledFunction",
            "ga",
            &format!(
                r#"{{"DecompiledFunction":{{"address":16,"tools":["ghidra","{instance}"],"content":"x"}}}}"#
            ),
        )
        .await,
        moved: seed(
            world,
            3,
            "WindowsProcessTree",
            "ga",
            &format!(r#"{{"WindowsProcessTree":{{"tools":["{instance}"],"legacy_tools":["volatility"]}}}}"#),
        )
        .await,
        broken: seed(world, 4, "FileSystem", "ga", "{not json").await,
        other: seed(world, 5, "Other", "ga", r#""Other""#).await,
        tree: seed(
            world,
            6,
            "WindowsProcessTree",
            "gb",
            r#"{"WindowsProcessTree":{"tools":["rekall"]}}"#,
        )
        .await,
    }
}

/// A world with the whole declared schema, so the entities table and its view exist
///
/// # Arguments
///
/// * `suffix` - What makes the test's keyspace unique
async fn world(suffix: &str) -> World {
    // every declared table and view
    World::new_full(suffix).await
}

/// Check the migration and return its run
///
/// # Arguments
///
/// * `world` - The test world
async fn check(world: &World) -> CheckRun {
    // a check never stops early and writes nothing
    let ctx = world.ctx(RunMode::Check, 100);
    check_pass(
        def().runner,
        &World::spec(def()),
        &RunOptions::default(),
        &ctx,
    )
    .await
    .expect("the check runs")
}

/// Run the migration to the end, optionally with a runner wrapped around it
///
/// # Arguments
///
/// * `world` - The test world
/// * `runner` - The runner to run
/// * `batch_rows` - The rows of each batch
async fn run_with(
    world: &World,
    runner: &'static dyn MigrationRunner,
    batch_rows: usize,
) -> Result<PassOutcome, MigrateError> {
    // the writer lease every run holds
    let ctx = world.ctx(RunMode::Run, batch_rows);
    let lease = world.lease("test").await;
    let opts = RunOptions {
        slices: 2,
        ..RunOptions::default()
    };
    let out = run_pass(
        runner,
        &World::spec(def()),
        &opts,
        &ctx,
        &world.store,
        &lease,
    )
    .await;
    lease.release().await;
    out
}

/// Run the registered migration to the end
///
/// # Arguments
///
/// * `world` - The test world
/// * `batch_rows` - The rows of each batch
async fn run(world: &World, batch_rows: usize) -> Result<PassOutcome, MigrateError> {
    // the registered runner as it is
    run_with(world, def().runner, batch_rows).await
}

/// A runner that cancels the run after a few batches
struct Stopper {
    /// The migration
    inner: &'static dyn MigrationRunner,
    /// Batches read
    seen: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl MigrationRunner for Stopper {
    /// Split the family as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `n` - How many slices to make
    /// * `ctx` - The backends, settings, and mode of this run
    async fn slices(&self, n: u32, ctx: &RunContext) -> Result<Vec<Slice>, MigrateError> {
        self.inner.slices(n, ctx).await
    }

    /// Read a batch as the wrapped migration does, then cancel the run after the first
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
        if self.seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 1 {
            ctx.cancel.cancel();
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

/// A check classifies every kind of row and writes nothing: the table and its view are the same
/// before and after
#[tokio::test]
async fn check_classifies_every_shape_and_writes_nothing() {
    // seven rows: three to move, three current, one that can't be read
    let world = world("mp1").await;
    seed_all(&world).await;
    let before = stored(&world).await;
    let before_view = view(&world).await;
    let run = check(&world).await;
    let counts = run.report.counts;
    assert_eq!(counts.read, 7, "{counts:?}");
    assert_eq!(counts.migratable, 3, "{counts:?}");
    assert_eq!(counts.current, 3, "{counts:?}");
    assert_eq!(counts.unrecognized, 1, "{counts:?}");
    // the broken row has the reason code of its parse failure and no content in its report
    let codes = run
        .report
        .reasons
        .iter()
        .map(|reason| (reason.code.clone(), reason.count))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(codes.get("malformed_json:kind_data"), Some(&1), "{codes:?}");
    // nothing was written
    assert_eq!(stored(&world).await, before);
    assert_eq!(view(&world).await, before_view);
    world.drop_keyspace().await;
}

/// A run moves the names of the rows that hold them and changes nothing else: every other column
/// of those rows, and every other row, is exactly as it was, and the by-id view follows
#[tokio::test]
async fn run_moves_names_and_changes_nothing_else() {
    let world = world("mp2").await;
    let rows = seed_all(&world).await;
    let before = stored(&world).await;
    let out = run(&world, 100).await.expect("the pass runs");
    // the broken row is an issue within the step's limit
    assert_eq!(out.status, MigrationRunStatus::DoneWithIssues);
    assert_eq!(out.counts.written, 3, "{:?}", out.counts);
    let after = stored(&world).await;
    assert_eq!(after.len(), before.len());
    // the rows with names hold the API's reading of them; nothing else about them changed
    for id in [rows.names, rows.mixed, rows.tree] {
        let (was, now) = (&before[&id], &after[&id]);
        assert_eq!(now.kind_data, shim(&was.kind_data), "{was:?}");
        assert_eq!(
            Stored {
                kind_data: String::new(),
                ..now.clone()
            },
            Stored {
                kind_data: String::new(),
                ..was.clone()
            }
        );
        // no stored tool string is a name any more
        let row = ProvenanceRow {
            key: id.to_string(),
            kind: now.kind.trim_matches('"').to_owned(),
            kind_data: now.kind_data.clone(),
        };
        assert_eq!(classify(&row), ProvenanceClass::Current, "{now:?}");
    }
    // every other row is byte for byte what it was
    for id in [rows.ids, rows.moved, rows.broken, rows.other] {
        assert_eq!(after[&id], before[&id], "{id}");
    }
    // the view holds the same kind data as the table
    let view = view(&world).await;
    for (id, row) in &after {
        assert_eq!(view[id], row.kind_data, "{id}");
    }
    world.drop_keyspace().await;
}

/// Running again finds nothing to move, writes nothing, and leaves every row as it was
#[tokio::test]
async fn a_rerun_finds_nothing_to_do() {
    let world = world("mp3").await;
    seed_all(&world).await;
    run(&world, 100).await.expect("the first pass runs");
    let after_first = stored(&world).await;
    let out = run(&world, 100).await.expect("the second pass runs");
    assert_eq!(out.counts.written, 0, "{:?}", out.counts);
    assert_eq!(out.counts.migratable, 0, "{:?}", out.counts);
    assert_eq!(out.counts.current, 6, "{:?}", out.counts);
    assert_eq!(out.counts.unrecognized, 1, "{:?}", out.counts);
    assert_eq!(stored(&world).await, after_first);
    world.drop_keyspace().await;
}

/// A run stopped between batches resumes and ends with every row moved
#[tokio::test]
async fn an_interrupted_run_resumes() {
    let world = world("mp4").await;
    let rows = seed_all(&world).await;
    let before = stored(&world).await;
    // the first pass is cancelled after its first batches, with two rows to a batch
    let stopper: &'static Stopper = Box::leak(Box::new(Stopper {
        inner: def().runner,
        seen: std::sync::atomic::AtomicUsize::new(0),
    }));
    let first = run_with(&world, stopper, 2).await;
    // an interrupted pass is stopped and resumable, not failed
    if let Ok(out) = &first {
        assert!(
            out.interrupted || out.status == MigrationRunStatus::Running,
            "{out:?}"
        );
    }
    // the second pass finishes the work
    let out = run(&world, 2).await.expect("the pass resumes");
    assert!(matches!(
        out.status,
        MigrationRunStatus::DoneWithIssues | MigrationRunStatus::Done
    ));
    let after = stored(&world).await;
    for id in [rows.names, rows.mixed, rows.tree] {
        assert_eq!(after[&id].kind_data, shim(&before[&id].kind_data), "{id}");
    }
    for id in [rows.ids, rows.moved, rows.broken, rows.other] {
        assert_eq!(after[&id], before[&id], "{id}");
    }
    world.drop_keyspace().await;
}

/// Verify accepts a row that has been moved and refuses one still holding names
#[tokio::test]
async fn verify_accepts_moved_rows_and_refuses_unmoved_ones() {
    let world = world("mp5").await;
    seed_all(&world).await;
    let ctx = world.ctx(RunMode::Run, 100);
    // a record as the scan reads it
    let record = |kind: &str, kind_data: &str| Record::Scylla {
        key: "k".to_owned(),
        columns: BTreeMap::from([
            (
                "kind".to_owned(),
                Some(CqlValue::Text(format!("\"{kind}\""))),
            ),
            (
                "kind_data".to_owned(),
                Some(CqlValue::Text(kind_data.to_owned())),
            ),
        ]),
    };
    let unmoved = r#"{"FileSystem":{"sha256":"abc","tools":["binwalk"]}}"#;
    let moved = shim(unmoved);
    let verified: Result<(), IssueReason> = def()
        .runner
        .verify(&record("FileSystem", &moved), &ctx)
        .await;
    assert!(verified.is_ok(), "{verified:?}");
    let refused = def()
        .runner
        .verify(&record("FileSystem", unmoved), &ctx)
        .await;
    assert!(refused.is_err(), "a row still holding names doesn't verify");
    world.drop_keyspace().await;
}

/// A row flagged by a run, fixed by hand, and named by its issue key with `--objects` is read
/// again and leaves its issue group
#[tokio::test]
async fn an_objects_run_rereads_a_fixed_row_and_credits_its_group() {
    // one row whose kind data doesn't parse, which the run flags
    let world = world("mp_objects").await;
    let broken = seed(&world, 4, "FileSystem", "ga", "{not json").await;
    let first = run(&world, 100).await.expect("the pass runs");
    assert_eq!(
        first.status,
        MigrationRunStatus::DoneWithIssues,
        "{first:?}"
    );
    let key = first.groups[0]
        .samples
        .iter()
        .next()
        .expect("the group lists the row")
        .clone();
    assert!(key.ends_with(&broken.to_string()), "{key}");
    // the row's kind data is fixed by hand
    let size = world.backends.partition_sizes["entities"];
    let bucket = i32::try_from(Bucket::of(at(4), 2026, size).unwrap().get()).unwrap();
    world
        .session()
        .query_unpaged(
            format!(
                "UPDATE {}.entities SET kind_data = ? WHERE kind = ? AND group = ? AND year = 2026 \
                 AND bucket = ? AND created = ? AND id = ?",
                world.ns
            ),
            (
                r#"{"FileSystem":{"sha256":"abc","tools":[]}}"#,
                "\"FileSystem\"",
                "ga",
                bucket,
                at(4),
                broken,
            ),
        )
        .await
        .expect("the row updates");
    // naming the row by the key its group lists reads it again and credits the group
    let ctx = world.ctx(RunMode::Run, 100);
    let lease = world.lease("test").await;
    let opts = RunOptions {
        slices: 2,
        objects: vec![key.clone()],
        reason: None,
    };
    let out = run_pass(
        def().runner,
        &World::spec(def()),
        &opts,
        &ctx,
        &world.store,
        &lease,
    )
    .await
    .expect("the objects run");
    lease.release().await;
    assert_eq!(out.counts.read, 1, "{:?}", out.counts);
    let group = &out.groups[0];
    assert_eq!((group.count, group.resolved), (0, 1), "{group:?}");
    assert!(!group.samples.contains(&key), "{group:?}");
    world.drop_keyspace().await;
}
