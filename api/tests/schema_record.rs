//! Tests of the deployment's revision and history in `schema_migrations`: seeding from a probe or
//! from the operator's state, reading the record, copying the operator's entries, moving the
//! revision, and the per-Job result rows

use chrono::{DateTime, Duration, Utc};
use scylla::client::session::Session;
use std::collections::BTreeMap;
use thorium::models::schema::{
    ApplyError, ApplyOptions, ApplyTarget, ConfigAccept, ObjectDef, RecordError, RecordStatus,
    SchemaDefinition, SchemaObject, SeedRequest, append_history, apply, apply_objects,
    apply_seeding, claim_consumed_code, copy_operator_entries, history_id, read_job_result,
    read_status, record_revision, seed_from_state, write_job_result,
};
use thorium::models::upgrades::fixtures::{self, BASE, ONE_EDGE, RELEASE};
use thorium::models::upgrades::plan_fixtures::FUSED;
use thorium::models::upgrades::{AppliedStep, Catalog, StepState, latest};
use thorium::test_utilities::{self, CONF};
use thorium::{Conf, Error};

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
    // run the query
    let rows = session
        .query_unpaged(cql, binds)
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

/// The names of a table's columns
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace the table is in
/// * `table` - The table
async fn columns(session: &Session, keyspace: &str, table: &str) -> Result<Vec<String>, Error> {
    // read the column names from the schema tables
    let rows = texts::<1>(
        session,
        "SELECT column_name FROM system_schema.columns WHERE keyspace_name = ? AND table_name = ?",
        &[keyspace, table],
    )
    .await?;
    Ok(rows.into_iter().filter_map(|[name]| name).collect())
}

/// Whether a table exists in a keyspace
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace
/// * `table` - The table
async fn has_table(session: &Session, keyspace: &str, table: &str) -> Result<bool, Error> {
    // look the table up in the schema tables
    let rows = texts::<1>(
        session,
        "SELECT table_name FROM system_schema.tables WHERE keyspace_name = ? AND table_name = ?",
        &[keyspace, table],
    )
    .await?;
    Ok(!rows.is_empty())
}

/// Create a keyspace the way an older release did, from its setup statements
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to create
/// * `edit` - Changes the statements before they run
async fn create_legacy(
    session: &Session,
    keyspace: &str,
    edit: impl Fn(String) -> String,
) -> Result<(), Error> {
    // the keyspace, then every statement of the older setup
    exec(
        session,
        &format!(
            "CREATE KEYSPACE {keyspace} WITH replication = \
             {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}}"
        ),
    )
    .await?;
    let legacy = edit(
        include_str!("../src/models/schema/goldens/legacy_ddl.cql")
            .replace("{ns}", keyspace)
            .replace("{ttl}", "2592000"),
    );
    for statement in legacy
        .split(';')
        .map(str::trim)
        .filter(|statement| !statement.is_empty())
    {
        exec(session, statement).await?;
    }
    Ok(())
}

/// Drop the three newest columns from the legacy setup statements, as release 1.1.1 had them
///
/// # Arguments
///
/// * `ddl` - The legacy setup statements
fn without_new_columns(ddl: &str) -> String {
    // drop the two association columns and the results table's entities column
    let ddl = ddl.replace(", submissions_source UUID, submissions_other UUID", "");
    let start = ddl.find("entities MAP").expect("results declares entities");
    let end = start + ddl[start..].find(">,").expect("the map type ends") + 2;
    format!("{}{}", &ddl[..start], &ddl[end..])
}

/// Create the keyspace of release 1.1.1: the legacy setup without its three newest columns
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to create
async fn create_old(session: &Session, keyspace: &str) -> Result<(), Error> {
    // the legacy statements without the newest columns
    create_legacy(session, keyspace, |ddl| without_new_columns(&ddl)).await
}

/// The options that converge to the declared schema and execute
fn declared() -> ApplyOptions {
    ApplyOptions {
        target: ApplyTarget::Declared,
        dry_run: false,
    }
}

/// Apply the declared schema with no seed, turning a refusal into a test error
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `conf` - The config naming the keyspace
async fn apply_declared(session: &Session, conf: &Conf) -> Result<(), Error> {
    // converge and record
    apply(session, conf, &declared(), "key", &|_| ())
        .await
        .map_err(|err| Error::new(format!("schema apply failed: {err:?}")))?;
    Ok(())
}

/// Apply the declared schema, seeding a revision
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `conf` - The config naming the keyspace
/// * `catalog` - The catalog whose floor the seed must clear
/// * `revision` - The revision to seed
/// * `source` - Where the revision came from
async fn apply_seeded(
    session: &Session,
    conf: &Conf,
    catalog: Catalog,
    revision: &str,
    source: &str,
) -> Result<(), ApplyError> {
    // build the request and run the apply with it
    let seed = SeedRequest {
        revision: revision.parse().expect("a revision"),
        source: source.to_owned(),
        catalog,
    };
    apply_seeding(
        session,
        conf,
        &declared(),
        &ConfigAccept::default(),
        Some(&seed),
        "key",
        &|_| (),
    )
    .await
    .map(|_| ())
}

/// Build a history entry
///
/// # Arguments
///
/// * `revision` - The revision the step belongs to
/// * `step` - The step's id
/// * `outcome` - How the step ended
/// * `at` - When it ended
fn entry(revision: &str, step: &str, outcome: StepState, at: DateTime<Utc>) -> AppliedStep {
    // the fields the record keeps apart
    AppliedStep {
        revision: revision.to_owned(),
        step: step.to_owned(),
        outcome,
        message: Some(format!("{step} {outcome:?}")),
        at: Some(at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)),
        by: Some("thorium-operator 1.8.1".to_owned()),
        ..AppliedStep::default()
    }
}

/// The instant the entries of a test count from
fn start() -> DateTime<Utc> {
    // a fixed moment, so ids are the same on every run
    DateTime::parse_from_rfc3339("2026-10-22T00:00:00Z")
        .expect("a time")
        .with_timezone(&Utc)
}

/// Read the record of a keyspace, which must hold one
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `conf` - The config naming the keyspace
/// * `catalog` - The catalog deciding which steps are finished
async fn record_of(
    session: &Session,
    conf: &Conf,
    catalog: &Catalog,
) -> Result<thorium::models::schema::DeploymentRecord, Error> {
    // a keyspace with a record reads as one
    match read_status(session, conf, catalog)
        .await
        .map_err(|err| Error::new(err.to_string()))?
    {
        RecordStatus::Recorded { record, .. } => Ok(record),
        other => Err(Error::new(format!("expected a record, read {other:?}"))),
    }
}

/// A history entry reduced to what survives a round trip through the record: the time is compared
/// as an instant, not as text
///
/// # Arguments
///
/// * `applied` - The entry
fn shape(
    applied: &AppliedStep,
) -> (
    String,
    String,
    StepState,
    Option<String>,
    Option<DateTime<Utc>>,
) {
    // read the time as an instant
    let at = applied
        .at
        .as_deref()
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&Utc));
    (
        applied.revision.clone(),
        applied.step.clone(),
        applied.outcome,
        applied.message.clone(),
        at,
    )
}

/// A keyspace that doesn't exist, and one that exists with no tables, read as no Thorium keyspace
#[tokio::test]
async fn a_missing_or_empty_keyspace_reads_as_no_keyspace() -> Result<(), Error> {
    // a keyspace that doesn't exist
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "rec_none").await?;
    let status = read_status(&session, &conf, &Catalog::PRODUCTION).await;
    assert_eq!(status, Ok(RecordStatus::NoKeyspace));
    // a keyspace with nothing in it
    exec(
        &session,
        &format!(
            "CREATE KEYSPACE {} WITH replication = \
             {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}}",
            conf.thorium.namespace
        ),
    )
    .await?;
    let status = read_status(&session, &conf, &Catalog::PRODUCTION).await;
    assert_eq!(status, Ok(RecordStatus::NoKeyspace));
    exec(
        &session,
        &format!("DROP KEYSPACE IF EXISTS {}", conf.thorium.namespace),
    )
    .await
}

/// A fresh install's first apply records the latest revision with a history entry, and the status
/// reads it with the applied schema and the data-shaping record
#[tokio::test]
async fn a_fresh_apply_records_the_latest_revision() -> Result<(), Error> {
    // apply to a keyspace that doesn't exist
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "rec_fresh").await?;
    apply_declared(&session, &conf).await?;
    // the record holds the latest revision, one entry for the install, and the applied schema
    let record = record_of(&session, &conf, &Catalog::PRODUCTION).await?;
    assert_eq!(record.revision, latest());
    assert_eq!(record.history.len(), 1, "{:?}", record.history);
    assert_eq!(record.history[0].step, "record-revision");
    assert_eq!(record.history[0].outcome, StepState::Done);
    assert_eq!(record.history[0].revision, latest().to_string());
    assert!(
        record.history[0]
            .by
            .as_deref()
            .is_some_and(|by| by.starts_with("thoradm")),
        "{:?}",
        record.history[0].by
    );
    assert_eq!(record.history_len, 1);
    assert!(record.applied_by_version.is_some());
    assert_ne!(record.schema.deployment.as_str(), "");
    assert_ne!(record.config.partition_sizes.len(), 0);
    // a second apply changes neither the revision nor the history
    apply_declared(&session, &conf).await?;
    let again = record_of(&session, &conf, &Catalog::PRODUCTION).await?;
    assert_eq!(again.revision, record.revision);
    assert_eq!(again.history, record.history);
    exec(
        &session,
        &format!("DROP KEYSPACE IF EXISTS {}", conf.thorium.namespace),
    )
    .await
}

/// A keyspace with data and no record reads as having none, and an apply that doesn't seed leaves
/// it so, since only a fresh install is recorded at the latest revision
#[tokio::test]
async fn a_keyspace_with_data_and_no_record_reads_as_no_record() -> Result<(), Error> {
    // a keyspace an older release created
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "rec_norec").await?;
    create_legacy(&session, &conf.thorium.namespace, |ddl| ddl).await?;
    let status = read_status(&session, &conf, &Catalog::PRODUCTION).await;
    assert_eq!(status, Ok(RecordStatus::NoRecord { suggested: None }));
    // an apply without a seed creates the record tables but records no revision
    apply_declared(&session, &conf).await?;
    assert!(has_table(&session, &conf.thorium.namespace, "schema_migrations").await?);
    let status = read_status(&session, &conf, &Catalog::PRODUCTION).await;
    assert_eq!(status, Ok(RecordStatus::NoRecord { suggested: None }));
    exec(
        &session,
        &format!("DROP KEYSPACE IF EXISTS {}", conf.thorium.namespace),
    )
    .await
}

/// A seed against a schema matching the revision's snapshot records the revision and a history
/// entry naming the seed's source; a keyspace shaped like release 1.1.1 is accepted at
/// `2026-10-v01`, and the apply adds its three missing columns
#[tokio::test]
async fn a_seed_records_the_revision_and_names_its_source() -> Result<(), Error> {
    // the keyspace of release 1.1.1, which lacks three columns
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "rec_seed").await?;
    let keyspace = conf.thorium.namespace.clone();
    create_old(&session, &keyspace).await?;
    assert!(
        !columns(&session, &keyspace, "results")
            .await?
            .contains(&"entities".to_owned())
    );
    // seed it at the first revision
    let source = "spec.upgrade.starting_revision";
    apply_seeded(&session, &conf, Catalog::PRODUCTION, "2026-10-v01", source)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    // the revision and the entry that names where it came from are recorded
    let record = record_of(&session, &conf, &Catalog::PRODUCTION).await?;
    assert_eq!(record.revision.as_str(), "2026-10-v01");
    assert_eq!(record.history.len(), 1, "{:?}", record.history);
    assert_eq!(record.history[0].step, "seed-revision");
    assert_eq!(record.history[0].revision, "2026-10-v01");
    assert_eq!(record.history[0].outcome, StepState::Done);
    assert!(
        record.history[0]
            .message
            .as_deref()
            .is_some_and(|message| message.contains(source)),
        "{:?}",
        record.history[0].message
    );
    // the three missing columns were added
    assert!(
        columns(&session, &keyspace, "results")
            .await?
            .contains(&"entities".to_owned())
    );
    let associations = columns(&session, &keyspace, "associations").await?;
    assert!(associations.contains(&"submissions_source".to_owned()));
    assert!(associations.contains(&"submissions_other".to_owned()));
    // a second seed of the same revision changes nothing, and another revision is refused
    apply_seeded(&session, &conf, Catalog::PRODUCTION, "2026-10-v01", source)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    let again = record_of(&session, &conf, &Catalog::PRODUCTION).await?;
    assert_eq!(again.history, record.history);
    let refused = apply_seeded(&session, &conf, Catalog::PRODUCTION, "2026-10-v02", source).await;
    assert!(
        matches!(&refused, Err(ApplyError::SeedRefused { recorded }) if recorded == "2026-10-v01"),
        "{refused:?}"
    );
    let after = record_of(&session, &conf, &Catalog::PRODUCTION).await?;
    assert_eq!(after.revision.as_str(), "2026-10-v01");
    assert_eq!(after.history, record.history);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// A seed against a schema holding a changed object of the revision is refused, naming the
/// difference, and writes nothing: no record table, no added column, no revision
#[tokio::test]
async fn a_seed_against_a_changed_object_is_refused_and_writes_nothing() -> Result<(), Error> {
    // a keyspace of release 1.1.1 whose nodes' health column has another type
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "rec_mismatch").await?;
    let keyspace = conf.thorium.namespace.clone();
    create_legacy(&session, &keyspace, |ddl| {
        without_new_columns(&ddl.replace("health TEXT", "health INT"))
    })
    .await?;
    // the seed is refused, naming the column
    let refused = apply_seeded(
        &session,
        &conf,
        Catalog::PRODUCTION,
        "2026-10-v01",
        "--seed-revision",
    )
    .await;
    match &refused {
        Err(ApplyError::SeedMismatch {
            revision,
            differences,
        }) => {
            assert_eq!(revision, "2026-10-v01");
            assert!(
                differences
                    .iter()
                    .any(|difference| difference.starts_with("nodes.health")),
                "{differences:?}"
            );
        }
        other => panic!("expected a mismatch, got {other:?}"),
    }
    // nothing was written
    assert!(!has_table(&session, &keyspace, "schema_migrations").await?);
    assert!(
        !columns(&session, &keyspace, "results")
            .await?
            .contains(&"entities".to_owned())
    );
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// A seed below the catalog's support floor is refused naming the floor, and so is a revision with
/// no committed snapshot; neither writes anything
#[tokio::test]
async fn a_seed_below_the_floor_or_without_a_snapshot_is_refused() -> Result<(), Error> {
    // a keyspace of release 1.1.1
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "rec_floor").await?;
    let keyspace = conf.thorium.namespace.clone();
    create_old(&session, &keyspace).await?;
    // a catalog whose floor is above the revision
    let refused = apply_seeded(
        &session,
        &conf,
        fixtures::FLOORED,
        "2026-10-v01",
        "--seed-revision",
    )
    .await;
    assert!(
        matches!(
            &refused,
            Err(ApplyError::SeedBelowMinimum { revision, minimum })
                if revision == "2026-10-v01" && minimum == fixtures::FLOOR
        ),
        "{refused:?}"
    );
    // a revision no snapshot is committed for
    let refused = apply_seeded(
        &session,
        &conf,
        Catalog::PRODUCTION,
        "2026-10-v77",
        "--seed-revision",
    )
    .await;
    assert!(
        matches!(&refused, Err(ApplyError::SeedMismatch { .. })),
        "{refused:?}"
    );
    // nothing was written by either
    assert!(!has_table(&session, &keyspace, "schema_migrations").await?);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// Seeding from the operator's state records its revision and its whole history, creating the
/// table when it is missing; a second seed is refused, and the apply that follows changes neither
#[tokio::test]
async fn seeding_from_the_state_copies_the_revision_and_the_history_once() -> Result<(), Error> {
    // a keyspace an older release created, with no record table
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "rec_state").await?;
    let keyspace = conf.thorium.namespace.clone();
    create_legacy(&session, &keyspace, |ddl| ddl).await?;
    assert!(!has_table(&session, &keyspace, "schema_migrations").await?);
    // the state's history, three entries
    let entries = [
        entry(BASE, "convert-to-helm", StepState::Done, start()),
        entry(
            RELEASE,
            "release-step",
            StepState::Blocked,
            start() + Duration::seconds(5),
        ),
        entry(
            RELEASE,
            "release-step",
            StepState::Done,
            start() + Duration::seconds(9),
        ),
    ];
    let release = RELEASE.parse().expect("a revision");
    let copied = seed_from_state(&session, &conf, &release, "1.8.1", &entries)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    assert_eq!(copied, 3);
    // the record holds the revision and every entry, oldest first
    let record = record_of(&session, &conf, &ONE_EDGE).await?;
    assert_eq!(record.revision, release);
    assert_eq!(record.applied_by_version.as_deref(), Some("1.8.1"));
    assert_eq!(
        record.history.iter().map(shape).collect::<Vec<_>>(),
        entries.iter().map(shape).collect::<Vec<_>>()
    );
    assert_eq!(record.history_len, 3);
    // a second seed is refused naming the recorded revision
    let again = seed_from_state(&session, &conf, &release, "1.8.1", &entries).await;
    assert_eq!(
        again,
        Err(RecordError::Recorded {
            revision: RELEASE.to_owned()
        })
    );
    // the apply that follows changes neither
    apply_declared(&session, &conf).await?;
    let after = record_of(&session, &conf, &ONE_EDGE).await?;
    assert_eq!(after.revision, record.revision);
    assert_eq!(after.history, record.history);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// Seed a keyspace's record from the state at a revision, with entries
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `conf` - The config naming the keyspace
/// * `revision` - The revision to record
/// * `entries` - The history
async fn seed(
    session: &Session,
    conf: &Conf,
    revision: &str,
    entries: &[AppliedStep],
) -> Result<(), Error> {
    // seed from the state, turning a refusal into a test error
    let revision = revision.parse().expect("a revision");
    seed_from_state(session, conf, &revision, "1.8.1", entries)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    Ok(())
}

/// The operator's entries are copied once each; a `Done` entry of a gated step placed in the state
/// by hand is never copied; entries already in the record stay one row
#[tokio::test]
async fn copying_the_operators_entries_copies_each_once_and_never_a_gated_done() -> Result<(), Error>
{
    // a record with two entries
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "rec_copy").await?;
    create_legacy(&session, &conf.thorium.namespace, |ddl| ddl).await?;
    let known = [
        entry(BASE, "convert-to-helm", StepState::Done, start()),
        entry(
            RELEASE,
            "other",
            StepState::Running,
            start() + Duration::seconds(1),
        ),
    ];
    seed(&session, &conf, BASE, &known).await?;
    // the state holds those, a `Skipped` and a `Blocked` and a `Running` entry the operator
    // recorded since, and a `Done` entry for the gated step that the operator never started a run
    // of, which nothing but the step's own run writes
    let mut state = known.to_vec();
    state.push(entry(
        RELEASE,
        "users-v2",
        StepState::Skipped,
        start() + Duration::seconds(2),
    ));
    state.push(entry(
        RELEASE,
        "users-v2",
        StepState::Blocked,
        start() + Duration::seconds(3),
    ));
    state.push(entry(
        RELEASE,
        "users-v2",
        StepState::Done,
        start() + Duration::seconds(4),
    ));
    state.push(entry(
        RELEASE,
        "other",
        StepState::Running,
        start() + Duration::seconds(5),
    ));
    let copied = copy_operator_entries(&session, &conf, &FUSED, &state)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    assert_eq!(copied, 3);
    // a second copy over the same state copies nothing
    let again = copy_operator_entries(&session, &conf, &FUSED, &state)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    assert_eq!(again, 0);
    // the record holds the five entries, none of them the gated `Done`
    let record = record_of(&session, &conf, &FUSED).await?;
    assert_eq!(record.history.len(), 5, "{:?}", record.history);
    assert_eq!(record.history_len, 5);
    assert!(
        !record
            .history
            .iter()
            .any(|applied| applied.step == "users-v2" && applied.outcome == StepState::Done),
        "{:?}",
        record.history
    );
    assert!(
        record
            .finished
            .iter()
            .all(|finished| finished.step != "users-v2" || {
                // only the `Skipped` entry finishes the step
                record.history.iter().any(|applied| {
                    applied.step == "users-v2" && applied.outcome == StepState::Skipped
                })
            })
    );
    exec(
        &session,
        &format!("DROP KEYSPACE IF EXISTS {}", conf.thorium.namespace),
    )
    .await
}

/// The operator's `Done` of a gated step whose run it started is copied once, since the run's own
/// summary is the step finishing, and a record that already holds a `Done` of the step keeps one
#[tokio::test]
async fn the_operators_done_of_a_run_it_started_is_copied_once() -> Result<(), Error> {
    // a record whose history holds the operator's `Running` entry of the gated step
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "rec_copy_done").await?;
    create_legacy(&session, &conf.thorium.namespace, |ddl| ddl).await?;
    let known = [
        entry(BASE, "convert-to-helm", StepState::Done, start()),
        entry(
            RELEASE,
            "users-v2",
            StepState::Running,
            start() + Duration::seconds(1),
        ),
    ];
    seed(&session, &conf, BASE, &known).await?;
    // the operator recorded the step's `Done` from its Job's summary
    let mut state = known.to_vec();
    state.push(entry(
        RELEASE,
        "users-v2",
        StepState::Done,
        start() + Duration::seconds(2),
    ));
    // the `Done` is copied, and a second copy over the same state copies nothing
    let copied = copy_operator_entries(&session, &conf, &FUSED, &state)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    assert_eq!(copied, 1);
    let again = copy_operator_entries(&session, &conf, &FUSED, &state)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    assert_eq!(again, 0);
    // the record holds the step's one `Done`
    let record = record_of(&session, &conf, &FUSED).await?;
    let done = record
        .history
        .iter()
        .filter(|applied| applied.step == "users-v2" && applied.outcome == StepState::Done)
        .count();
    assert_eq!(done, 1, "{:?}", record.history);
    exec(
        &session,
        &format!("DROP KEYSPACE IF EXISTS {}", conf.thorium.namespace),
    )
    .await
}

/// The revision doesn't move while a step of the path has no `Done` or `Skipped` entry, naming the
/// step, and it moves once every step is recorded; with no record there is nothing to move
#[tokio::test]
async fn the_revision_moves_only_once_every_step_of_the_path_is_recorded() -> Result<(), Error> {
    // a keyspace with no record can't move
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "rec_move").await?;
    create_legacy(&session, &conf.thorium.namespace, |ddl| ddl).await?;
    apply_declared(&session, &conf).await?;
    let release = RELEASE.parse().expect("a revision");
    let none = record_revision(&session, &conf, &FUSED, &release, "1.8.1").await;
    assert_eq!(none, Err(RecordError::Unrecorded));
    // a record at the base, with the path's one step unrecorded
    seed(
        &session,
        &conf,
        BASE,
        &[entry(BASE, "convert-to-helm", StepState::Done, start())],
    )
    .await?;
    let refused = record_revision(&session, &conf, &FUSED, &release, "1.8.1").await;
    assert_eq!(
        refused,
        Err(RecordError::Unfinished {
            target: RELEASE.to_owned(),
            steps: vec![format!("{RELEASE}/users-v2")],
        })
    );
    assert_eq!(
        record_of(&session, &conf, &FUSED).await?.revision.as_str(),
        BASE
    );
    // a blocked note doesn't finish the step
    append_history(
        &session,
        &conf,
        &[entry(
            RELEASE,
            "users-v2",
            StepState::Blocked,
            start() + Duration::seconds(1),
        )],
    )
    .await
    .map_err(|err| Error::new(err.to_string()))?;
    assert!(
        record_revision(&session, &conf, &FUSED, &release, "1.8.1")
            .await
            .is_err()
    );
    // recorded `Done`, it does
    append_history(
        &session,
        &conf,
        &[entry(
            RELEASE,
            "users-v2",
            StepState::Done,
            start() + Duration::seconds(2),
        )],
    )
    .await
    .map_err(|err| Error::new(err.to_string()))?;
    record_revision(&session, &conf, &FUSED, &release, "1.8.2")
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    let record = record_of(&session, &conf, &FUSED).await?;
    assert_eq!(record.revision, release);
    assert_eq!(record.applied_by_version.as_deref(), Some("1.8.2"));
    exec(
        &session,
        &format!("DROP KEYSPACE IF EXISTS {}", conf.thorium.namespace),
    )
    .await
}

/// The layout versions Redis rows record in a keyspace's backend row
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace holding the record
/// * `backend` - The backend row
async fn layouts(
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

/// Moving the revision across a step recorded `Skipped` writes the step's target layout into the
/// backend row, so the record never lags data that needed no conversion; a step recorded `Done`
/// writes none, since its own run does
#[tokio::test]
async fn a_skipped_step_records_its_layout_when_the_revision_moves() -> Result<(), Error> {
    // two keyspaces whose Redis row records users in layout 1
    let session = test_utilities::scylla_session().await?;
    let mut keyspaces = Vec::new();
    for (suffix, outcome, expected) in [
        ("rec_skip", StepState::Skipped, 2),
        ("rec_done", StepState::Done, 1),
    ] {
        let conf = fresh(&session, suffix).await?;
        let keyspace = conf.thorium.namespace.clone();
        create_legacy(&session, &keyspace, |ddl| ddl).await?;
        apply_declared(&session, &conf).await?;
        exec(
            &session,
            &format!(
                "UPDATE {keyspace}.schema_migrations SET layouts['users'] = 1 \
                 WHERE kind = 'backend' AND id = 'redis'"
            ),
        )
        .await?;
        // the record is at the base and the step ended with the outcome
        seed(
            &session,
            &conf,
            BASE,
            &[entry(BASE, "convert-to-helm", StepState::Done, start())],
        )
        .await?;
        append_history(
            &session,
            &conf,
            &[entry(
                RELEASE,
                "users-v2",
                outcome,
                start() + Duration::seconds(1),
            )],
        )
        .await
        .map_err(|err| Error::new(err.to_string()))?;
        record_revision(
            &session,
            &conf,
            &FUSED,
            &RELEASE.parse().expect("a revision"),
            "1.8.1",
        )
        .await
        .map_err(|err| Error::new(err.to_string()))?;
        // the revision moved either way, and the layout only for the skipped step
        assert_eq!(
            record_of(&session, &conf, &FUSED).await?.revision.as_str(),
            RELEASE
        );
        assert_eq!(
            layouts(&session, &keyspace, "redis").await?.get("users"),
            Some(&expected),
            "{outcome:?}"
        );
        keyspaces.push(keyspace);
    }
    for keyspace in keyspaces {
        exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await?;
    }
    Ok(())
}

/// Count the history rows of a keyspace
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace holding the record
async fn history_ids(session: &Session, keyspace: &str) -> Result<Vec<String>, Error> {
    // read every history row's id, which Scylla returns in key order
    let rows = texts::<1>(
        session,
        &format!("SELECT id FROM {keyspace}.schema_migrations WHERE kind = 'history'"),
        &[],
    )
    .await?;
    Ok(rows.into_iter().filter_map(|[id]| id).collect())
}

/// History rows are keyed `<at>/<revision>/<step>/<outcome>`; an entry written again, as a pod
/// retried by its Job does, stays one row; two writers recording entries at once leave both; and
/// `history_len` equals the rows read
#[tokio::test]
async fn history_rows_are_keyed_by_their_entries_and_never_doubled() -> Result<(), Error> {
    // a record with one entry
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "rec_ids").await?;
    let keyspace = conf.thorium.namespace.clone();
    create_legacy(&session, &keyspace, |ddl| ddl).await?;
    seed(
        &session,
        &conf,
        BASE,
        &[entry(BASE, "convert-to-helm", StepState::Done, start())],
    )
    .await?;
    // a migration Job's `Running` entry, written by the pod and by two retries of it
    let running = entry(
        RELEASE,
        "users-v2",
        StepState::Running,
        start() + Duration::seconds(10),
    );
    for _ in 0..3 {
        append_history(&session, &conf, std::slice::from_ref(&running))
            .await
            .map_err(|err| Error::new(err.to_string()))?;
    }
    let ids = history_ids(&session, &keyspace).await?;
    let id = history_id(&running).map_err(|err| Error::new(err.to_string()))?;
    assert_eq!(
        ids.iter().filter(|candidate| **candidate == id).count(),
        1,
        "{ids:?}"
    );
    assert_eq!(
        id,
        format!(
            "{}/{RELEASE}/users-v2/Running",
            running.at.as_deref().expect("a time")
        )
    );
    // two writers recording at once leave both entries
    let one = entry(
        RELEASE,
        "writer-one",
        StepState::Done,
        start() + Duration::seconds(20),
    );
    let two = entry(
        RELEASE,
        "writer-two",
        StepState::Done,
        start() + Duration::seconds(20),
    );
    let (first, second) = tokio::join!(
        append_history(&session, &conf, std::slice::from_ref(&one)),
        append_history(&session, &conf, std::slice::from_ref(&two)),
    );
    first.map_err(|err| Error::new(err.to_string()))?;
    second.map_err(|err| Error::new(err.to_string()))?;
    let ids = history_ids(&session, &keyspace).await?;
    assert_eq!(ids.len(), 4, "{ids:?}");
    // the status reads them in time order, and counts as many as the rows
    let record = record_of(&session, &conf, &FUSED).await?;
    assert_eq!(record.history_len, ids.len() as u64);
    assert_eq!(record.history.len(), ids.len());
    let times = record
        .history
        .iter()
        .filter_map(|applied| applied.at.clone())
        .map(|at| DateTime::parse_from_rfc3339(&at).expect("a time"))
        .collect::<Vec<_>>();
    assert!(times.windows(2).all(|pair| pair[0] <= pair[1]), "{times:?}");
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// A step recorded `Done`, followed by 250 more entries, is in the finished set the status
/// computes over the whole history although the newest 200 entries it returns don't hold it
#[tokio::test]
async fn a_done_step_beyond_the_mirror_window_is_in_the_finished_set() -> Result<(), Error> {
    // a record whose history is a `Done` entry and 250 notes after it
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "rec_window").await?;
    create_legacy(&session, &conf.thorium.namespace, |ddl| ddl).await?;
    let mut entries = vec![entry(RELEASE, "release-step", StepState::Done, start())];
    entries.extend((1..=250).map(|index| {
        entry(
            RELEASE,
            "other-step",
            StepState::Blocked,
            start() + Duration::seconds(index),
        )
    }));
    seed(&session, &conf, BASE, &entries).await?;
    // the status returns the newest 200, counts all 251, and still finishes the step
    let record = record_of(&session, &conf, &ONE_EDGE).await?;
    assert_eq!(record.history.len(), 200);
    assert_eq!(record.history_len, 251);
    assert!(
        record
            .history
            .iter()
            .all(|applied| applied.step != "release-step")
    );
    assert_eq!(record.finished.len(), 1, "{:?}", record.finished);
    assert_eq!(record.finished[0].step, "release-step");
    assert_eq!(record.finished[0].revision.as_str(), RELEASE);
    exec(
        &session,
        &format!("DROP KEYSPACE IF EXISTS {}", conf.thorium.namespace),
    )
    .await
}

/// A Job's result larger than 4 KiB round trips intact under the Job's UID with a seven day TTL,
/// and another UID has none
#[tokio::test]
async fn a_job_result_round_trips_under_its_uid_with_a_ttl() -> Result<(), Error> {
    // a keyspace with the record tables
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "rec_result").await?;
    let keyspace = conf.thorium.namespace.clone();
    apply_declared(&session, &conf).await?;
    // a result of 200 history entries, far past the termination message's limit
    let entries = (0..200)
        .map(|index| {
            entry(
                RELEASE,
                "release-step",
                StepState::Blocked,
                start() + Duration::seconds(index),
            )
        })
        .collect::<Vec<_>>();
    let result = serde_json::json!({"state": "recorded", "history": entries.iter().map(|applied| serde_json::to_value(applied).expect("json")).collect::<Vec<_>>()});
    assert!(result.to_string().len() > 4096);
    let uid = "2f1c1a7e-0b0c-4d6e-9a44-7a6f2f3c9e01";
    write_job_result(&session, &conf, uid, &result)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    // it reads back as it was written
    let read = read_job_result(&session, &conf, uid)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    assert_eq!(read, Some(result));
    let other = read_job_result(&session, &conf, "another-uid")
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    assert_eq!(other, None);
    // the row expires after at most seven days
    let rows = session
        .query_unpaged(
            format!("SELECT TTL(result) FROM {keyspace}.schema_migrations WHERE kind = 'job_result' AND id = ?"),
            (uid,),
        )
        .await
        .map_err(|err| Error::new(err.to_string()))?
        .into_rows_result()
        .map_err(|err| Error::new(err.to_string()))?;
    let (ttl,) = rows
        .single_row::<(Option<i32>,)>()
        .map_err(|err| Error::new(err.to_string()))?;
    let ttl = ttl.expect("the result expires");
    assert!(
        ttl > 0 && ttl <= thorium::models::schema::RESULT_TTL_SECS,
        "{ttl}"
    );
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// A fresh install that stops after its first statements is recorded by the next apply: the
/// revision is written as soon as `schema_migrations` exists, so the retry finds it
#[tokio::test]
async fn r7b_an_interrupted_fresh_install_is_recorded_by_the_next_apply() -> Result<(), Error> {
    // a run that dies at a table no statement accepts, after the record table and others exist
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "rec_interrupt").await?;
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
    apply_objects(&session, &conf, &schema, false, "key", &|_| ())
        .await
        .expect_err("a statement that can't run was applied");
    // the next apply converges and the deployment is recorded at the latest revision
    apply_declared(&session, &conf).await?;
    let record = record_of(&session, &conf, &Catalog::PRODUCTION).await?;
    assert_eq!(record.revision, latest());
    assert_eq!(record.history.len(), 1, "{:?}", record.history);
    exec(
        &session,
        &format!("DROP KEYSPACE IF EXISTS {}", conf.thorium.namespace),
    )
    .await
}

/// A seed asked for where no keyspace or table exists is refused, since there is no data to
/// seed and the apply would create the latest schema under an older revision
#[tokio::test]
async fn r7b_a_seed_on_a_keyspace_without_tables_is_refused() -> Result<(), Error> {
    // no keyspace at all
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "rec_seednone").await?;
    let refused = apply_seeded(
        &session,
        &conf,
        Catalog::PRODUCTION,
        "2026-10-v01",
        "--seed-revision",
    )
    .await;
    assert!(
        matches!(&refused, Err(ApplyError::SeedMismatch { .. })),
        "{refused:?}"
    );
    // nothing was created
    let status = read_status(&session, &conf, &Catalog::PRODUCTION).await;
    assert_eq!(status, Ok(RecordStatus::NoKeyspace));
    Ok(())
}

/// The status never carries the files password's hash, which would reach the pod log and the
/// Job's result row
#[tokio::test]
async fn r7b_the_status_leaves_the_files_password_hash_out() -> Result<(), Error> {
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "rec_nopass").await?;
    apply_declared(&session, &conf).await?;
    let record = record_of(&session, &conf, &Catalog::PRODUCTION).await?;
    let text = serde_json::to_string(&record).map_err(|err| Error::new(err.to_string()))?;
    assert!(!text.contains("$argon2"), "{text}");
    assert_ne!(record.config.files_password, "");
    exec(
        &session,
        &format!("DROP KEYSPACE IF EXISTS {}", conf.thorium.namespace),
    )
    .await
}

/// A confirmation code is claimed once: the second claim finds its `consumed_code` row and says so,
/// whatever the history holds; the row isn't a history entry, so the history's length and the
/// status don't change
#[tokio::test]
async fn a_consumed_code_is_claimed_once_and_stays_out_of_the_history() -> Result<(), Error> {
    // a record with one entry
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "rec_consumed").await?;
    let keyspace = conf.thorium.namespace.clone();
    create_legacy(&session, &keyspace, |ddl| ddl).await?;
    seed(
        &session,
        &conf,
        BASE,
        &[entry(BASE, "convert-to-helm", StepState::Done, start())],
    )
    .await?;
    // the first claim takes the code, and the second finds it taken
    let running = entry(RELEASE, "users-v2", StepState::Running, start());
    assert!(
        claim_consumed_code(&session, &conf, "ABCD2345", &running)
            .await
            .map_err(|err| Error::new(err.to_string()))?
    );
    assert!(
        !claim_consumed_code(&session, &conf, "ABCD2345", &running)
            .await
            .map_err(|err| Error::new(err.to_string()))?
    );
    // another code is its own
    assert!(
        claim_consumed_code(&session, &conf, "EFGH6789", &running)
            .await
            .map_err(|err| Error::new(err.to_string()))?
    );
    // the row holds the entry of the run that claimed it
    let rows = texts::<2>(
        &session,
        &format!("SELECT id, entry FROM {keyspace}.schema_migrations WHERE kind = 'consumed_code'"),
        &[],
    )
    .await?;
    assert_eq!(rows.len(), 2, "{rows:?}");
    let held = rows
        .iter()
        .find(|[id, _]| id.as_deref() == Some("ABCD2345"))
        .and_then(|[_, entry]| entry.clone())
        .expect("the claimed row holds an entry");
    assert!(held.contains("users-v2"), "{held}");
    // it is no history entry
    let record = record_of(&session, &conf, &FUSED).await?;
    assert_eq!(record.history_len, 1);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}
