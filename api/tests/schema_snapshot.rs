//! Tests that `schema apply` converges Scylla to a committed revision snapshot, rendering the
//! statements from the snapshot's canonical forms and recording the snapshot's fingerprints

use scylla::client::session::Session;
use scylla::statement::Statement;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;
use thorium::models::schema::{
    ApplyError, ApplyOptions, ApplyReport, ApplyTarget, Fingerprint, RevisionSnapshot, SchemaRows,
    SnapshotObject, SnapshotRun, apply, apply_snapshot,
};
use thorium::models::upgrades::{RevisionId, SchemaBackend};
use thorium::test_utilities::{self, CONF};
use thorium::{Conf, Error};

/// The committed revision these tests converge to
const REVISION: &str = "2026-10-v02";

/// The objects released revisions created that no release reads, which a restore leaves uncreated
const UNUSED: [&str; 2] = ["events", "entities_by_name"];

/// Run a CQL statement, failing with its text on error
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `cql` - The statement to run
async fn exec(session: &Session, cql: &str) -> Result<(), Error> {
    // run the statement, naming it in any error
    session
        .query_unpaged(cql, &())
        .await
        .map_err(|err| Error::new(format!("{cql}: {err}")))?;
    Ok(())
}

/// Read every row of a query of text columns
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `cql` - The query, selecting only text columns
/// * `binds` - The values to bind
async fn texts<const N: usize>(
    session: &Session,
    cql: &str,
    binds: &[&str],
) -> Result<Vec<[Option<String>; N]>, Error> {
    // run the query with a generous timeout
    let mut statement = Statement::new(cql);
    statement.set_request_timeout(Some(Duration::from_secs(300)));
    let rows = session
        .query_unpaged(statement, binds)
        .await
        .map_err(|err| Error::new(format!("{cql}: {err}")))?
        .into_rows_result()
        .map_err(|err| Error::new(format!("{cql}: {err}")))?;
    // read each row's columns as optional text
    let mut out = Vec::new();
    for row in rows
        .rows::<scylla::value::Row>()
        .map_err(|err| Error::new(err.to_string()))?
    {
        let row = row.map_err(|err| Error::new(err.to_string()))?;
        let mut values: [Option<String>; N] = std::array::from_fn(|_| None);
        for (slot, value) in values.iter_mut().zip(row.columns) {
            *slot = value.map(|value| match value {
                scylla::value::CqlValue::Text(text) => text,
                other => format!("{other:?}"),
            });
        }
        out.push(values);
    }
    Ok(out)
}

/// Build a config for a keyspace beside the run's own namespace, dropping any keyspace an earlier
/// run left there
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `suffix` - What makes this test's keyspace name unique
async fn fresh(session: &Session, suffix: &str) -> Result<Conf, Error> {
    // replace any keyspace an earlier run left behind
    let keyspace = format!("{}_{suffix}", CONF.thorium.namespace);
    exec(session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await?;
    // point a copy of the test config at it
    let mut conf = CONF.clone();
    conf.thorium.namespace = keyspace;
    Ok(conf)
}

/// The names of the tables and views in a keyspace
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to list
async fn objects(session: &Session, keyspace: &str) -> Result<BTreeSet<String>, Error> {
    // read the tables and the views from the schema tables
    let mut names = BTreeSet::new();
    for cql in [
        "SELECT table_name FROM system_schema.tables WHERE keyspace_name = ?",
        "SELECT view_name FROM system_schema.views WHERE keyspace_name = ?",
    ] {
        for [name] in texts::<1>(session, cql, &[keyspace]).await? {
            names.extend(name);
        }
    }
    Ok(names)
}

/// The names of a table's columns
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace the table is in
/// * `table` - The table
async fn columns(
    session: &Session,
    keyspace: &str,
    table: &str,
) -> Result<BTreeSet<String>, Error> {
    // read the column names from the schema tables
    let rows = texts::<1>(
        session,
        "SELECT column_name FROM system_schema.columns WHERE keyspace_name = ? AND table_name = ?",
        &[keyspace, table],
    )
    .await?;
    Ok(rows.into_iter().filter_map(|[name]| name).collect())
}

/// The recorded backend rows, by id: each one's fingerprint
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace holding the record
async fn backend_rows(
    session: &Session,
    keyspace: &str,
) -> Result<BTreeMap<String, Option<String>>, Error> {
    // read every backend row's fingerprint
    let rows = texts::<2>(
        session,
        &format!("SELECT id, fingerprint FROM {keyspace}.schema_migrations WHERE kind = 'backend'"),
        &[],
    )
    .await?;
    Ok(rows
        .into_iter()
        .map(|[id, fingerprint]| (id.unwrap_or_default(), fingerprint))
        .collect())
}

/// The committed snapshot the tests converge to
fn committed() -> RevisionSnapshot {
    // a committed snapshot is checked against its own canonical forms when loaded
    RevisionSnapshot::committed(REVISION)
        .expect("the revision has a snapshot")
        .expect("the snapshot is well formed")
}

/// The Scylla object names a snapshot lists
///
/// # Arguments
///
/// * `snapshot` - The snapshot
fn scylla_names(snapshot: &RevisionSnapshot) -> BTreeSet<String> {
    // every object of the Scylla backend
    snapshot
        .objects
        .iter()
        .filter(|object| object.backend == SchemaBackend::Scylla)
        .map(|object| object.name.clone())
        .collect()
}

/// The names of the migration-state tables, which every apply converges from this build
fn state_tables() -> BTreeSet<String> {
    ["schema_migrations", "schema_migration_issues"]
        .into_iter()
        .map(ToOwned::to_owned)
        .collect()
}

/// Apply a snapshot, turning a refusal into a test error
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `conf` - The config naming the keyspace
/// * `snapshot` - The snapshot to apply
/// * `run` - How to run
async fn converge(
    session: &Session,
    conf: &Conf,
    snapshot: &RevisionSnapshot,
    run: &SnapshotRun<'_>,
) -> Result<ApplyReport, Error> {
    // converge and record
    apply_snapshot(session, conf, snapshot, run, &|_| ())
        .await
        .map_err(|err| Error::new(format!("snapshot apply failed: {err}")))
}

/// Applying a committed revision to an empty keyspace creates every object its snapshot lists, in
/// the form the snapshot records, beside the migration-state tables
#[tokio::test]
async fn a_revision_applies_to_an_empty_keyspace() -> Result<(), Error> {
    // apply the revision through the target the command line names
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "snap_empty").await?;
    let keyspace = conf.thorium.namespace.clone();
    let options = ApplyOptions {
        target: ApplyTarget::Snapshot(REVISION.parse::<RevisionId>().unwrap()),
        dry_run: false,
    };
    let report = apply(&session, &conf, &options, "key-snap", &|_| ())
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    // the keyspace holds the snapshot's objects and the state tables, nothing else
    let snapshot = committed();
    let expected = scylla_names(&snapshot)
        .union(&state_tables())
        .cloned()
        .collect::<BTreeSet<_>>();
    assert_eq!(objects(&session, &keyspace).await?, expected);
    assert!(
        report.created.len() >= expected.len(),
        "{:?}",
        report.created
    );
    // the live objects have the canonical forms the snapshot records
    let live = SchemaRows::read(&session, &keyspace)
        .await
        .map_err(|err| Error::new(err.to_string()))?
        .objects()
        .map_err(|err| Error::new(err.to_string()))?;
    for object in snapshot
        .objects
        .iter()
        .filter(|object| object.backend == SchemaBackend::Scylla)
    {
        let found: Option<&SnapshotObject> = live.iter().find(|live| live.name == object.name);
        assert_eq!(
            found.map(|live| &live.canonical),
            Some(&object.canonical),
            "{}",
            object.name
        );
    }
    // the report and the record carry the snapshot's fingerprints
    assert_eq!(report.fingerprint, snapshot.deployment);
    let rows = backend_rows(&session, &keyspace).await?;
    assert_eq!(
        rows.get("deployment"),
        Some(&Some(snapshot.deployment.as_str().to_owned()))
    );
    for (backend, id) in [
        (SchemaBackend::Scylla, "scylla"),
        (SchemaBackend::Redis, "redis"),
        (SchemaBackend::S3, "s3"),
    ] {
        let expected = Some(snapshot.backends[&backend].as_str().to_owned());
        assert_eq!(rows.get(id), Some(&expected), "{id}");
    }
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// Objects a run skips aren't created, and the others still are
#[tokio::test]
async fn skipped_objects_are_not_created() -> Result<(), Error> {
    // apply the revision, skipping the objects no release reads
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "snap_skip").await?;
    let keyspace = conf.thorium.namespace.clone();
    let snapshot = committed();
    let run = SnapshotRun {
        skip: UNUSED.iter().map(|name| (*name).to_owned()).collect(),
        ..SnapshotRun::default()
    };
    converge(&session, &conf, &snapshot, &run).await?;
    // neither exists, and every other object does
    let live = objects(&session, &keyspace).await?;
    for name in UNUSED {
        assert!(!live.contains(name), "{name} was created: {live:?}");
    }
    let expected = scylla_names(&snapshot)
        .union(&state_tables())
        .filter(|name| !UNUSED.contains(&name.as_str()))
        .cloned()
        .collect::<BTreeSet<_>>();
    assert_eq!(live, expected);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// A second apply of the same snapshot issues no DDL
#[tokio::test]
async fn a_second_snapshot_apply_issues_no_ddl() -> Result<(), Error> {
    // apply twice
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "snap_twice").await?;
    let keyspace = conf.thorium.namespace.clone();
    let snapshot = committed();
    let run = SnapshotRun::default();
    converge(&session, &conf, &snapshot, &run).await?;
    let second = converge(&session, &conf, &snapshot, &run).await?;
    // nothing is created, added, or altered
    assert!(second.statements.is_empty(), "{:?}", second.statements);
    assert!(
        second.created.is_empty() && second.added.is_empty(),
        "{second:?}"
    );
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// A snapshot that holds a column and a table the live keyspace lacks adds them, and drops
/// neither a table nor a column the snapshot doesn't list
#[tokio::test]
async fn an_older_snapshot_re_adds_what_a_later_revision_dropped() -> Result<(), Error> {
    // a live keyspace at the committed revision, with a column the snapshot doesn't know
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "snap_readd").await?;
    let keyspace = conf.thorium.namespace.clone();
    let snapshot = committed();
    converge(&session, &conf, &snapshot, &SnapshotRun::default()).await?;
    exec(
        &session,
        &format!("ALTER TABLE {keyspace}.samples_list ADD newer_column text"),
    )
    .await?;
    exec(
        &session,
        &format!("CREATE TABLE {keyspace}.newer_table (k text PRIMARY KEY)"),
    )
    .await?;
    // the snapshot of an older revision holds one more column on samples_list, and a table
    let mut older = snapshot.clone();
    for object in &mut older.objects {
        if object.name == "samples_list" {
            let columns = object.canonical["columns"].as_array_mut().unwrap();
            columns.push(json!({"name": "dropped_later", "type": "text"}));
        }
    }
    older.objects.push(SnapshotObject {
        backend: SchemaBackend::Scylla,
        name: "dropped_table".to_owned(),
        fingerprint: Fingerprint::of(&json!({})),
        canonical: json!({
            "kind": "table", "name": "dropped_table",
            "columns": [{"name": "k", "type": "text"}, {"name": "v", "type": "map<text, uuid>"}],
            "partition_key": ["k"], "clustering_key": [], "static": [],
        }),
    });
    let report = converge(&session, &conf, &older, &SnapshotRun::default()).await?;
    // the column and the table come back, and nothing newer goes
    assert_eq!(report.added, vec!["samples_list.dropped_later".to_owned()]);
    assert!(
        report.created.contains(&"dropped_table".to_owned()),
        "{:?}",
        report.created
    );
    let samples = columns(&session, &keyspace, "samples_list").await?;
    assert!(samples.contains("dropped_later") && samples.contains("newer_column"));
    let live = objects(&session, &keyspace).await?;
    assert!(
        live.contains("dropped_table") && live.contains("newer_table"),
        "{live:?}"
    );
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// The apply returns only once every view it created is built
#[tokio::test]
async fn a_snapshot_apply_waits_for_its_views() -> Result<(), Error> {
    // apply to an empty keyspace
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "snap_views").await?;
    let keyspace = conf.thorium.namespace.clone();
    let snapshot = committed();
    let report = converge(&session, &conf, &snapshot, &SnapshotRun::default()).await?;
    // every view the snapshot lists was reported built
    let views = snapshot
        .objects
        .iter()
        .filter(|object| object.backend == SchemaBackend::Scylla)
        .filter(|object| object.canonical["kind"] == "view")
        .map(|object| object.name.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        report.views_built.iter().cloned().collect::<BTreeSet<_>>(),
        views
    );
    // and the node lists each as built
    for view in &views {
        let built = texts::<1>(
            &session,
            "SELECT view_name FROM system.built_views WHERE keyspace_name = ? AND view_name = ?",
            &[&keyspace, view],
        )
        .await?;
        assert_eq!(built.len(), 1, "{view} isn't built");
    }
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// A dry run lists the statements and creates nothing, not even the keyspace
#[tokio::test]
async fn a_snapshot_dry_run_changes_nothing() -> Result<(), Error> {
    // plan without executing
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "snap_dry").await?;
    let keyspace = conf.thorium.namespace.clone();
    let run = SnapshotRun {
        dry_run: true,
        ..SnapshotRun::default()
    };
    let report = converge(&session, &conf, &committed(), &run).await?;
    // the statements are listed, and the keyspace is still missing
    assert!(report.dry_run);
    assert!(
        report.statements[0].starts_with("CREATE KEYSPACE"),
        "{:?}",
        report.statements
    );
    assert!(
        report.statements.len() > 30,
        "{:?}",
        report.statements.len()
    );
    let found = texts::<1>(
        &session,
        "SELECT keyspace_name FROM system_schema.keyspaces WHERE keyspace_name = ?",
        &[&keyspace],
    )
    .await?;
    assert!(found.is_empty(), "the dry run created the keyspace");
    Ok(())
}

/// A revision with no committed snapshot is unavailable, naming the revision
#[tokio::test]
async fn a_revision_without_a_snapshot_is_unavailable() -> Result<(), Error> {
    // no snapshot is committed for this revision
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "snap_unknown").await?;
    let options = ApplyOptions {
        target: ApplyTarget::Snapshot("2026-10-v99".parse::<RevisionId>().unwrap()),
        dry_run: false,
    };
    let err = apply(&session, &conf, &options, "", &|_| ())
        .await
        .unwrap_err();
    // the error says which revision, and the keyspace is untouched
    assert!(
        matches!(&err, ApplyError::Unavailable { what } if what.contains("2026-10-v99") && what.contains("no committed snapshot")),
        "{err:?}"
    );
    Ok(())
}

/// The first snapshot apply records the data-shaping settings of the config, as `apply` does
#[tokio::test]
async fn a_snapshot_apply_records_the_data_shaping_settings() -> Result<(), Error> {
    // apply the revision
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "snap_config").await?;
    let keyspace = conf.thorium.namespace.clone();
    converge(&session, &conf, &committed(), &SnapshotRun::default()).await?;
    // the files partition size is recorded as the config holds it
    let rows = texts::<1>(
        &session,
        &format!(
            "SELECT value FROM {keyspace}.schema_migrations \
             WHERE kind = 'config' AND id = 'partition_size.files'"
        ),
        &[],
    )
    .await?;
    let recorded = rows[0][0].clone().unwrap_or_default();
    assert_eq!(recorded, conf.thorium.files.partition_size.to_string());
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// The default TTL of a table
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace the table is in
/// * `table` - The table
async fn live_ttl(session: &Session, keyspace: &str, table: &str) -> Result<i32, Error> {
    // read the table's default TTL from the schema tables
    let rows = session
        .query_unpaged(
            "SELECT default_time_to_live FROM system_schema.tables \
             WHERE keyspace_name = ? AND table_name = ?",
            (keyspace, table),
        )
        .await
        .map_err(|err| Error::new(err.to_string()))?
        .into_rows_result()
        .map_err(|err| Error::new(err.to_string()))?;
    let (ttl,) = rows
        .single_row::<(Option<i32>,)>()
        .map_err(|err| Error::new(err.to_string()))?;
    Ok(ttl.unwrap_or(0))
}

/// A table whose default TTL comes from a setting is created with the configured TTL, and a
/// later apply to a table whose live TTL differs alters it
#[tokio::test]
async fn snapshot_tables_take_their_configured_default_ttl() -> Result<(), Error> {
    // a config whose log retention differs from the default
    let session = test_utilities::scylla_session().await?;
    let mut conf = fresh(&session, "snap_ttl").await?;
    conf.thorium.retention.logs = 4242;
    let keyspace = conf.thorium.namespace.clone();
    let snapshot = committed();
    converge(&session, &conf, &snapshot, &SnapshotRun::default()).await?;
    // the logs table was created with it
    assert_eq!(live_ttl(&session, &keyspace, "logs").await?, 4242);
    // a changed setting is altered by the next apply, and reported as a config change
    conf.thorium.retention.logs = 5151;
    let report = converge(&session, &conf, &snapshot, &SnapshotRun::default()).await?;
    assert_eq!(live_ttl(&session, &keyspace, "logs").await?, 5151);
    assert_eq!(
        report.config_changes.len(),
        1,
        "{:?}",
        report.config_changes
    );
    assert_eq!(report.config_changes[0].table, "logs");
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}
