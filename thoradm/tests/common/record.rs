//! Rows of the migration-state tables, seeded into a namespace and read back
//!
//! The seeded rows cover every kind a backup carries, each with every cell a test checks set, plus
//! a `job_result` row a backup leaves out.

use super::harness::Harness;
use chrono::{DateTime, TimeZone, Utc};
use futures::StreamExt;
use scylla::client::session::Session;
use std::collections::{BTreeMap, BTreeSet};
use thoradm::backup::schema::{BackupSchemaRecord, issue_row, migrations_row};
use thorium::models::schema::scylla::{
    SchemaMigrationIssueRow, SchemaMigrationIssuesTable, SchemaMigrationRow, SchemaMigrationsTable,
};
use thorium::models::schema::{
    JsonText, MigrationRecordKind, MigrationRunStatus, ObjectClass, PassCheckpoint, Projection,
    ScyllaObject, TableStatements, with_keyspace,
};
use thorium::models::upgrades::{AppliedStep, StepState};

/// The pass the seeded run and issue rows belong to
pub const PASS: &str = "elastic-reindex-keyword-mappings";

/// The rows of both migration-state tables
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rows {
    /// The `schema_migrations` rows
    pub migrations: Vec<SchemaMigrationsTable>,
    /// The `schema_migration_issues` rows
    pub issues: Vec<SchemaMigrationIssuesTable>,
}

impl Rows {
    /// Finds a `schema_migrations` row
    ///
    /// # Arguments
    ///
    /// * `kind` - The row's kind
    /// * `id` - The row's id
    #[must_use]
    pub fn get(&self, kind: MigrationRecordKind, id: &str) -> Option<&SchemaMigrationsTable> {
        // the table holds one row of a kind and id
        self.migrations
            .iter()
            .find(|row| row.kind == kind && row.id == id)
    }

    /// The rows of one kind, in id order
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind
    #[must_use]
    pub fn kind(&self, kind: MigrationRecordKind) -> Vec<&SchemaMigrationsTable> {
        // keep the kind's rows, ordered by id
        let mut rows: Vec<_> = self
            .migrations
            .iter()
            .filter(|row| row.kind == kind)
            .collect();
        rows.sort_by(|left, right| left.id.cmp(&right.id));
        rows
    }

    /// The ids of every row, as `kind/id`, for comparing which rows exist
    #[must_use]
    pub fn ids(&self) -> BTreeSet<String> {
        // one name per row
        self.migrations
            .iter()
            .map(|row| format!("{}/{}", row.kind.as_str(), row.id))
            .collect()
    }

    /// Sorts every row by its key, so two sets of rows compare equal when they hold the same
    pub fn sort(&mut self) {
        // order the rows the way the record does
        self.migrations.sort_by(|left, right| {
            (left.kind.as_str(), &left.id).cmp(&(right.kind.as_str(), &right.id))
        });
        self.issues.sort_by(|left, right| {
            (&left.migration, &left.transform, &left.reason, &left.worker).cmp(&(
                &right.migration,
                &right.transform,
                &right.reason,
                &right.worker,
            ))
        });
    }
}

/// A fixed millisecond-aligned time, so a row compares equal after Scylla stores it
///
/// # Arguments
///
/// * `minute` - Minutes past the start of the day
#[must_use]
pub fn at(minute: u32) -> DateTime<Utc> {
    // Scylla keeps milliseconds, so none are finer than that
    Utc.with_ymd_and_hms(2026, 10, 1, 0, minute % 60, 0)
        .unwrap()
}

/// A `schema_migrations` row with only its key and its write time set
///
/// # Arguments
///
/// * `kind` - The row's kind
/// * `id` - The row's id
#[must_use]
pub fn blank(kind: MigrationRecordKind, id: &str) -> SchemaMigrationsTable {
    // every optional cell unset
    SchemaMigrationsTable {
        kind,
        id: id.to_owned(),
        fingerprint: None,
        apply_key: None,
        layouts: BTreeMap::new(),
        revision: None,
        version: None,
        entry: None,
        result: None,
        value: None,
        status: None,
        attempt: None,
        lease: None,
        started: None,
        finished: None,
        job: None,
        backup: None,
        counts: None,
        report: None,
        updated: at(1),
    }
}

/// One applied-history entry
///
/// # Arguments
///
/// * `revision` - The entry's revision
/// * `step` - The step
/// * `minute` - When it finished, in minutes past the start of the day
#[must_use]
pub fn step(revision: &str, step: &str, minute: u32) -> AppliedStep {
    // a finished step applied by a test
    AppliedStep {
        revision: revision.to_owned(),
        step: step.to_owned(),
        outcome: StepState::Done,
        message: Some("seeded".to_owned()),
        at: Some(at(minute).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)),
        by: Some("test".to_owned()),
        ..AppliedStep::default()
    }
}

/// Builds a history row the way the applied history keys it
///
/// # Arguments
///
/// * `entry` - The entry the row holds
#[must_use]
pub fn history_row(entry: &AppliedStep) -> SchemaMigrationsTable {
    // `<at>/<revision>/<step>/<outcome>` sorts oldest first
    let id = format!(
        "{}/{}/{}/done",
        entry.at.clone().unwrap_or_default(),
        entry.revision,
        entry.step
    );
    let mut row = blank(MigrationRecordKind::History, &id);
    row.entry = Some(JsonText(entry.clone()));
    row
}

/// Seeds every kind of row a deployment at a revision holds
///
/// # Arguments
///
/// * `revision` - The revision the deployment is at
/// * `tag` - Text that makes this deployment's values differ from another's
/// * `users_layout` - The layout version of the Redis `users` family
/// * `steps` - The steps its history records
#[must_use]
pub fn deployment(revision: &str, tag: &str, users_layout: u32, steps: &[&str]) -> Rows {
    let mut rows = Rows::default();
    // the backend rows: one per backend, the deployment's with its trigger key
    for backend in ["scylla", "elastic", "s3"] {
        let mut row = blank(MigrationRecordKind::Backend, backend);
        row.fingerprint = Some(format!("fp-{backend}-{tag}"));
        rows.migrations.push(row);
    }
    let mut redis = blank(MigrationRecordKind::Backend, "redis");
    redis.fingerprint = Some(format!("fp-redis-{tag}"));
    redis.layouts = BTreeMap::from([("users".to_owned(), users_layout), ("groups".to_owned(), 1)]);
    rows.migrations.push(redis);
    let mut whole = blank(MigrationRecordKind::Backend, "deployment");
    whole.fingerprint = Some(format!("fp-deployment-{tag}"));
    whole.apply_key = Some(format!("key-{tag}"));
    rows.migrations.push(whole);
    // the revision and its history
    let mut current = blank(MigrationRecordKind::Revision, "deployment");
    current.revision = Some(revision.to_owned());
    current.version = Some("1.8.1".to_owned());
    rows.migrations.push(current);
    for (index, name) in steps.iter().enumerate() {
        let minute = u32::try_from(index).unwrap() + 2;
        rows.migrations
            .push(history_row(&step(revision, name, minute)));
    }
    // one finished migration pass, with its worker row and one issue group
    let mut run = blank(MigrationRecordKind::Run, PASS);
    run.revision = Some(revision.to_owned());
    run.status = Some(MigrationRunStatus::DoneWithIssues);
    run.attempt = Some(format!("attempt-{tag}"));
    run.lease = Some("lease-holder".to_owned());
    run.started = Some(at(3));
    run.finished = Some(at(4));
    run.job = Some(format!("job-{tag}"));
    run.backup = Some("approved-backup".to_owned());
    rows.migrations.push(run);
    // the data-shaping record: a size the restoring cluster doesn't use, and a password identity
    let mut size = blank(MigrationRecordKind::Config, "partition_size.files");
    size.value = Some("7777".to_owned());
    rows.migrations.push(size);
    // a consumed lossy-approval code, and a history entry a restore once archived
    let mut code = blank(MigrationRecordKind::ConsumedCode, &format!("CODE-{tag}"));
    code.entry = Some(JsonText(step(revision, "lossy-step", 5)));
    rows.migrations.push(code);
    let mut archived = blank(
        MigrationRecordKind::ArchivedHistory,
        &format!("{tag}-restore/old"),
    );
    archived.entry = Some(JsonText(step(revision, "older-step", 6)));
    rows.migrations.push(archived);
    // a Job's result, which a backup leaves out
    let mut result = blank(MigrationRecordKind::JobResult, &format!("uid-{tag}"));
    result.result = Some(JsonText(serde_json::json!({"outcome": "done"})));
    rows.migrations.push(result);
    // the pass's worker row and one issue group
    rows.issues.push(SchemaMigrationIssuesTable {
        migration: PASS.to_owned(),
        transform: "~worker".to_owned(),
        reason: "~".to_owned(),
        worker: "w00".to_owned(),
        attempt: format!("attempt-{tag}"),
        checkpoint: Some(JsonText(PassCheckpoint {
            chain: vec![PASS.to_owned()],
            slices: 4,
            position: serde_json::json!({"token": 42}),
            done: true,
            counts: thorium::models::schema::PassCounts {
                read: 10,
                written: 7,
                flagged: 3,
                ..Default::default()
            },
            report_counts: BTreeMap::new(),
        })),
        lease: Some("fence-1".to_owned()),
        class: None,
        count: 0,
        first_seen: None,
        last_seen: None,
        samples: BTreeSet::new(),
        resolved_count: 0,
        accepted: false,
        updated: at(4),
    });
    rows.issues.push(SchemaMigrationIssuesTable {
        migration: PASS.to_owned(),
        transform: PASS.to_owned(),
        reason: "unrecognized:mapping".to_owned(),
        worker: "w00".to_owned(),
        attempt: format!("attempt-{tag}"),
        checkpoint: None,
        lease: None,
        class: Some(ObjectClass::Unrecognized),
        count: 3,
        first_seen: Some(at(3)),
        last_seen: Some(at(4)),
        samples: BTreeSet::from(["a/b".to_owned(), "c/d".to_owned()]),
        resolved_count: 1,
        accepted: true,
        updated: at(4),
    });
    rows
}

/// Empties both migration-state tables of the harness's namespace
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
pub async fn clear(harness: &Harness) {
    // truncate each table
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    for table in [
        <SchemaMigrationsTable as ScyllaObject>::NAME,
        <SchemaMigrationIssuesTable as ScyllaObject>::NAME,
    ] {
        scylla
            .query_unpaged(format!("TRUNCATE {ns}.{table}"), ())
            .await
            .unwrap_or_else(|err| panic!("failed to truncate {table}: {err}"));
    }
}

/// Writes rows into both migration-state tables of the harness's namespace
///
/// # Arguments
///
/// * `scylla` - The session to write with
/// * `namespace` - The keyspace
/// * `rows` - The rows to write
pub async fn insert(scylla: &Session, namespace: &str, rows: &Rows) {
    // prepare each table's generated insert and bind every row by column name
    let migrations = scylla
        .prepare(with_keyspace(SchemaMigrationsTable::INSERT, namespace))
        .await
        .unwrap();
    for row in &rows.migrations {
        scylla.execute_unpaged(&migrations, row).await.unwrap();
    }
    let issues = scylla
        .prepare(with_keyspace(SchemaMigrationIssuesTable::INSERT, namespace))
        .await
        .unwrap();
    for row in &rows.issues {
        scylla.execute_unpaged(&issues, row).await.unwrap();
    }
}

/// Replaces the harness namespace's migration-state rows with some rows
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `rows` - The rows the tables should hold
pub async fn set(harness: &Harness, rows: &Rows) {
    // empty both tables, then write the rows
    clear(harness).await;
    let scylla = harness.scylla().await;
    insert(&scylla, &harness.conf.thorium.namespace, rows).await;
}

/// Reads every row of both migration-state tables of the harness's namespace, sorted by key
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
pub async fn read(harness: &Harness) -> Rows {
    // scan each table through the projection its readers use
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    let columns = <SchemaMigrationRow as Projection>::COLUMNS.join(", ");
    let mut rows = Rows::default();
    // only the kinds this build knows, one partition each, since a row of another kind can't be
    // read as a typed row
    for kind in MigrationRecordKind::ALL {
        let mut stream = scylla
            .query_iter(
                format!("SELECT {columns} FROM {ns}.schema_migrations WHERE kind = ?"),
                (kind.as_str(),),
            )
            .await
            .unwrap()
            .rows_stream::<SchemaMigrationRow>()
            .unwrap();
        while let Some(row) = stream.next().await {
            rows.migrations.push(migrations_row(row.unwrap()));
        }
    }
    let columns = <SchemaMigrationIssueRow as Projection>::COLUMNS.join(", ");
    let mut stream = scylla
        .query_iter(
            format!("SELECT {columns} FROM {ns}.schema_migration_issues"),
            (),
        )
        .await
        .unwrap()
        .rows_stream::<SchemaMigrationIssueRow>()
        .unwrap();
    while let Some(row) = stream.next().await {
        rows.issues.push(issue_row(row.unwrap()));
    }
    rows.sort();
    rows
}

/// Loads the record a backup holds, which the test expects to be there
///
/// # Arguments
///
/// * `backup` - The backup root
///
/// # Panics
///
/// Panics when the backup holds no record or an unreadable one.
pub async fn load(backup: &std::path::Path) -> BackupSchemaRecord {
    // the record is the backup's `schema.json`
    BackupSchemaRecord::load(backup)
        .await
        .expect("the backup's schema.json reads")
        .expect("the backup holds a schema.json")
}

/// Empties the data target and initializes the system again, as a fresh install has it; the
/// migration-state tables are left to the caller
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
pub async fn empty_target(harness: &Harness) {
    // the state tables and views exist, as an earlier test may have dropped them
    ensure_tables(harness).await;
    ensure_views(harness).await;
    // wipe the data, then put back what a fresh install has
    super::cycle::wipe(harness).await;
    harness
        .client()
        .await
        .system
        .init()
        .await
        .expect("failed to initialize the system");
}

/// Backs up a deployment whose state tables hold some rows, with one sample in the data, and
/// leaves the namespace holding the sample and the same rows
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `name` - The scratch directory's name
/// * `rows` - The rows the state tables hold when the backup is taken
///
/// # Returns
///
/// The backup root and the sample's sha256
pub async fn backup_of(harness: &Harness, name: &str, rows: &Rows) -> (std::path::PathBuf, String) {
    // one sample in a group of its own, in a target no other test left data in
    empty_target(harness).await;
    let client = harness.client().await;
    let names = super::names::Names::new();
    let group = names.group("minerva");
    super::seed::group(&client, &group).await;
    let sha = super::seed::sample(&client, &group, "owl.bin", names.suffix.as_bytes()).await;
    // the rows the deployment holds when it is backed up
    set(harness, rows).await;
    let out = harness.scratch(name).join("backup");
    let taken = harness
        .thoradm()
        .args(["--workers", "2", "backup", "new", "no-s3", "-m", "2", "-o"])
        .arg(&out)
        .output()
        .expect("failed to run thoradm");
    assert_eq!(
        taken.status.code(),
        Some(0),
        "{}",
        super::cycle::describe(&taken)
    );
    (out, sha)
}

/// Restores a backup without a terminal into the namespace, with its report in a named directory
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `backup` - The backup
/// * `report_dir` - The report directory
#[must_use]
pub fn restore(
    harness: &Harness,
    backup: &std::path::Path,
    report_dir: &std::path::Path,
) -> std::process::Output {
    // confirm with --confirm
    harness
        .thoradm()
        .args(["--workers", "2", "backup", "restore", "--confirm", "-b"])
        .arg(backup)
        .arg("--report-dir")
        .arg(report_dir)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("failed to run thoradm")
}

/// Creates the migration-state tables when the namespace lacks them, with `schema apply`
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
pub async fn ensure_tables(harness: &Harness) {
    // nothing to do while the record table exists, whose rows a test may have set
    let scylla = harness.scylla().await;
    let found = scylla
        .query_unpaged(
            "SELECT table_name FROM system_schema.tables WHERE keyspace_name = ? AND table_name = ?",
            (harness.conf.thorium.namespace.to_lowercase(), "schema_migrations"),
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap()
        .rows_num();
    if found > 0 {
        return;
    }
    // the applier creates what is missing
    thorium::models::schema::apply(
        &scylla,
        &harness.conf,
        &thorium::models::schema::ApplyOptions {
            target: thorium::models::schema::ApplyTarget::Declared,
            dry_run: false,
        },
        "",
        &|_| (),
    )
    .await
    .expect("failed to create the state tables");
}

/// Drops both migration-state tables of the harness's namespace, as a keyspace that never
/// recorded its schema lacks them
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
pub async fn drop_tables(harness: &Harness) {
    // drop each table
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    for table in ["schema_migrations", "schema_migration_issues"] {
        scylla
            .query_unpaged(format!("DROP TABLE IF EXISTS {ns}.{table}"), ())
            .await
            .unwrap();
    }
}

/// Creates every declared view the namespace lacks, as a test that dropped one leaves it when it
/// fails
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
pub async fn ensure_views(harness: &Harness) {
    // the views the keyspace holds now
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    let held: std::collections::BTreeSet<String> = scylla
        .query_unpaged(
            "SELECT view_name FROM system_schema.views WHERE keyspace_name = ?",
            (ns.to_lowercase(),),
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap()
        .rows::<(String,)>()
        .unwrap()
        .map(|row| row.unwrap().0)
        .collect();
    // create each declared view that is missing, from the statement its declaration renders
    for object in thorium::models::schema::SchemaObject::declared() {
        if let thorium::models::schema::SchemaDefinition::Scylla(
            thorium::models::schema::ObjectDef::View(view),
        ) = object.definition
            && !held.contains(view.name)
        {
            scylla.query_unpaged(view.create_cql(ns), ()).await.unwrap();
        }
    }
}

/// Writes one `schema_migrations` row of any kind, known to this build or not, with raw CQL
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `kind` - The row's kind, as the table stores it
/// * `id` - The row's id
/// * `value` - The row's `value` cell
pub async fn insert_raw(harness: &Harness, kind: &str, id: &str, value: &str) {
    // a fixed write time, so a copy of the row compares equal
    let scylla = harness.scylla().await;
    scylla
        .query_unpaged(
            format!(
                "INSERT INTO {}.schema_migrations (kind, id, value, updated) VALUES (?, ?, ?, \
                 '2026-10-01 00:01:00+0000')",
                harness.conf.thorium.namespace
            ),
            (kind, id, value),
        )
        .await
        .unwrap();
}

/// Reads the `value` cells of the `schema_migrations` rows of one kind, by id, with raw CQL
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `kind` - The kind, as the table stores it
pub async fn raw_values(
    harness: &Harness,
    kind: &str,
) -> std::collections::BTreeMap<String, Option<String>> {
    // one partition, read as plain text columns
    let scylla = harness.scylla().await;
    scylla
        .query_unpaged(
            format!(
                "SELECT id, value FROM {}.schema_migrations WHERE kind = ?",
                harness.conf.thorium.namespace
            ),
            (kind,),
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap()
        .rows::<(String, Option<String>)>()
        .unwrap()
        .map(|row| row.unwrap())
        .collect()
}

/// Backs up the namespace as it stands, tables and Redis without objects
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `name` - The scratch directory's name
pub fn backup_now(harness: &Harness, name: &str) -> std::path::PathBuf {
    // the same backup the other helpers take
    let out = harness.scratch(name).join("backup");
    let taken = harness
        .thoradm()
        .args(["--workers", "2", "backup", "new", "no-s3", "-m", "2", "-o"])
        .arg(&out)
        .output()
        .expect("failed to run thoradm");
    assert_eq!(
        taken.status.code(),
        Some(0),
        "{}",
        super::cycle::describe(&taken)
    );
    out
}
