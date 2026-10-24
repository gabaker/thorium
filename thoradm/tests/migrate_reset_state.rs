//! Tests of `thoradm migrate reset-state`: the refusals that change nothing, the revision row and
//! audit entry it writes, the open passes and pause counts it voids, and the re-upgrade that
//! follows

mod common;

use common::migrate::World;
use common::record::{PASS, at, blank, deployment, history_row, insert, step};
use futures::StreamExt;
use std::collections::BTreeMap;
use std::process::Output;
use thorium::models::schema::scylla::SchemaMigrationIssuesTable;
use thorium::models::schema::scylla::SchemaMigrationRow;
use thorium::models::schema::{
    DeploymentRecord, JsonText, MigrationRecordKind, MigrationRunStatus, PassCheckpoint,
    Projection, RecordStatus, read_status,
};
use thorium::models::upgrades::{Catalog, RESET_STATE_STEP, RevisionId, StepState, UpgradeState};

/// The revision the seeded deployment records
const CURRENT: &str = "2026-10-v02";

/// The revision the data is reset to
const TARGET: &str = "2026-10-v01";

/// A pass at the target's own revision, which a reset keeps
const BELOW_PASS: &str = "below-pass";

/// A finished pass above the target, which a reset keeps
const DONE_PASS: &str = "done-pass";

/// The steps of the current revision, in the order the catalog lists them
fn current_steps() -> Vec<&'static str> {
    // every step of the edge that makes the current revision
    let current = CURRENT.parse::<RevisionId>().expect("a revision");
    Catalog::PRODUCTION
        .edges
        .iter()
        .find(|edge| edge.id == current.as_str())
        .expect("the edge exists")
        .steps
        .iter()
        .map(|step| step.id)
        .collect()
}

/// A worker row of a pass
///
/// # Arguments
///
/// * `pass` - The pass the row belongs to
/// * `attempt` - The attempt the row belongs to
fn worker(pass: &str, attempt: &str) -> SchemaMigrationIssuesTable {
    // a checkpoint taken over the old data
    SchemaMigrationIssuesTable {
        migration: pass.to_owned(),
        transform: "~worker".to_owned(),
        reason: "~".to_owned(),
        worker: "w00".to_owned(),
        attempt: attempt.to_owned(),
        checkpoint: Some(JsonText(PassCheckpoint {
            chain: vec![pass.to_owned()],
            slices: 1,
            position: serde_json::json!({"token": 7}),
            done: false,
            counts: thorium::models::schema::PassCounts::default(),
            report_counts: BTreeMap::new(),
        })),
        lease: Some("fence".to_owned()),
        class: None,
        count: 0,
        first_seen: None,
        last_seen: None,
        samples: std::collections::BTreeSet::new(),
        resolved_count: 0,
        accepted: false,
        updated: at(5),
    }
}

/// Empty both state tables, which the world's apply left holding a record
///
/// # Arguments
///
/// * `world` - The world to empty
async fn empty(world: &World) {
    // the apply recorded the latest revision, which no test here starts from
    for table in ["schema_migrations", "schema_migration_issues"] {
        world
            .session()
            .query_unpaged(format!("TRUNCATE {}.{table}", world.ns), ())
            .await
            .expect("the table truncates");
    }
}

/// Seed a world with a deployment recorded at the current revision, every step of it done, an
/// open pass above the target, a pass at the target, a finished pass above it, and a `Running`
/// entry holding pause counts
///
/// # Arguments
///
/// * `world` - The world to seed
async fn seed(world: &World) {
    // start from empty tables, then seed the current revision with a `Done` entry for each step
    empty(world).await;
    let steps = current_steps();
    let mut rows = deployment(CURRENT, "rs", 2, &steps);
    // the seeded pass is open: running, with a worker row of its attempt
    for row in &mut rows.migrations {
        if row.kind == MigrationRecordKind::Run {
            row.status = Some(MigrationRunStatus::Running);
        }
    }
    rows.issues.clear();
    rows.issues.push(worker(PASS, "attempt-rs"));
    // a pass at the target's revision, and a finished pass above it
    let mut below = blank(MigrationRecordKind::Run, BELOW_PASS);
    below.revision = Some(TARGET.to_owned());
    below.status = Some(MigrationRunStatus::Running);
    below.attempt = Some("attempt-below".to_owned());
    below.started = Some(at(3));
    rows.migrations.push(below);
    rows.issues.push(worker(BELOW_PASS, "attempt-below"));
    let mut done = blank(MigrationRecordKind::Run, DONE_PASS);
    done.revision = Some(CURRENT.to_owned());
    done.status = Some(MigrationRunStatus::Done);
    done.attempt = Some("attempt-done".to_owned());
    done.started = Some(at(3));
    done.finished = Some(at(4));
    rows.migrations.push(done);
    // the step that paused a component recorded its replica count on a `Running` entry
    let mut running = step(CURRENT, "elastic-reindex-keyword-mappings", 30);
    running.outcome = StepState::Running;
    running.paused = BTreeMap::from([("thorium-search-streamer".to_owned(), 3)]);
    let mut row = history_row(&running);
    row.id = row.id.replace("/done", "/running");
    rows.migrations.push(row);
    insert(world.session(), &world.ns, &rows).await;
}

/// Run `migrate reset-state` with some arguments
///
/// # Arguments
///
/// * `world` - The world naming the namespace
/// * `extra` - The arguments after the subcommand
fn reset(world: &World, extra: &[&str]) -> Output {
    // the OS user is the one the audit entry names
    World::run(
        world
            .thoradm()
            .args(["migrate", "reset-state"])
            .args(extra)
            .env("USER", "alice"),
    )
}

/// Read the record as the status Job does
///
/// # Arguments
///
/// * `world` - The world naming the namespace
async fn record(world: &World) -> DeploymentRecord {
    // the same read `schema status` prints
    match read_status(world.session(), &world.conf, &Catalog::PRODUCTION)
        .await
        .expect("the record reads")
    {
        RecordStatus::Recorded { record, .. } => record,
        other => panic!("expected a record, read {other:?}"),
    }
}

/// Every row of one kind of `schema_migrations`, as `(id, row)` pairs
///
/// # Arguments
///
/// * `world` - The world naming the namespace
/// * `kind` - The kind to read
async fn rows(world: &World, kind: MigrationRecordKind) -> Vec<SchemaMigrationRow> {
    // the projection every reader uses
    let columns = <SchemaMigrationRow as Projection>::COLUMNS.join(", ");
    let mut stream = world
        .session()
        .query_iter(
            format!(
                "SELECT {columns} FROM {}.schema_migrations WHERE kind = ?",
                world.ns
            ),
            (kind.as_str(),),
        )
        .await
        .expect("the rows read")
        .rows_stream::<SchemaMigrationRow>()
        .expect("typed rows");
    let mut rows = Vec::new();
    while let Some(row) = stream.next().await {
        rows.push(row.expect("a row"));
    }
    rows
}

/// A thoradm stderr, for failure messages
///
/// # Arguments
///
/// * `output` - The command's output
fn stderr(output: &Output) -> String {
    // lossy text of the error stream
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Without `--i-restored-data` the command refuses and changes nothing: the revision, the history,
/// the pass rows, and the worker rows are what they were, and a target no revision names is
/// refused the same way
#[tokio::test]
async fn reset_without_the_flag_refuses_and_changes_nothing() {
    // a seeded deployment
    let world = World::new("rv10a").await;
    seed(&world).await;
    let before = record(&world).await;
    let passes = rows(&world, MigrationRecordKind::Run).await.len();
    // the command without the flag, and with a target the catalog doesn't know
    let refused = reset(&world, &["--to", TARGET]);
    assert_ne!(refused.status.code(), Some(0), "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("--i-restored-data"),
        "{}",
        stderr(&refused)
    );
    let unknown = reset(&world, &["--to", "2026-10-v99", "--i-restored-data"]);
    assert_ne!(unknown.status.code(), Some(0), "{}", stderr(&unknown));
    // nothing changed
    let after = record(&world).await;
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.history_len, before.history_len);
    assert_eq!(after.history, before.history);
    assert_eq!(rows(&world, MigrationRecordKind::Run).await.len(), passes);
    let attempt = world.store.rows(PASS, "attempt-rs").await.expect("rows");
    assert_eq!(attempt.0.len(), 1);
    world.drop_keyspace().await;
}

/// With the flag the command writes the revision row and one `reset-state` entry naming the
/// change, the thoradm version, and the OS user; it voids the open pass above the target with its
/// worker rows and the recorded pause counts, and keeps the pass at the target, the finished pass,
/// and the old `Done` entries
#[tokio::test]
async fn reset_writes_the_revision_and_one_audit_entry_and_voids_open_work() {
    // a seeded deployment
    let world = World::new("rv10b").await;
    seed(&world).await;
    let before = record(&world).await;
    // reset it to the target
    let output = reset(&world, &["--to", TARGET, "--i-restored-data"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    // the revision row names the target, and exactly one entry was added
    let after = record(&world).await;
    assert_eq!(after.revision.as_str(), TARGET);
    assert_eq!(after.history_len, before.history_len + 1);
    let entries = after
        .history
        .iter()
        .filter(|applied| applied.step == RESET_STATE_STEP)
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 1, "{:?}", after.history);
    let entry = entries[0];
    assert_eq!(entry.outcome, StepState::Done);
    assert_eq!(entry.revision, TARGET);
    assert_eq!(
        entry.message.as_deref(),
        Some(format!("from {CURRENT} to {TARGET}").as_str())
    );
    let by = entry.by.as_deref().expect("who ran it");
    assert!(by.contains(env!("CARGO_PKG_VERSION")), "{by}");
    assert!(by.contains("alice"), "{by}");
    assert!(entry.at.is_some());
    // the old `Done` entries are all still there
    for name in current_steps() {
        assert!(
            after
                .history
                .iter()
                .any(|applied| applied.step == name && applied.outcome == StepState::Done),
            "{name}"
        );
    }
    // the open pass above the target is gone with its worker rows; the others stay
    assert_eq!(world.store.pass_row(PASS).await.expect("a read"), None);
    let voided = world.store.rows(PASS, "attempt-rs").await.expect("rows");
    assert_eq!(voided.0.len(), 0);
    assert!(
        world
            .store
            .pass_row(BELOW_PASS)
            .await
            .expect("a read")
            .is_some()
    );
    let kept = world
        .store
        .rows(BELOW_PASS, "attempt-below")
        .await
        .expect("rows");
    assert_eq!(kept.0.len(), 1);
    assert_eq!(
        world.status(DONE_PASS).await,
        Some(MigrationRunStatus::Done)
    );
    // the pause counts of the revision above the target are void
    let running = after
        .history
        .iter()
        .find(|applied| applied.outcome == StepState::Running)
        .expect("the running entry stays");
    assert!(running.paused.is_empty(), "{:?}", running.paused);
    world.drop_keyspace().await;
}

/// After a reset every step of the revisions above the target counts as unfinished again although
/// its old `Done` entry remains, so the re-upgrade plans and runs them all
#[tokio::test]
async fn the_re_upgrade_after_a_reset_runs_every_step_above_the_target() {
    // a seeded deployment, before the reset nothing above the baseline is left to do
    let world = World::new("rv10c").await;
    seed(&world).await;
    let current = CURRENT.parse::<RevisionId>().expect("a revision");
    let target = TARGET.parse::<RevisionId>().expect("a revision");
    let edge_steps = |record: &DeploymentRecord| {
        let mut state = UpgradeState::new(record.revision.clone(), "test");
        state.applied.clone_from(&record.history);
        let path = Catalog::PRODUCTION
            .path(&record.revision, &current)
            .expect("a path");
        path.iter()
            .flat_map(|edge| Catalog::PRODUCTION.edge_steps(&state, edge))
            .map(|step| step.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(edge_steps(&record(&world).await), Vec::<&str>::new());
    // reset to the baseline
    let output = reset(&world, &["--to", TARGET, "--i-restored-data"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    // the plan from the target holds every step above it, and the status says none finished
    let after = record(&world).await;
    assert_eq!(after.revision, target);
    assert_eq!(edge_steps(&after), current_steps());
    assert!(
        after
            .finished
            .iter()
            .all(|finished| finished.revision != current),
        "{:?}",
        after.finished
    );
    world.drop_keyspace().await;
}

/// While another writer holds the namespace's lease the command refuses and changes nothing, and
/// once the lease is released the same command succeeds
#[tokio::test]
async fn reset_refuses_while_a_writer_holds_the_lease() {
    // a seeded deployment and a lease held by a running Job
    let world = World::new("rv10d").await;
    seed(&world).await;
    let before = record(&world).await;
    let lease = world.lease("a-running-job").await;
    let refused = reset(&world, &["--to", TARGET, "--i-restored-data"]);
    assert_ne!(refused.status.code(), Some(0), "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("a-running-job"),
        "{}",
        stderr(&refused)
    );
    // nothing changed, not even the pass above the target
    let after = record(&world).await;
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.history_len, before.history_len);
    assert!(world.store.pass_row(PASS).await.expect("a read").is_some());
    // with the lease released the reset runs, and the lease is free afterwards
    lease.release().await;
    let output = reset(&world, &["--to", TARGET, "--i-restored-data"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(record(&world).await.revision.as_str(), TARGET);
    world.lease("next-writer").await.release().await;
    world.drop_keyspace().await;
}

/// A namespace whose record holds no revision has nothing to reset: the command refuses and
/// writes nothing
#[tokio::test]
async fn reset_refuses_a_namespace_with_no_recorded_revision() {
    // the state tables, emptied of the record the world's apply wrote
    let world = World::new("rv10e").await;
    empty(&world).await;
    let refused = reset(&world, &["--to", TARGET, "--i-restored-data"]);
    assert_ne!(refused.status.code(), Some(0), "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("records no revision"),
        "{}",
        stderr(&refused)
    );
    assert!(
        rows(&world, MigrationRecordKind::History).await.is_empty(),
        "no history is written"
    );
    assert!(
        rows(&world, MigrationRecordKind::Revision).await.is_empty(),
        "no revision is written"
    );
    world.drop_keyspace().await;
}
