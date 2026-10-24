//! Tests of the API's startup against the live schema: it checks the applied fingerprint and the
//! data-shaping record, issues no DDL itself unless asked to apply the schema, and prepares every
//! statement before serving

use chrono::{DateTime, Datelike, Utc};
use scylla::client::session::Session;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::LazyLock;
use thorium::models::backends::setup::Scylla;
use thorium::models::schema::{
    ApplyOptions, ApplyTarget, Fingerprint, MigrationRecordKind, RevisionSnapshot, SchemaBackend,
    SchemaCheckError, SchemaCheckPass, SnapshotError, SnapshotObject, apply,
};
use thorium::models::upgrades::{Catalog, CatalogEntry};
use thorium::test_utilities::{self, CONF};
use thorium::utils::StartupOpts;
use thorium::utils::helpers::partition;
use thorium::{Conf, Error};

/// The revision this build declares in these tests
const OWN: &str = "2026-10-v10";

/// The older revision of the fixture catalog
const OLDER: &str = "2026-10-v09";

/// The later revision whose only changes are additions
const ADDITIVE: &str = "2026-10-v11";

/// The later revision that drops a column the API reads
const DROPPED: &str = "2026-10-v12";

/// The later revision that changes a column's type
const RETYPED: &str = "2026-10-v13";

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

/// Read the first text column of every row a query returns
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `cql` - The query, selecting one text column
/// * `binds` - The values to bind
async fn column(session: &Session, cql: &str, binds: &[&str]) -> Result<Vec<String>, Error> {
    // run the query and take its rows
    let rows = session
        .query_unpaged(cql, binds)
        .await
        .map_err(|err| Error::new(format!("{cql}: {err}")))?
        .into_rows_result()
        .map_err(|err| Error::new(format!("{cql}: {err}")))?;
    let mut out = Vec::new();
    for row in rows
        .rows::<(String,)>()
        .map_err(|err| Error::new(err.to_string()))?
    {
        out.push(row.map_err(|err| Error::new(err.to_string()))?.0);
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
    // point a copy of the test config at it, with a short setup time so a refused start is quick
    let mut conf = CONF.clone();
    conf.thorium.namespace = keyspace;
    conf.scylla.setup_time = 20;
    Ok(conf)
}

/// Whether a keyspace exists
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to look for
async fn keyspace_exists(session: &Session, keyspace: &str) -> Result<bool, Error> {
    // ask the schema tables for the keyspace
    let found = column(
        session,
        "SELECT keyspace_name FROM system_schema.keyspaces WHERE keyspace_name = ?",
        &[keyspace],
    )
    .await?;
    Ok(!found.is_empty())
}

/// Whether a keyspace holds a table
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to look in
/// * `table` - The table to look for
async fn has_table(session: &Session, keyspace: &str, table: &str) -> Result<bool, Error> {
    // ask the schema tables for the table
    let found = column(
        session,
        "SELECT table_name FROM system_schema.tables WHERE keyspace_name = ? AND table_name = ?",
        &[keyspace, table],
    )
    .await?;
    Ok(!found.is_empty())
}

/// Every table, view, and column a keyspace holds, which changes with any DDL on it
///
/// The cluster-wide schema version isn't used because other tests change the schema of their own
/// keyspaces at the same time.
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to describe
async fn shape(session: &Session, keyspace: &str) -> Result<Vec<String>, Error> {
    // list the tables, the views, and each column of either
    let mut shape = Vec::new();
    for (kind, cql) in [
        (
            "table",
            "SELECT table_name FROM system_schema.tables WHERE keyspace_name = ?",
        ),
        (
            "view",
            "SELECT view_name FROM system_schema.views WHERE keyspace_name = ?",
        ),
    ] {
        for name in column(session, cql, &[keyspace]).await? {
            shape.push(format!("{kind} {name}"));
        }
    }
    // then every column, with its table
    let columns = session
        .query_unpaged(
            "SELECT table_name, column_name FROM system_schema.columns WHERE keyspace_name = ?",
            (keyspace,),
        )
        .await
        .map_err(|err| Error::new(err.to_string()))?
        .into_rows_result()
        .map_err(|err| Error::new(err.to_string()))?;
    for row in columns
        .rows::<(String, String)>()
        .map_err(|err| Error::new(err.to_string()))?
    {
        let (table, column) = row.map_err(|err| Error::new(err.to_string()))?;
        shape.push(format!("column {table}.{column}"));
    }
    shape.sort();
    Ok(shape)
}

/// Apply the declared schema to a keyspace
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `conf` - The config naming the keyspace
async fn apply_declared(session: &Session, conf: &Conf) -> Result<(), Error> {
    // converge and record, turning a refusal into a test error
    let options = ApplyOptions {
        target: ApplyTarget::Declared,
        dry_run: false,
    };
    apply(session, conf, &options, "key-startup", &|_| ())
        .await
        .map_err(|err| Error::new(format!("schema apply failed: {err:?}")))?;
    Ok(())
}

/// Record a fingerprint as the one the deployment was applied at
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to write to
/// * `fingerprint` - The fingerprint to record
async fn record_fingerprint(
    session: &Session,
    keyspace: &str,
    fingerprint: &Fingerprint,
) -> Result<(), Error> {
    // overwrite the deployment's backend row
    exec(
        session,
        &format!(
            "UPDATE {keyspace}.schema_migrations SET fingerprint = '{}' \
             WHERE kind = '{}' AND id = 'deployment'",
            fingerprint.as_str(),
            MigrationRecordKind::Backend.as_str()
        ),
    )
    .await
}

/// The fingerprint recorded for the deployment
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to read
async fn recorded_fingerprint(session: &Session, keyspace: &str) -> Result<String, Error> {
    // read the deployment's backend row
    let rows = column(
        session,
        &format!(
            "SELECT fingerprint FROM {keyspace}.schema_migrations \
             WHERE kind = '{}' AND id = 'deployment'",
            MigrationRecordKind::Backend.as_str()
        ),
        &[],
    )
    .await?;
    rows.into_iter()
        .next()
        .ok_or_else(|| Error::new("no deployment row"))
}

/// Start the API's Scylla setup on a keyspace, in its own task as the API does
///
/// # Arguments
///
/// * `conf` - The config naming the keyspace
/// * `opts` - The startup options
async fn start(conf: &Conf, opts: StartupOpts) -> Result<Result<Scylla, SchemaCheckError>, Error> {
    // a panic inside the setup shows as a join error, which the caller reports
    let conf = conf.clone();
    tokio::spawn(async move { Scylla::start(&conf, opts).await })
        .await
        .map_err(|err| Error::new(format!("the setup panicked: {err}")))
}

/// Build a snapshot of a revision from objects, sorted and fingerprinted
///
/// # Arguments
///
/// * `revision` - The revision the snapshot belongs to
/// * `objects` - The objects it holds
fn snapshot(revision: &str, mut objects: Vec<SnapshotObject>) -> RevisionSnapshot {
    // objects are listed by backend and name
    objects.sort_by(|a, b| (a.backend, &a.name).cmp(&(b.backend, &b.name)));
    let backends = RevisionSnapshot::backend_fingerprints(&objects);
    RevisionSnapshot {
        revision: revision.parse().expect("a valid revision id"),
        deployment: RevisionSnapshot::deployment_fingerprint(&backends),
        backends,
        objects,
    }
}

/// Rebuild an object with a changed canonical form and the fingerprint that form gives
///
/// # Arguments
///
/// * `object` - The object to change
/// * `change` - Edits the canonical form
fn edited(object: &SnapshotObject, change: impl FnOnce(&mut Value)) -> SnapshotObject {
    // edit a copy of the canonical form and hash it
    let mut canonical = object.canonical.clone();
    change(&mut canonical);
    SnapshotObject {
        backend: object.backend,
        name: object.name.clone(),
        fingerprint: Fingerprint::of(&canonical),
        canonical,
    }
}

/// The snapshot of what this build declares
fn declared() -> RevisionSnapshot {
    // the declared objects at the build's own revision
    RevisionSnapshot::declared(OWN.parse().expect("a valid revision id"))
}

/// The declared snapshot with edits, as a later revision
///
/// # Arguments
///
/// * `revision` - The later revision
/// * `edit` - Edits the declared objects
fn later(revision: &str, edit: impl FnOnce(&mut Vec<SnapshotObject>)) -> RevisionSnapshot {
    // start from what this build declares
    let mut objects = declared().objects;
    edit(&mut objects);
    snapshot(revision, objects)
}

/// Edit one Scylla table's canonical form in a list of objects
///
/// # Arguments
///
/// * `objects` - The objects to change
/// * `table` - The table to edit
/// * `change` - Edits the table's canonical form
fn edit_table(objects: &mut [SnapshotObject], table: &str, change: impl FnOnce(&mut Value)) {
    // swap in the edited table
    let slot = objects
        .iter_mut()
        .find(|object| object.backend == SchemaBackend::Scylla && object.name == table)
        .expect("the table is declared");
    *slot = edited(slot, change);
}

/// The later revisions the fixture catalogs know, by revision
static LATER: LazyLock<BTreeMap<&'static str, RevisionSnapshot>> = LazyLock::new(|| {
    let mut later_snapshots = BTreeMap::new();
    // a new column and a new table
    later_snapshots.insert(
        ADDITIVE,
        later(ADDITIVE, |objects| {
            edit_table(objects, "nodes", |canonical| {
                canonical["columns"]
                    .as_array_mut()
                    .expect("columns")
                    .push(json!({"name": "zone", "type": "text"}));
            });
            let nodes = objects
                .iter()
                .find(|object| object.name == "nodes")
                .expect("nodes")
                .clone();
            let mut extra = edited(&nodes, |canonical| {
                canonical["name"] = json!("fixture_extra");
            });
            "fixture_extra".clone_into(&mut extra.name);
            objects.push(extra);
        }),
    );
    // a dropped column the API reads
    later_snapshots.insert(
        DROPPED,
        later(DROPPED, |objects| {
            edit_table(objects, "nodes", |canonical| {
                canonical["columns"]
                    .as_array_mut()
                    .expect("columns")
                    .retain(|column| column["name"] != "health");
            });
        }),
    );
    // a changed column type
    later_snapshots.insert(
        RETYPED,
        later(RETYPED, |objects| {
            edit_table(objects, "nodes", |canonical| {
                for column in canonical["columns"].as_array_mut().expect("columns") {
                    if column["name"] == "resources" {
                        column["type"] = json!("blob");
                    }
                }
            });
        }),
    );
    later_snapshots
});

/// Load a fixture revision's snapshot
///
/// # Arguments
///
/// * `revision` - The revision to load
fn load(revision: &str) -> Option<Result<RevisionSnapshot, SnapshotError>> {
    // only the later revisions have snapshots
    LATER.get(revision).cloned().map(Ok)
}

/// Build a fixture catalog that knows an older revision, this build's own, and one later revision
///
/// # Arguments
///
/// * `later_revision` - The later revision the catalog knows
fn catalog(later_revision: &'static str) -> Catalog {
    // leak the fingerprints the edges point at
    let leak = |fingerprint: &Fingerprint| -> &'static str {
        Box::leak(fingerprint.as_str().to_owned().into_boxed_str())
    };
    let edge = |from, id, schema| CatalogEntry {
        from,
        id,
        released_with: "0.0.0",
        description: "A fixture edge",
        schema: Some(schema),
        prerelease: false,
        sibling: false,
        steps: &[],
    };
    let older = leak(&Fingerprint::of(&json!("older")));
    let own = leak(&declared().deployment);
    let newer = leak(&LATER[later_revision].deployment);
    let edges = vec![
        edge(None, OLDER, older),
        edge(Some(OLDER), OWN, own),
        edge(Some(OWN), later_revision, newer),
    ];
    Catalog {
        edges: Box::leak(edges.into_boxed_slice()),
        floors: &[],
    }
}

/// Start the API's Scylla setup against a fixture catalog
///
/// # Arguments
///
/// * `conf` - The config naming the keyspace
/// * `later_revision` - The later revision the catalog knows
async fn start_later(
    conf: &Conf,
    later_revision: &'static str,
) -> Result<Result<(Scylla, SchemaCheckPass), SchemaCheckError>, Error> {
    // a panic inside the setup shows as a join error
    let conf = conf.clone();
    let catalog = catalog(later_revision);
    tokio::spawn(
        async move { Scylla::start_with(&conf, StartupOpts::default(), catalog, load).await },
    )
    .await
    .map_err(|err| Error::new(format!("the setup panicked: {err}")))
}

/// Starting against a cluster without the schema exits with the apply hint and creates nothing:
/// not the keyspace, and not `schema_migrations`
#[tokio::test]
async fn startup_without_the_schema_exits_with_the_apply_hint() -> Result<(), Error> {
    // no keyspace at all
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "su_t08").await?;
    let keyspace = conf.thorium.namespace.clone();
    let err = start(&conf, StartupOpts::default())
        .await?
        .err()
        .expect("the API started without a schema");
    assert_eq!(err, SchemaCheckError::Missing);
    assert!(err.to_string().contains("thoradm schema apply"), "{err}");
    assert!(
        !keyspace_exists(&session, &keyspace).await?,
        "the refused start created the keyspace"
    );
    // a keyspace without schema_migrations
    exec(
        &session,
        &format!(
            "CREATE KEYSPACE {keyspace} WITH replication = \
             {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}}"
        ),
    )
    .await?;
    let err = start(&conf, StartupOpts::default())
        .await?
        .err()
        .expect("the API started on an empty keyspace");
    assert_eq!(err, SchemaCheckError::Missing);
    assert!(
        !has_table(&session, &keyspace, "schema_migrations").await?,
        "the refused start created schema_migrations"
    );
    // a schema_migrations table that holds no deployment row
    apply_declared(&session, &conf).await?;
    exec(
        &session,
        &format!(
            "DELETE FROM {keyspace}.schema_migrations WHERE kind = '{}' AND id = 'deployment'",
            MigrationRecordKind::Backend.as_str()
        ),
    )
    .await?;
    let err = start(&conf, StartupOpts::default())
        .await?
        .err()
        .expect("the API started without a deployment record");
    assert_eq!(err, SchemaCheckError::Missing);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// Starting on an older applied fingerprint exits naming the revision this build needs
#[tokio::test]
async fn startup_on_an_older_fingerprint_names_the_revision() -> Result<(), Error> {
    // apply, then record a fingerprint the fixture catalog names as an older revision
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "su_t09").await?;
    let keyspace = conf.thorium.namespace.clone();
    apply_declared(&session, &conf).await?;
    let older = Fingerprint::of(&json!("older"));
    record_fingerprint(&session, &keyspace, &older).await?;
    let err = start_later(&conf, ADDITIVE)
        .await?
        .err()
        .expect("the API started on an older schema");
    assert_eq!(
        err,
        SchemaCheckError::Older {
            applied: older,
            declared: declared().deployment,
            revision: OWN.parse().expect("a valid revision id"),
        }
    );
    assert!(err.to_string().contains(OWN), "{err}");
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// With the apply flag the API applies the schema to an empty keyspace and then starts, recording
/// what it applied; without the flag the same start issues no DDL
#[tokio::test]
async fn the_apply_flag_applies_the_schema_before_starting() -> Result<(), Error> {
    // without the flag nothing is created
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "su_t10").await?;
    let keyspace = conf.thorium.namespace.clone();
    let refused = start(&conf, StartupOpts::default()).await?;
    assert!(refused.is_err());
    assert!(!keyspace_exists(&session, &keyspace).await?);
    // with the flag the schema is applied, recorded, and usable
    let opts = StartupOpts { apply_schema: true };
    let scylla = start(&conf, opts)
        .await?
        .map_err(|err| Error::new(format!("the API refused to apply and start: {err}")))?;
    assert_eq!(
        recorded_fingerprint(&session, &keyspace).await?,
        declared().deployment.as_str()
    );
    let read = scylla
        .session
        .query_unpaged(format!("SELECT sha256 FROM {keyspace}.s3_ids LIMIT 1"), &())
        .await;
    assert!(read.is_ok(), "the started API can't read: {read:?}");
    // starting again with the flag changes nothing
    let before = shape(&session, &keyspace).await?;
    start(&conf, opts)
        .await?
        .map_err(|err| Error::new(format!("the second start refused: {err}")))?;
    assert_eq!(shape(&session, &keyspace).await?, before);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// With a current schema the API prepares every statement and issues no DDL: the keyspace's
/// tables and columns don't change, and a table that went missing isn't created
#[tokio::test]
async fn startup_on_a_current_schema_issues_no_ddl() -> Result<(), Error> {
    // apply, then start without the flag
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "su_t11").await?;
    let keyspace = conf.thorium.namespace.clone();
    apply_declared(&session, &conf).await?;
    let before = shape(&session, &keyspace).await?;
    let started = start(&conf, StartupOpts::default())
        .await?
        .map_err(|err| Error::new(format!("the API refused a current schema: {err}")))?;
    assert_eq!(shape(&session, &keyspace).await?, before);
    drop(started);
    // a table that went missing stops the start at its statements, and isn't created
    exec(&session, &format!("DROP TABLE {keyspace}.nodes")).await?;
    let before = shape(&session, &keyspace).await?;
    let broken = start(&conf, StartupOpts::default()).await;
    assert!(broken.is_err(), "the API started without the nodes table");
    assert!(!has_table(&session, &keyspace, "nodes").await?);
    assert_eq!(shape(&session, &keyspace).await?, before);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// When the start of a later additive revision's keyspace is judged, the API starts, prepares
/// every statement, answers a read, and carries one warning naming the revision
#[tokio::test]
async fn startup_on_a_later_additive_revision_warns() -> Result<(), Error> {
    // record the later revision's fingerprint over a current keyspace
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "su_t37").await?;
    let keyspace = conf.thorium.namespace.clone();
    apply_declared(&session, &conf).await?;
    let fingerprint = LATER[ADDITIVE].deployment.clone();
    record_fingerprint(&session, &keyspace, &fingerprint).await?;
    let (scylla, pass) = start_later(&conf, ADDITIVE)
        .await?
        .map_err(|err| Error::new(format!("the API refused an additive schema: {err}")))?;
    assert_eq!(
        pass,
        SchemaCheckPass::NewerAdditive {
            applied: fingerprint,
            revision: ADDITIVE.parse().expect("a valid revision id"),
        }
    );
    let warning = pass.warning().expect("a later schema warns");
    assert!(warning.contains(ADDITIVE), "{warning}");
    let read = scylla
        .session
        .query_unpaged(format!("SELECT sha256 FROM {keyspace}.s3_ids LIMIT 1"), &())
        .await;
    assert!(read.is_ok(), "the started API can't read: {read:?}");
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// A later revision that drops a column the API reads, and one that changes a column's type, are
/// refused naming the revision and the object
#[tokio::test]
async fn startup_on_a_later_breaking_revision_exits() -> Result<(), Error> {
    // each breaking revision in turn
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "su_t38").await?;
    let keyspace = conf.thorium.namespace.clone();
    apply_declared(&session, &conf).await?;
    for (revision, object) in [(DROPPED, "nodes.health"), (RETYPED, "nodes.resources")] {
        record_fingerprint(&session, &keyspace, &LATER[revision].deployment).await?;
        let err = start_later(&conf, revision)
            .await?
            .err()
            .expect("the API started on a breaking schema");
        assert_eq!(
            err,
            SchemaCheckError::Newer {
                applied: LATER[revision].deployment.clone(),
                declared: declared().deployment,
                revision: revision.parse().expect("a valid revision id"),
                breaking: vec![object.to_owned()],
            }
        );
        let message = err.to_string();
        assert!(
            message.contains(revision) && message.contains(object),
            "{message}"
        );
    }
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// A fingerprint no revision names exits naming both fingerprints and the command that compares
/// them
#[tokio::test]
async fn startup_on_an_unknown_fingerprint_exits() -> Result<(), Error> {
    // record a fingerprint the fixture catalog doesn't know
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "su_t39").await?;
    let keyspace = conf.thorium.namespace.clone();
    apply_declared(&session, &conf).await?;
    let unknown = Fingerprint::of(&json!("written by a newer build"));
    record_fingerprint(&session, &keyspace, &unknown).await?;
    let err = start_later(&conf, ADDITIVE)
        .await?
        .err()
        .expect("the API started on an unknown schema");
    assert_eq!(
        err,
        SchemaCheckError::UnknownFingerprint {
            applied: unknown.clone(),
            declared: declared().deployment,
        }
    );
    let message = err.to_string();
    assert!(message.contains(unknown.as_str()), "{message}");
    assert!(
        message.contains(declared().deployment.as_str()),
        "{message}"
    );
    assert!(message.contains("thoradm schema diff"), "{message}");
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// When the files partition size or the files password differs from the record while data of that
/// type exists, startup exits naming the setting before preparing any statement, never printing a
/// password; with matching values it starts
#[tokio::test]
async fn startup_refuses_a_changed_data_shaping_setting() -> Result<(), Error> {
    // record the settings, then store a file at the recorded size
    let session = test_utilities::scylla_session().await?;
    let mut conf = fresh(&session, "su_t21").await?;
    let keyspace = conf.thorium.namespace.clone();
    let first = "l12-first-files-password";
    let second = "l12-second-files-password";
    conf.thorium.files.password = first.to_owned();
    apply_declared(&session, &conf).await?;
    let size = conf.thorium.files.partition_size;
    let uploaded = DateTime::parse_from_rfc3339("2026-04-10T00:00:05Z")
        .expect("a valid time")
        .with_timezone(&Utc);
    let bucket = partition(uploaded, uploaded.year(), size);
    exec(
        &session,
        &format!(
            "INSERT INTO {keyspace}.samples_list (group, year, bucket, uploaded, id) \
             VALUES ('g', {}, {bucket}, '{}', uuid())",
            uploaded.year(),
            uploaded.to_rfc3339()
        ),
    )
    .await?;
    // matching values start
    let started = start(&conf, StartupOpts::default()).await?;
    assert!(started.is_ok(), "{:?}", started.err());
    // drop a table, so a start that got as far as preparing statements would panic instead of
    // returning the refusal
    exec(&session, &format!("DROP TABLE {keyspace}.nodes")).await?;
    let before = shape(&session, &keyspace).await?;
    // a changed partition size is refused, naming the setting
    let mut changed = conf.clone();
    changed.thorium.files.partition_size = size * 20;
    let err = start(&changed, StartupOpts::default())
        .await?
        .err()
        .expect("the API started with a changed partition size");
    assert_eq!(
        err,
        SchemaCheckError::ConfigChanged {
            setting: "thorium.files.partition_size".to_owned()
        }
    );
    // a changed password is refused, naming the setting and neither password
    let mut other = conf.clone();
    other.thorium.files.password = second.to_owned();
    let err = start(&other, StartupOpts::default())
        .await?
        .err()
        .expect("the API started with a changed files password");
    assert_eq!(
        err,
        SchemaCheckError::ConfigChanged {
            setting: "thorium.files.password".to_owned()
        }
    );
    for text in [err.to_string(), format!("{err:?}")] {
        assert!(text.contains("files.password"), "{text}");
        assert!(!text.contains(first) && !text.contains(second), "{text}");
    }
    // none of it changed the schema
    assert_eq!(shape(&session, &keyspace).await?, before);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// With the apply flag, a refused data-shaping setting comes back as the final refusal the API
/// prints, at once, instead of a panic that is retried
#[tokio::test]
async fn an_apply_refusal_is_returned_not_retried() -> Result<(), Error> {
    // record one files password and store a file
    let session = test_utilities::scylla_session().await?;
    let mut conf = fresh(&session, "su_apply_refused").await?;
    let keyspace = conf.thorium.namespace.clone();
    let first = "l12-first-apply-password";
    conf.thorium.files.password = first.to_owned();
    apply_declared(&session, &conf).await?;
    let uploaded = DateTime::parse_from_rfc3339("2026-04-10T00:00:05Z")
        .expect("a valid time")
        .with_timezone(&Utc);
    let bucket = partition(uploaded, uploaded.year(), conf.thorium.files.partition_size);
    exec(
        &session,
        &format!(
            "INSERT INTO {keyspace}.samples_list (group, year, bucket, uploaded, id) \
             VALUES ('g', {}, {bucket}, '{}', uuid())",
            uploaded.year(),
            uploaded.to_rfc3339()
        ),
    )
    .await?;
    // another password with the apply flag is refused as a result, within a few seconds
    let mut other = conf.clone();
    other.thorium.files.password = "l12-second-apply-password".to_owned();
    let started = std::time::Instant::now();
    let err = start(&other, StartupOpts { apply_schema: true })
        .await?
        .err()
        .expect("the API applied with a changed files password");
    assert_eq!(
        err,
        SchemaCheckError::ConfigChanged {
            setting: "thorium.files.password".to_owned()
        }
    );
    assert!(started.elapsed().as_secs() < 15, "the refusal was retried");
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// A schema record that can't be read is retried like a connection failure, not returned as a
/// final refusal
#[tokio::test]
async fn an_unreadable_record_is_retried() -> Result<(), Error> {
    // apply, then break the record table so preparing its statements fails
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "su_unreadable").await?;
    let keyspace = conf.thorium.namespace.clone();
    apply_declared(&session, &conf).await?;
    exec(
        &session,
        &format!("ALTER TABLE {keyspace}.schema_migrations DROP fingerprint"),
    )
    .await?;
    // the start gives up by panicking after its attempts, which `start` reports as an error
    let Err(err) = start(&conf, StartupOpts::default()).await else {
        panic!("the unreadable record was returned as a final refusal");
    };
    assert!(
        err.to_string().contains("Failed to connect/setup Scylla"),
        "{err}"
    );
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}
