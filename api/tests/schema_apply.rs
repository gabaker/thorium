//! Tests that `schema apply` creates and converges Thorium's Scylla schema, waits for the views
//! it creates to build, and records what it applied in `schema_migrations`

use futures::{StreamExt, stream};
use scylla::client::session::Session;
use scylla::statement::Statement;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thorium::models::backends::setup::Scylla;
use thorium::models::schema::queries::ScyllaQueries;
use thorium::models::schema::queries::migrations::SchemaStateQueries;
use thorium::models::schema::scylla::ThoriumKeyspace;
use thorium::models::schema::statements::{prepare_all, registry};
use thorium::models::schema::{
    ApplyError, ApplyOptions, ApplyReport, ApplyTarget, ColumnDef, ColumnRole, ConfigChange,
    CqlDesc, CqlNative, Keyspace, ObjectDef, RevisionSnapshot, SchemaDefinition, SchemaObject,
    apply, apply_objects, fingerprints,
};
use thorium::models::upgrades::{LAYOUTS, SchemaBackend, latest};
use thorium::test_utilities::{self, CONF};
use thorium::utils::StartupOpts;
use thorium::utils::helpers::partition;
use thorium::{Conf, Error};
use uuid::Uuid;

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
    // run the query with a generous timeout, since some count over many rows
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

/// The options that converge to the declared schema and execute
fn declared() -> ApplyOptions {
    ApplyOptions {
        target: ApplyTarget::Declared,
        dry_run: false,
    }
}

/// Apply the declared schema, ignoring progress
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `conf` - The config naming the keyspace
/// * `key` - The trigger key to record
async fn apply_declared(session: &Session, conf: &Conf, key: &str) -> Result<ApplyReport, Error> {
    // converge and record, turning a refusal into a test error
    apply(session, conf, &declared(), key, &|_| ())
        .await
        .map_err(|err| Error::new(format!("schema apply failed: {err:?}")))
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

/// The recorded backend rows, by id: each one's fingerprint and apply key
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace holding the record
async fn backend_rows(
    session: &Session,
    keyspace: &str,
) -> Result<BTreeMap<String, (Option<String>, Option<String>)>, Error> {
    // read every backend row's fingerprint and key
    let rows = texts::<3>(
        session,
        &format!(
            "SELECT id, fingerprint, apply_key FROM {keyspace}.schema_migrations \
             WHERE kind = 'backend'"
        ),
        &[],
    )
    .await?;
    Ok(rows
        .into_iter()
        .map(|[id, fingerprint, key]| (id.unwrap_or_default(), (fingerprint, key)))
        .collect())
}

/// When each recorded backend row's fingerprint was written, by id, in microseconds since the
/// epoch
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace holding the record
async fn write_times(session: &Session, keyspace: &str) -> Result<BTreeMap<String, i64>, Error> {
    // read every backend row's id and the write time of its fingerprint
    let cql = format!(
        "SELECT id, WRITETIME(fingerprint) FROM {keyspace}.schema_migrations \
         WHERE kind = 'backend'"
    );
    let rows = session
        .query_unpaged(cql, ())
        .await
        .map_err(|err| Error::new(err.to_string()))?
        .into_rows_result()
        .map_err(|err| Error::new(err.to_string()))?;
    let mut times = BTreeMap::new();
    for row in rows
        .rows::<(String, Option<i64>)>()
        .map_err(|err| Error::new(err.to_string()))?
    {
        let (id, time) = row.map_err(|err| Error::new(err.to_string()))?;
        times.insert(id, time.unwrap_or_default());
    }
    Ok(times)
}

/// The layout versions a backend row records
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace holding the record
/// * `backend` - The backend row to read
async fn recorded_layouts(
    session: &Session,
    keyspace: &str,
    backend: &str,
) -> Result<BTreeMap<String, i64>, Error> {
    // read the row's layouts map
    let cql = format!(
        "SELECT layouts FROM {keyspace}.schema_migrations WHERE kind = 'backend' AND id = ?"
    );
    let rows = session
        .query_unpaged(cql, (backend,))
        .await
        .map_err(|err| Error::new(err.to_string()))?
        .into_rows_result()
        .map_err(|err| Error::new(err.to_string()))?;
    let (layouts,) = rows
        .single_row::<(Option<BTreeMap<String, i64>>,)>()
        .map_err(|err| Error::new(err.to_string()))?;
    Ok(layouts.unwrap_or_default())
}

/// The names of the declared Scylla objects, in creation order
fn declared_names() -> Vec<&'static str> {
    <ThoriumKeyspace as Keyspace>::OBJECTS
        .iter()
        .map(ObjectDef::name)
        .collect()
}

/// On an empty cluster, `schema apply` creates the keyspace, then `schema_migrations`, then every
/// declared object, in that order
#[tokio::test]
async fn apply_creates_the_keyspace_then_the_record_then_every_object() -> Result<(), Error> {
    // apply to a keyspace that doesn't exist
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "apply_order").await?;
    let keyspace = conf.thorium.namespace.clone();
    let report = apply_declared(&session, &conf, "key-order").await?;
    // the keyspace comes first, then schema_migrations, then every other object in order
    assert!(
        report.statements[0].starts_with(&format!("CREATE KEYSPACE IF NOT EXISTS {keyspace} ")),
        "{:?}",
        report.statements.first()
    );
    assert!(
        report.statements[1].starts_with(&format!(
            "CREATE TABLE IF NOT EXISTS {keyspace}.schema_migrations ("
        )),
        "{:?}",
        report.statements.get(1)
    );
    assert_eq!(report.created, declared_names());
    assert_eq!(report.statements.len(), 1 + declared_names().len());
    for (statement, name) in report.statements[1..].iter().zip(declared_names()) {
        assert!(
            statement.contains(&format!(" IF NOT EXISTS {keyspace}.{name} ")),
            "{statement} doesn't create {name}"
        );
    }
    assert!(report.added.is_empty(), "{:?}", report.added);
    assert!(!report.dry_run);
    // the live keyspace holds exactly the declared objects
    let live = objects(&session, &keyspace).await?;
    let expected = declared_names()
        .into_iter()
        .map(ToOwned::to_owned)
        .collect::<BTreeSet<_>>();
    assert_eq!(live, expected);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// After `schema apply` on an empty keyspace, `schema_migrations` holds the deployment
/// fingerprint row with the trigger key, each backend's fingerprint, and the Redis and S3 layout
/// versions from the registry
#[tokio::test]
async fn apply_records_the_fingerprints_and_layouts() -> Result<(), Error> {
    // apply the declared schema to a new keyspace
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "apply_record").await?;
    let keyspace = conf.thorium.namespace.clone();
    let report = apply_declared(&session, &conf, "key-record").await?;
    // the report and the record carry the declared snapshot's fingerprints
    let snapshot = RevisionSnapshot::declared(latest());
    assert_eq!(report.fingerprint, snapshot.deployment);
    assert_eq!(report.trigger_key, "key-record");
    let rows = backend_rows(&session, &keyspace).await?;
    assert_eq!(
        rows.get("deployment"),
        Some(&(
            Some(snapshot.deployment.as_str().to_owned()),
            Some("key-record".to_owned())
        ))
    );
    for (backend, id) in [
        (SchemaBackend::Scylla, "scylla"),
        (SchemaBackend::Elastic, "elastic"),
        (SchemaBackend::Redis, "redis"),
        (SchemaBackend::S3, "s3"),
    ] {
        let expected = snapshot.backends[&backend].as_str().to_owned();
        assert_eq!(
            rows.get(id).map(|row| row.0.clone()),
            Some(Some(expected)),
            "{id}"
        );
    }
    assert_eq!(rows.len(), 5, "{rows:?}");
    // only the deployment's row carries the key
    for (id, (_, key)) in &rows {
        if id != "deployment" {
            assert_eq!(key, &None, "the {id} row carries the key");
        }
    }
    // the deployment's row is written after every backend row, so it only names a full record
    let times = write_times(&session, &keyspace).await?;
    let deployment = times["deployment"];
    for (id, time) in &times {
        assert!(
            id == "deployment" || *time < deployment,
            "the {id} row was written at {time}, after the deployment row at {deployment}"
        );
    }
    // the Redis and S3 rows hold every layout version the registry declares
    for (backend, id) in [(SchemaBackend::Redis, "redis"), (SchemaBackend::S3, "s3")] {
        let expected = LAYOUTS
            .iter()
            .filter(|layout| layout.backend == backend)
            .map(|layout| (layout.family.to_owned(), i64::from(layout.version)))
            .collect::<BTreeMap<_, _>>();
        assert!(!expected.is_empty());
        assert_eq!(
            recorded_layouts(&session, &keyspace, id).await?,
            expected,
            "{id}"
        );
    }
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// The trigger key is recorded and reported without its files password hash segment, while
/// every other segment is kept
#[tokio::test]
async fn the_recorded_apply_key_leaves_out_the_password() -> Result<(), Error> {
    // apply with a key that carries the password's hash
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "apply_password").await?;
    let keyspace = conf.thorium.namespace.clone();
    let key = "image=thorium:1;password=c0ffee5ecret;revision=2026-10-v02";
    let report = apply_declared(&session, &conf, key).await?;
    // the report and the deployment row keep only the other segments
    let redacted = "image=thorium:1;revision=2026-10-v02";
    assert_eq!(report.trigger_key, redacted);
    let rows = backend_rows(&session, &keyspace).await?;
    assert_eq!(
        rows.get("deployment").and_then(|row| row.1.clone()),
        Some(redacted.to_owned())
    );
    // no row anywhere in the record holds the hash
    let all = texts::<1>(
        &session,
        &format!("SELECT apply_key FROM {keyspace}.schema_migrations"),
        &[],
    )
    .await?;
    for [stored] in all {
        assert!(
            !stored.unwrap_or_default().contains("c0ffee5ecret"),
            "the record holds the password hash"
        );
    }
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// A second `schema apply` issues no DDL and leaves the recorded fingerprint unchanged
#[tokio::test]
async fn a_second_apply_issues_no_ddl() -> Result<(), Error> {
    // apply twice to the same keyspace
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "apply_again").await?;
    let keyspace = conf.thorium.namespace.clone();
    let first = apply_declared(&session, &conf, "key-again").await?;
    let before = backend_rows(&session, &keyspace).await?;
    let second = apply_declared(&session, &conf, "key-again").await?;
    // the second run issued nothing and recorded the same fingerprints
    assert!(second.statements.is_empty(), "{:?}", second.statements);
    assert!(second.created.is_empty(), "{:?}", second.created);
    assert!(second.added.is_empty(), "{:?}", second.added);
    assert_eq!(second.fingerprint, first.fingerprint);
    assert_eq!(backend_rows(&session, &keyspace).await?, before);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// After a column is added to a table in the registry, `schema apply` adds it with
/// `ALTER … ADD` and records the new fingerprint
#[tokio::test]
async fn a_column_added_to_the_registry_is_added_and_recorded() -> Result<(), Error> {
    // apply the declared schema first
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "apply_column").await?;
    let keyspace = conf.thorium.namespace.clone();
    apply_declared(&session, &conf, "key-column").await?;
    // the registry's nodes table gains a column
    let mut objects = SchemaObject::declared();
    let nodes = objects
        .iter_mut()
        .find(|object| object.name == "nodes")
        .expect("nodes is declared");
    let SchemaDefinition::Scylla(ObjectDef::Table(mut table)) = nodes.definition else {
        panic!("nodes is a table");
    };
    let mut cols = table.columns.to_vec();
    cols.push(ColumnDef {
        name: "rack",
        desc: CqlDesc::Native(CqlNative::Text),
        role: ColumnRole::Regular,
        security_sensitive: false,
    });
    table.columns = Box::leak(cols.into_boxed_slice());
    nodes.definition = SchemaDefinition::Scylla(ObjectDef::Table(table));
    // applying the changed registry adds the column and records its fingerprint
    let report = apply_objects(&session, &conf, &objects, false, "key-column", &|_| ())
        .await
        .map_err(|err| Error::new(format!("{err:?}")))?;
    assert_eq!(
        report.statements,
        [format!("ALTER TABLE {keyspace}.nodes ADD rack text")]
    );
    assert_eq!(report.added, ["nodes.rack"]);
    assert!(report.created.is_empty(), "{:?}", report.created);
    assert!(
        columns(&session, &keyspace, "nodes")
            .await?
            .contains("rack")
    );
    let (changed, _) = fingerprints(&objects);
    assert_ne!(changed, RevisionSnapshot::declared(latest()).deployment);
    assert_eq!(report.fingerprint, changed);
    let rows = backend_rows(&session, &keyspace).await?;
    assert_eq!(
        rows.get("deployment").and_then(|row| row.0.clone()),
        Some(changed.as_str().to_owned())
    );
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// With an extra table and an extra column created by hand, `schema apply` drops neither and
/// finishes successfully
#[tokio::test]
async fn extra_tables_and_columns_are_left_alone() -> Result<(), Error> {
    // apply, then add a table and a column the registry doesn't declare
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "apply_extra").await?;
    let keyspace = conf.thorium.namespace.clone();
    apply_declared(&session, &conf, "key-extra").await?;
    exec(
        &session,
        &format!("CREATE TABLE {keyspace}.hand_made (id int PRIMARY KEY, note text)"),
    )
    .await?;
    exec(
        &session,
        &format!("ALTER TABLE {keyspace}.nodes ADD hand_note text"),
    )
    .await?;
    // the next apply succeeds and issues nothing
    let report = apply_declared(&session, &conf, "key-extra").await?;
    assert!(report.statements.is_empty(), "{:?}", report.statements);
    // both extras are still there
    assert!(objects(&session, &keyspace).await?.contains("hand_made"));
    assert!(
        columns(&session, &keyspace, "nodes")
            .await?
            .contains("hand_note")
    );
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// Render today's legacy setup statements without the three columns most recently added to them
/// (`associations.submissions_source`, `associations.submissions_other`, `results.entities`), the
/// shape of a keyspace an older release created
///
/// # Arguments
///
/// * `keyspace` - The keyspace to create the objects in
fn legacy_ddl_without_new_columns(keyspace: &str) -> Vec<String> {
    // the legacy statements, without the three later columns
    let legacy = include_str!("../src/models/schema/goldens/legacy_ddl.cql")
        .replace(", submissions_source UUID, submissions_other UUID", "")
        .replace("{ns}", keyspace)
        .replace("{ttl}", "2592000");
    let mut statements = legacy
        .split(';')
        .map(str::trim)
        .filter(|statement| !statement.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    // the results table had no entities column yet
    for statement in &mut statements {
        if statement.contains(&format!("TABLE IF NOT EXISTS {keyspace}.results ")) {
            let start = statement
                .find("entities")
                .expect("results declares entities");
            let end = start + statement[start..].find(">,").expect("the map type ends") + 2;
            statement.replace_range(start..end, "");
        }
    }
    statements
}

/// A keyspace created by the legacy setup statements before their three newest columns, seeded
/// with rows, converges under `schema apply`: the three missing columns are added with
/// `ALTER … ADD`, the two tables whose legacy TTL differs from the config get the configured one,
/// every seeded row reads back unchanged, a second apply issues no DDL, and the API then starts
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn a_legacy_keyspace_without_the_new_columns_converges() -> Result<(), Error> {
    // create the keyspace with the legacy statements, minus the three newest columns
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "apply_legacy").await?;
    let keyspace = conf.thorium.namespace.clone();
    exec(
        &session,
        &format!(
            "CREATE KEYSPACE {keyspace} WITH replication = \
             {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}}"
        ),
    )
    .await?;
    for statement in legacy_ddl_without_new_columns(&keyspace) {
        exec(&session, &statement).await?;
    }
    assert!(
        !columns(&session, &keyspace, "results")
            .await?
            .contains("entities")
    );
    // seed rows in the tables that lack columns and in one that doesn't, the association in the
    // bucket its partition size gives, as the API writes it
    let created = chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
        .map_err(|err| Error::new(err.to_string()))?
        .with_timezone(&chrono::Utc);
    let bucket = partition(created, 2026, conf.thorium.associations.partition_size);
    let seeds = [
        format!(
            "INSERT INTO {keyspace}.associations (group, year, bucket, source, created, target, \
             direction, kind, submitter) VALUES ('minerva', 2026, {bucket}, 'corn', \
             '2026-01-02T03:04:05Z', 'husk', 'To', 'FileFor', 'io')"
        ),
        format!(
            "INSERT INTO {keyspace}.results (id, uploaded, tool, tool_version, cmd, result) \
             VALUES (5b6e8a52-6c3f-4d1e-9c1a-0b9d2e3f4a5b, '2026-01-02T03:04:05Z', 'harvest', \
             '1.0', 'reap', '{{}}')"
        ),
        format!(
            "INSERT INTO {keyspace}.nodes (cluster, node, health) VALUES ('field', 'n1', 'ok')"
        ),
    ];
    for seed in &seeds {
        exec(&session, seed).await?;
    }
    let reads = [
        format!(
            "SELECT kind, submitter, CAST(submissions_source AS text), \
             CAST(submissions_other AS text) FROM {keyspace}.associations"
        ),
        format!("SELECT tool, cmd, result, entities FROM {keyspace}.results"),
        format!("SELECT cluster, node, health, resources FROM {keyspace}.nodes"),
    ];
    // apply: the state tables are created, the three columns added, and the two TTLs set from the
    // config, nothing else
    let report = apply_declared(&session, &conf, "key-legacy").await?;
    assert_eq!(
        report.created,
        ["schema_migrations", "schema_migration_issues"]
    );
    assert_eq!(
        report.added,
        [
            "associations.submissions_source",
            "associations.submissions_other",
            "results.entities"
        ]
    );
    let alters = report
        .statements
        .iter()
        .filter(|statement| statement.starts_with("ALTER TABLE") && statement.contains(" ADD "))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        alters,
        [
            format!("ALTER TABLE {keyspace}.associations ADD submissions_source uuid"),
            format!("ALTER TABLE {keyspace}.associations ADD submissions_other uuid"),
            format!("ALTER TABLE {keyspace}.results ADD entities map<text, bigint>"),
        ]
    );
    // the legacy statements gave both TTL tables 30 days; the test config gives other values
    let ttl = |table: &str, to: u64| ConfigChange {
        table: table.to_owned(),
        setting: "default_time_to_live".to_owned(),
        from: "2592000".to_owned(),
        to: to.to_string(),
    };
    assert_eq!(
        report.config_changes,
        [
            ttl("logs", conf.thorium.retention.logs),
            ttl("notifications", conf.thorium.retention.notifications)
        ]
    );
    assert_eq!(report.statements.len(), 7, "{:?}", report.statements);
    // every seeded row reads back unchanged, with the new columns null
    let rows = [
        texts::<4>(&session, &reads[0], &[]).await?,
        texts::<4>(&session, &reads[1], &[]).await?,
        texts::<4>(&session, &reads[2], &[]).await?,
    ];
    let some = |text: &str| Some(text.to_owned());
    assert_eq!(rows[0], [[some("FileFor"), some("io"), None, None]]);
    assert_eq!(rows[1], [[some("harvest"), some("reap"), some("{}"), None]]);
    assert_eq!(rows[2], [[some("field"), some("n1"), some("ok"), None]]);
    // a second apply issues no DDL
    let again = apply_declared(&session, &conf, "key-legacy").await?;
    assert!(again.statements.is_empty(), "{:?}", again.statements);
    // and the API starts on the converged keyspace
    let started = tokio::spawn({
        let conf = conf.clone();
        async move {
            Scylla::start(&conf, StartupOpts::default())
                .await
                .expect("the API starts")
                .session
        }
    })
    .await;
    assert!(
        started.is_ok(),
        "the API didn't start on the converged keyspace"
    );
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// Check that the reports of a wait for two views, `results_auth` over an empty table and
/// `samples` over a full one, say how many are built and name the one still building
///
/// The view over the empty table builds first, so once it is built only `samples` remains.
///
/// # Arguments
///
/// * `lines` - Each report with when it was made
fn assert_view_reports(lines: &[(i64, String)]) {
    // the reports that count built views, which leave out the closing report
    let building = lines
        .iter()
        .filter(|(_, line)| line.contains(" views built after "))
        .collect::<Vec<_>>();
    assert!(
        !building.is_empty(),
        "no report split built from building: {lines:?}"
    );
    // each names `samples` and, once one view is built, names only it
    for (_, line) in &building {
        assert!(
            line.starts_with("0 of 2 views built after ")
                || line.starts_with("1 of 2 views built after "),
            "unexpected report: {line}"
        );
        assert!(line.contains("; building: "), "no building list: {line}");
        assert!(
            line.contains("samples"),
            "samples isn't listed as building: {line}"
        );
        if line.starts_with("1 of 2") {
            assert!(
                line.ends_with("; building: samples"),
                "wrong view listed: {line}"
            );
        }
    }
}

/// Seed `samples_list` with rows the `samples` view selects, spread over many partitions
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace holding the table
/// * `rows` - How many rows to write
async fn seed_samples(session: &Session, keyspace: &str, rows: usize) -> Result<(), Error> {
    // prepare the insert once
    let insert = session
        .prepare(format!(
            "INSERT INTO {keyspace}.samples_list (group, year, bucket, uploaded, id, sha256) \
             VALUES (?, 2026, ?, ?, ?, ?)"
        ))
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    // write the rows many at a time
    let results = stream::iter(0..rows)
        .map(|index| {
            let insert = &insert;
            async move {
                // spread the rows over many partitions
                let group = format!("g{}", index % 97);
                let bucket = i32::try_from(index % 53).unwrap_or_default();
                let uploaded = scylla::value::CqlTimestamp(
                    1_767_225_600_000 + i64::try_from(index).unwrap_or_default(),
                );
                let sha256 = format!("{index:064x}");
                session
                    .execute_unpaged(insert, (group, bucket, uploaded, Uuid::new_v4(), sha256))
                    .await
                    .map(|_| ())
            }
        })
        .buffer_unordered(256)
        .collect::<Vec<_>>()
        .await;
    // the first failed write fails the seeding
    if let Some(Err(err)) = results.into_iter().find(Result::is_err) {
        return Err(Error::new(format!("seeding failed: {err}")));
    }
    Ok(())
}

/// How many rows the view-build test seeds
const VIEW_ROWS: usize = 1_000_000;

/// On a table seeded with 1 million rows, `schema apply` that creates a new view reports success
/// only once `system.built_views` lists the view and a count over the view equals the base
/// table's, with progress in its log that says how many views are built and names those still
/// building
#[tokio::test]
#[ignore = "seeds one million rows and waits for a view build, so it runs in the nightly pipeline; run by hand: cargo test -p thorium-api --features test-utilities --test schema_apply -- --ignored apply_waits_for_a_new_view_to_build"]
async fn apply_waits_for_a_new_view_to_build() -> Result<(), Error> {
    // apply, then drop the samples view so the next apply creates it over a full base table, and
    // a view over an empty table that builds at once
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "apply_views").await?;
    let keyspace = conf.thorium.namespace.clone();
    apply_declared(&session, &conf, "key-views").await?;
    for view in ["samples", "results_auth"] {
        exec(
            &session,
            &format!("DROP MATERIALIZED VIEW {keyspace}.{view}"),
        )
        .await?;
    }
    // seed the base table with a million rows, every one of them selected by the view
    seed_samples(&session, &keyspace, VIEW_ROWS).await?;
    // the apply creates the view and reports it built, logging its progress
    let lines = Arc::new(Mutex::new(Vec::new()));
    let progress = {
        let lines = lines.clone();
        move |line: &str| {
            // note when each line was reported, in microseconds since the epoch like write times
            let at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| {
                    i64::try_from(since.as_micros()).unwrap_or(i64::MAX)
                });
            lines.lock().unwrap().push((at, line.to_owned()));
        }
    };
    let report = apply(&session, &conf, &declared(), "key-views", &progress)
        .await
        .map_err(|err| Error::new(format!("{err:?}")))?;
    assert_eq!(report.created, ["results_auth", "samples"]);
    assert_eq!(report.views_built, ["results_auth", "samples"]);
    // the moment it returns, the view is listed as built and holds every base row
    let built = texts::<1>(
        &session,
        "SELECT view_name FROM system.built_views WHERE keyspace_name = ? AND view_name = ?",
        &[&keyspace, "samples"],
    )
    .await?;
    assert_eq!(built.len(), 1, "samples isn't listed as built");
    let count = |table: &str| format!("SELECT CAST(count(*) AS text) FROM {keyspace}.{table}");
    let base = texts::<1>(&session, &count("samples_list"), &[]).await?;
    let view = texts::<1>(&session, &count("samples"), &[]).await?;
    assert_eq!(base, [[Some(VIEW_ROWS.to_string())]]);
    assert_eq!(view, base);
    // the progress named the view it waited for, shown in the test's output
    let lines = lines.lock().unwrap().clone();
    println!("progress: {lines:#?}");
    assert!(
        lines.iter().any(|(_, line)| line.contains("samples")),
        "no progress named the view: {lines:?}"
    );
    // every report says how many views are built and names the one still building
    assert_view_reports(&lines);
    // the deployment's row was written after the last report of the view still building
    let waiting = lines
        .iter()
        .filter(|(_, line)| line.contains(" views built after "))
        .map(|(at, _)| *at)
        .max()
        .expect("no progress reported the view still building");
    let recorded = write_times(&session, &keyspace).await?["deployment"];
    assert!(
        recorded > waiting,
        "the record was written at {recorded}, while the view was still building at {waiting}"
    );
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// A view dropped while `schema apply` waits for it to build ends the wait with an error naming
/// the view, instead of waiting for a build that can never finish
#[tokio::test]
#[ignore = "seeds one million rows so the build outlasts the drop, so it runs in the nightly pipeline; run by hand: cargo test -p thorium-api --features test-utilities --test schema_apply -- --ignored a_view_dropped_during_the_wait_ends_it"]
async fn a_view_dropped_during_the_wait_ends_it() -> Result<(), Error> {
    // apply, then drop the samples view so the next apply creates it over a table with rows to build
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "apply_vanish").await?;
    let keyspace = conf.thorium.namespace.clone();
    apply_declared(&session, &conf, "key-vanish").await?;
    exec(
        &session,
        &format!("DROP MATERIALIZED VIEW {keyspace}.samples"),
    )
    .await?;
    seed_samples(&session, &keyspace, VIEW_ROWS).await?;
    // a dropper that drops the view the moment it exists, which is while it still builds, and
    // gives up after a minute if the apply never creates it
    let dropper = tokio::time::timeout(Duration::from_secs(60), async {
        let lookup = format!(
            "SELECT view_name FROM system_schema.views \
             WHERE keyspace_name = '{keyspace}' AND view_name = 'samples'"
        );
        while texts::<1>(&session, &lookup, &[]).await?.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        exec(
            &session,
            &format!("DROP MATERIALIZED VIEW IF EXISTS {keyspace}.samples"),
        )
        .await
    });
    // the apply ends within a minute with an error naming the view
    let options = declared();
    let (ended, drop_outcome) = tokio::join!(
        tokio::time::timeout(
            Duration::from_secs(60),
            apply(&session, &conf, &options, "key-vanish", &|_| ()),
        ),
        dropper
    );
    drop_outcome.map_err(|_| Error::new("the apply never created the view"))??;
    match ended {
        Ok(Err(ApplyError::ViewVanished { views })) => assert_eq!(views, ["samples"]),
        other => panic!("the wait didn't end with the view named: {other:?}"),
    }
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// Every registered statement prepares against a keyspace `schema apply` created, and so do the
/// API's queries and the statements on the migration-state tables
#[tokio::test]
async fn every_registered_statement_prepares_on_an_applied_keyspace() -> Result<(), Error> {
    // build the schema with schema apply alone
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "apply_prepare").await?;
    let keyspace = conf.thorium.namespace.clone();
    apply_declared(&session, &conf, "key-prepare").await?;
    // the prepare check passes over the whole registry
    let prepared = prepare_all(&session, &keyspace)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    assert_eq!(prepared, registry(&keyspace).len());
    // the queries structs prepare too
    ScyllaQueries::prepare(&session, &keyspace)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    SchemaStateQueries::prepare(&session, &keyspace)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// A dry run returns the statements a real run would issue without executing or recording any
#[tokio::test]
async fn a_dry_run_changes_nothing() -> Result<(), Error> {
    // dry-run against a keyspace that doesn't exist
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "apply_dry").await?;
    let keyspace = conf.thorium.namespace.clone();
    let options = ApplyOptions {
        target: ApplyTarget::Declared,
        dry_run: true,
    };
    let report = apply(&session, &conf, &options, "key-dry", &|_| ())
        .await
        .map_err(|err| Error::new(format!("{err:?}")))?;
    // it lists every statement but created nothing
    assert!(report.dry_run);
    assert_eq!(report.statements.len(), 1 + declared_names().len());
    assert_eq!(
        report.statements,
        thorium::models::schema::render(&conf, &SchemaObject::declared())
    );
    let exists = texts::<1>(
        &session,
        "SELECT keyspace_name FROM system_schema.keyspaces WHERE keyspace_name = ?",
        &[&keyspace],
    )
    .await?;
    assert!(exists.is_empty(), "a dry run created the keyspace");
    Ok(())
}

/// A run without a trigger key records the deployment's key as null rather than an empty string
#[tokio::test]
async fn an_empty_trigger_key_is_recorded_as_null() -> Result<(), Error> {
    // apply without a key
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "apply_nokey").await?;
    let keyspace = conf.thorium.namespace.clone();
    apply_declared(&session, &conf, "").await?;
    // the deployment row holds a fingerprint and no key
    let rows = backend_rows(&session, &keyspace).await?;
    let deployment = rows
        .get("deployment")
        .expect("the deployment row is written");
    assert!(deployment.0.is_some(), "{deployment:?}");
    assert_eq!(deployment.1, None);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// A statement that fails after `schema_migrations` and some objects are created stops the run
/// with a query error and records nothing, so no record names a schema that wasn't applied
#[tokio::test]
async fn a_failed_statement_records_nothing() -> Result<(), Error> {
    // declare one more table after the others, whose name no CQL statement accepts
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "apply_fail").await?;
    let keyspace = conf.thorium.namespace.clone();
    let mut schema = SchemaObject::declared();
    let mut broken = schema
        .iter()
        .find(|object| object.name == "nodes")
        .expect("nodes is declared")
        .clone();
    let SchemaDefinition::Scylla(ObjectDef::Table(mut table)) = broken.definition else {
        panic!("nodes is a table");
    };
    table.name = "not-a-name";
    broken.name = table.name.to_owned();
    broken.definition = SchemaDefinition::Scylla(ObjectDef::Table(table));
    schema.push(broken);
    // the run stops at that table with the statement that failed
    let err = apply_objects(&session, &conf, &schema, false, "key-fail", &|_| ())
        .await
        .expect_err("a statement that can't run was applied");
    match &err {
        ApplyError::Query { statement, .. } => {
            assert!(statement.contains("not-a-name"), "{err}");
        }
        other => panic!("not a query error: {other:?}"),
    }
    // the record table and the objects before the failure exist, but no row was written
    let live = objects(&session, &keyspace).await?;
    assert!(live.contains("schema_migrations"), "{live:?}");
    assert!(live.contains("nodes"), "{live:?}");
    let rows = backend_rows(&session, &keyspace).await?;
    assert!(rows.is_empty(), "a failed run recorded {rows:?}");
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// A dry run on a converged keyspace returns no statement and leaves every recorded row as it
/// was, its key and write time included
#[tokio::test]
async fn a_dry_run_on_a_converged_keyspace_keeps_the_record() -> Result<(), Error> {
    // apply, then note the record
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "apply_dry_again").await?;
    let keyspace = conf.thorium.namespace.clone();
    apply_declared(&session, &conf, "key-before").await?;
    let rows = backend_rows(&session, &keyspace).await?;
    let times = write_times(&session, &keyspace).await?;
    // dry-run with another key
    let options = ApplyOptions {
        target: ApplyTarget::Declared,
        dry_run: true,
    };
    let report = apply(&session, &conf, &options, "key-after", &|_| ())
        .await
        .map_err(|err| Error::new(format!("{err:?}")))?;
    assert!(report.dry_run);
    assert!(report.statements.is_empty(), "{:?}", report.statements);
    // the record is untouched
    assert_eq!(backend_rows(&session, &keyspace).await?, rows);
    assert_eq!(write_times(&session, &keyspace).await?, times);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// Read a table's live `default_time_to_live` from `system_schema.tables`
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace holding the table
/// * `table` - The table to read
async fn live_ttl(session: &Session, keyspace: &str, table: &str) -> Result<i32, Error> {
    // read the one row the table has in the schema tables
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
        .first_row::<(i32,)>()
        .map_err(|err| Error::new(format!("{table}: {err}")))?;
    Ok(ttl)
}

/// On a Scylla with tablets enabled, `schema apply` creates the keyspace with the statement every
/// release has used (no `tablets` option), the keyspace uses tablets, every declared table and
/// all 12 declared views exist and serve reads after writes to their base tables, and a second
/// apply against the existing tablet keyspace issues no DDL
#[tokio::test]
async fn apply_converges_on_a_tablet_keyspace_with_every_view() -> Result<(), Error> {
    // apply to a keyspace that doesn't exist yet
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "apply_tbl").await?;
    let keyspace = conf.thorium.namespace.clone();
    let first = apply_declared(&session, &conf, "key-tablets").await?;
    // the keyspace statement names no tablets option
    assert!(
        first.statements[0].starts_with("CREATE KEYSPACE IF NOT EXISTS "),
        "{:?}",
        first.statements.first()
    );
    assert!(
        !first.statements[0].to_lowercase().contains("tablets"),
        "{}",
        first.statements[0]
    );
    // the cluster put the keyspace on tablets
    let tablets = session
        .query_unpaged(
            "SELECT initial_tablets FROM system_schema.scylla_keyspaces WHERE keyspace_name = ?",
            (&keyspace,),
        )
        .await
        .map_err(|err| Error::new(err.to_string()))?
        .into_rows_result()
        .map_err(|err| Error::new(err.to_string()))?;
    assert_eq!(tablets.rows_num(), 1, "{keyspace} isn't on tablets");
    // every declared table and all 12 declared views exist
    let live = objects(&session, &keyspace).await?;
    let views = texts::<1>(
        &session,
        "SELECT view_name FROM system_schema.views WHERE keyspace_name = ?",
        &[&keyspace],
    )
    .await?;
    assert_eq!(views.len(), 12, "{views:?}");
    for name in declared_names() {
        assert!(live.contains(name), "{name} wasn't created");
    }
    // a write to a base table is readable through its view
    let sha256 = format!("{:064x}", 7);
    exec(
        &session,
        &format!(
            "INSERT INTO {keyspace}.samples_list (group, year, bucket, uploaded, id, sha256) \
             VALUES ('g', 2026, 1, '2026-01-01T00:00:00Z', {}, '{sha256}')",
            Uuid::new_v4()
        ),
    )
    .await?;
    let read = texts::<1>(
        &session,
        &format!("SELECT group FROM {keyspace}.samples WHERE sha256 = ?"),
        &[&sha256],
    )
    .await?;
    assert_eq!(read, [[Some("g".to_owned())]]);
    // a second apply to the existing tablet keyspace issues no DDL and errors on nothing
    let second = apply_declared(&session, &conf, "key-tablets").await?;
    assert!(second.statements.is_empty(), "{:?}", second.statements);
    assert_eq!(second.fingerprint, first.fingerprint);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// A configured TTL that differs from a table's live `default_time_to_live` is altered by
/// `schema apply`, reported as a config change, and moves no fingerprint; a dry run reports it
/// without altering, once altered the next apply changes nothing, and a shorter retention is
/// applied the same way
#[tokio::test]
async fn a_changed_ttl_is_altered_and_reported_as_a_config_change() -> Result<(), Error> {
    // apply with one retention for logs
    let session = test_utilities::scylla_session().await?;
    let mut conf = fresh(&session, "apply_ttl").await?;
    let keyspace = conf.thorium.namespace.clone();
    conf.thorium.retention.logs = 86_400;
    let first = apply_declared(&session, &conf, "key-ttl").await?;
    assert!(
        first.config_changes.is_empty(),
        "{:?}",
        first.config_changes
    );
    assert_eq!(live_ttl(&session, &keyspace, "logs").await?, 86_400);
    let before = backend_rows(&session, &keyspace).await?;
    // a dry run with a different retention reports the change and alters nothing
    conf.thorium.retention.logs = 172_800;
    let dry = apply(
        &session,
        &conf,
        &ApplyOptions {
            target: ApplyTarget::Declared,
            dry_run: true,
        },
        "key-ttl",
        &|_| (),
    )
    .await
    .map_err(|err| Error::new(format!("{err:?}")))?;
    let change = ConfigChange {
        table: "logs".to_owned(),
        setting: "default_time_to_live".to_owned(),
        from: "86400".to_owned(),
        to: "172800".to_owned(),
    };
    assert_eq!(dry.config_changes, std::slice::from_ref(&change));
    assert_eq!(live_ttl(&session, &keyspace, "logs").await?, 86_400);
    // the real apply alters the table and reports the change
    let second = apply_declared(&session, &conf, "key-ttl").await?;
    assert_eq!(second.config_changes, [change]);
    assert_eq!(second.statements.len(), 1, "{:?}", second.statements);
    assert!(
        second.statements[0].starts_with(&format!("ALTER TABLE {keyspace}.logs WITH ")),
        "{}",
        second.statements[0]
    );
    assert!(second.created.is_empty() && second.added.is_empty());
    assert_eq!(live_ttl(&session, &keyspace, "logs").await?, 172_800);
    // no other table's TTL was touched
    assert_eq!(
        live_ttl(&session, &keyspace, "notifications").await?,
        i32::try_from(conf.thorium.retention.notifications).unwrap_or_default()
    );
    // the fingerprint and the recorded rows are unchanged
    assert_eq!(second.fingerprint, first.fingerprint);
    assert_eq!(backend_rows(&session, &keyspace).await?, before);
    // once altered, the next apply has nothing to change
    let third = apply_declared(&session, &conf, "key-ttl").await?;
    assert!(third.statements.is_empty(), "{:?}", third.statements);
    assert!(
        third.config_changes.is_empty(),
        "{:?}",
        third.config_changes
    );
    // a shorter retention is applied too, from the live value to the configured one
    conf.thorium.retention.logs = 3_600;
    let fourth = apply_declared(&session, &conf, "key-ttl").await?;
    assert_eq!(
        fourth.config_changes.len(),
        1,
        "{:?}",
        fourth.config_changes
    );
    assert_eq!(fourth.config_changes[0].from, "172800");
    assert_eq!(fourth.config_changes[0].to, "3600");
    assert_eq!(live_ttl(&session, &keyspace, "logs").await?, 3_600);
    assert_eq!(fourth.fingerprint, first.fingerprint);
    // a table with no default TTL at all is brought to the configured one
    exec(
        &session,
        &format!("ALTER TABLE {keyspace}.logs WITH default_time_to_live = 0"),
    )
    .await?;
    let fifth = apply_declared(&session, &conf, "key-ttl").await?;
    assert_eq!(fifth.config_changes.len(), 1, "{:?}", fifth.config_changes);
    assert_eq!(fifth.config_changes[0].from, "0");
    assert_eq!(fifth.config_changes[0].to, "3600");
    assert_eq!(live_ttl(&session, &keyspace, "logs").await?, 3_600);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// Every declared table whose default TTL comes from a setting has that TTL altered once when its
/// setting changes, each reported as one config change from the old value to the new one
#[tokio::test]
async fn every_ttl_source_is_altered_once_when_its_setting_changes() -> Result<(), Error> {
    // apply once with the test config's retentions
    let session = test_utilities::scylla_session().await?;
    let mut conf = fresh(&session, "apply_ttls").await?;
    let keyspace = conf.thorium.namespace.clone();
    apply_declared(&session, &conf, "key-ttls").await?;
    // raise the setting of every declared table that takes its TTL from one
    let mut expected = Vec::new();
    for object in <ThoriumKeyspace as Keyspace>::OBJECTS {
        let ObjectDef::Table(table) = object else {
            continue;
        };
        let Some(source) = table.ttl else {
            continue;
        };
        let setting = match source.config_path {
            "thorium.retention.logs" => &mut conf.thorium.retention.logs,
            "thorium.retention.notifications" => &mut conf.thorium.retention.notifications,
            "thorium.events.retention" => &mut conf.thorium.events.retention,
            other => panic!("{other} has no case in this test"),
        };
        let from = *setting;
        *setting += 3_600;
        expected.push(ConfigChange {
            table: table.name.to_owned(),
            setting: "default_time_to_live".to_owned(),
            from: from.to_string(),
            to: (from + 3_600).to_string(),
        });
    }
    assert!(
        !expected.is_empty(),
        "no declared table takes its TTL from a setting"
    );
    // the next apply alters each of them once and changes nothing else
    let report = apply_declared(&session, &conf, "key-ttls").await?;
    let mut changes = report.config_changes.clone();
    changes.sort_by(|left, right| left.table.cmp(&right.table));
    expected.sort_by(|left, right| left.table.cmp(&right.table));
    assert_eq!(changes, expected);
    assert_eq!(
        report.statements.len(),
        expected.len(),
        "{:?}",
        report.statements
    );
    for change in &expected {
        let to = change
            .to
            .parse::<i32>()
            .map_err(|err| Error::new(err.to_string()))?;
        assert_eq!(live_ttl(&session, &keyspace, &change.table).await?, to);
    }
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// A keyspace that already exists without tablets is left as it is: `schema apply` creates the
/// declared objects in it and doesn't move it onto tablets
#[tokio::test]
async fn an_existing_keyspace_without_tablets_stays_without_them() -> Result<(), Error> {
    // create the keyspace the way a cluster without tablets did
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "apply_vnodes").await?;
    let keyspace = conf.thorium.namespace.clone();
    exec(
        &session,
        &format!(
            "CREATE KEYSPACE {keyspace} WITH replication = \
             {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}} \
             AND tablets = {{'enabled': false}}"
        ),
    )
    .await?;
    // apply converges it without altering the keyspace
    let report = apply_declared(&session, &conf, "key-vnodes").await?;
    assert!(
        !report
            .statements
            .iter()
            .any(|statement| statement.starts_with("ALTER KEYSPACE")),
        "{:?}",
        report.statements
    );
    let live = objects(&session, &keyspace).await?;
    for name in declared_names() {
        assert!(live.contains(name), "{name} wasn't created");
    }
    // the keyspace still has no tablets
    let tablets = session
        .query_unpaged(
            "SELECT initial_tablets FROM system_schema.scylla_keyspaces WHERE keyspace_name = ?",
            (&keyspace,),
        )
        .await
        .map_err(|err| Error::new(err.to_string()))?
        .into_rows_result()
        .map_err(|err| Error::new(err.to_string()))?;
    let initial = tablets
        .rows::<(Option<i32>,)>()
        .map_err(|err| Error::new(err.to_string()))?
        .map(|row| row.map(|(initial,)| initial))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| Error::new(err.to_string()))?;
    assert!(
        initial.iter().all(Option::is_none),
        "{keyspace} moved onto tablets: {initial:?}"
    );
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// Applying where a table holds the name of a declared view returns a clear refusal within a
/// minute instead of waiting for a view build that never starts
#[tokio::test]
async fn a_view_name_held_by_a_table_is_refused_instead_of_waited_for() -> Result<(), Error> {
    // apply, then replace the declared view `results_auth` with a table of the same name
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "apply_held").await?;
    let keyspace = conf.thorium.namespace.clone();
    apply_declared(&session, &conf, "key-held").await?;
    exec(
        &session,
        &format!("DROP MATERIALIZED VIEW {keyspace}.results_auth"),
    )
    .await?;
    exec(
        &session,
        &format!("CREATE TABLE {keyspace}.results_auth (id uuid PRIMARY KEY)"),
    )
    .await?;
    // the apply ends within the bound, naming the view
    let applied = tokio::time::timeout(
        Duration::from_secs(60),
        apply(&session, &conf, &declared(), "key-held", &|_| ()),
    )
    .await
    .map_err(|_| Error::new("the apply was still waiting after 60s"))?;
    match applied {
        Err(ApplyError::ViewHeldByTable { views }) => assert_eq!(views, ["results_auth"]),
        other => return Err(Error::new(format!("expected a refusal, got {other:?}"))),
    }
    // the table is untouched
    let tables = texts::<1>(
        &session,
        "SELECT table_name FROM system_schema.tables WHERE keyspace_name = ? AND table_name = ?",
        &[&keyspace.to_lowercase(), "results_auth"],
    )
    .await?;
    assert_eq!(tables.len(), 1);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}
