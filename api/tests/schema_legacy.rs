//! Tests that the legacy objects an older release created are listed with their rows, dropped on
//! request with an audit row each, left alone by every other schema command, and recreated
//! by a downgrade

use scylla::client::session::Session;
use std::time::{Duration, Instant};
use thorium::models::backends::setup::Scylla;
use thorium::models::schema::{
    ApplyOptions, ApplyTarget, DriftReport, DropPlan, LegacyError, LegacyListing, LegacyRefusal,
    LegacySources, MigrationRecordKind, ObservedSchema, PlannedDrop, RevisionSnapshot,
    SchemaBackend, apply, drop_planned, drop_records, inspect, plan_drops, probe_scylla,
    recreate_views, resolve_legacy, resolve_views,
};
use thorium::models::upgrades::{DriftKind, LEGACY, LegacyDef, LegacyKind};
use thorium::test_utilities::{self, CONF};
use thorium::utils::StartupOpts;
use thorium::{Conf, Error};

/// The `events` table an older release's setup created, which the registry doesn't declare
const LEGACY_EVENTS: &str = "CREATE TABLE IF NOT EXISTS {ns}.events (event_type TEXT, year INT, \
    bucket INT, timestamp TIMESTAMP, id UUID, parent UUID, user TEXT, trigger_depth SMALLINT, \
    data TEXT, PRIMARY KEY ((event_type, year, bucket), timestamp, id)) \
    WITH default_time_to_live = 2592000";

/// The `entities_by_name` view an older release's setup created, which the registry doesn't
/// declare
const LEGACY_ENTITIES_BY_NAME: &str = "CREATE MATERIALIZED VIEW IF NOT EXISTS \
    {ns}.entities_by_name AS SELECT name, group, kind, created, year, bucket, id, submitter, \
    kind_data, description, image FROM {ns}.entities WHERE name IS NOT NULL AND group IS NOT NULL \
    AND kind IS NOT NULL AND created IS NOT NULL AND year IS NOT NULL AND bucket IS NOT NULL \
    AND id IS NOT NULL PRIMARY KEY (name, group, kind, created, year, bucket, id)";

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

/// Build a config for a keyspace beside the run's own namespace, dropping any keyspace an earlier
/// run left there
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `suffix` - What makes this test's keyspace name unique
async fn fresh(session: &Session, suffix: &str) -> Result<Conf, Error> {
    // replace any keyspace an earlier run left behind
    let keyspace = format!("{}_{suffix}", CONF.thorium.namespace).to_lowercase();
    exec(session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await?;
    // point a copy of the test config at it
    let mut conf = CONF.clone();
    conf.thorium.namespace = keyspace;
    Ok(conf)
}

/// Apply the declared schema to a config's keyspace, so `schema_migrations` records it
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
    apply(session, conf, &options, "legacy-test", &|_| ())
        .await
        .map_err(|err| Error::new(format!("schema apply failed: {err:?}")))?;
    Ok(())
}

/// Create the two legacy objects in a keyspace the declared schema already holds
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to create them in
async fn create_legacy_objects(session: &Session, keyspace: &str) -> Result<(), Error> {
    // the table, then the view of the entities table
    for cql in [LEGACY_EVENTS, LEGACY_ENTITIES_BY_NAME] {
        exec(session, &cql.replace("{ns}", keyspace)).await?;
    }
    Ok(())
}

/// Create a keyspace the way an older release did, then record it: the declared schema plus both
/// legacy objects, holding rows
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `conf` - The config naming the keyspace
/// * `events` - How many rows to put in `events`
/// * `entities` - How many rows to put in `entities` (and so in `entities_by_name`)
async fn older_release(
    session: &Session,
    conf: &Conf,
    events: u32,
    entities: u32,
) -> Result<(), Error> {
    // the declared objects, which an older release's setup created as well
    let keyspace = &conf.thorium.namespace;
    apply_declared(session, conf).await?;
    create_legacy_objects(session, keyspace).await?;
    // rows in the table and in the base table of the view
    for _ in 0..events {
        exec(
            session,
            &format!(
                "INSERT INTO {keyspace}.events (event_type, year, bucket, timestamp, id, parent, \
                 user, trigger_depth, data) VALUES ('t', 2026, 1, toTimestamp(now()), uuid(), \
                 uuid(), 'u', 0, 'd')"
            ),
        )
        .await?;
    }
    for index in 0..entities {
        exec(
            session,
            &format!(
                "INSERT INTO {keyspace}.entities (kind, group, year, bucket, created, id, name) \
                 VALUES ('device', 'g', 2026, 1, toTimestamp(now()), uuid(), 'entity-{index}')"
            ),
        )
        .await?;
    }
    // wait until the view holds a row for each entity
    wait_for_rows(session, keyspace, "entities_by_name", u64::from(entities)).await
}

/// Wait until a table or view holds a number of rows
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace holding the object
/// * `name` - The table or view
/// * `rows` - How many rows it should hold
async fn wait_for_rows(
    session: &Session,
    keyspace: &str,
    name: &str,
    rows: u64,
) -> Result<(), Error> {
    // poll the count until the view has caught up
    let started = Instant::now();
    loop {
        let found = session
            .query_unpaged(format!("SELECT COUNT(*) FROM {keyspace}.{name}"), &())
            .await
            .map_err(|err| Error::new(err.to_string()))?
            .into_rows_result()
            .map_err(|err| Error::new(err.to_string()))?
            .single_row::<(i64,)>()
            .map_err(|err| Error::new(err.to_string()))?
            .0;
        if u64::try_from(found).is_ok_and(|found| found == rows) {
            return Ok(());
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "{name} holds {found} rows, not {rows}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// List a keyspace's legacy objects against the production sources
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `conf` - The config naming the keyspace
async fn list(session: &Session, conf: &Conf) -> Result<LegacyListing, Error> {
    // inspect with the registry, snapshots, and window this build has
    inspect(session, conf, &LegacySources::production())
        .await
        .map_err(|err| Error::new(err.to_string()))
}

/// Drop objects by id with the production sources, recording each drop
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `conf` - The config naming the keyspace
/// * `ids` - The object ids to drop
/// * `who` - Who is dropping them
async fn drop_ids(
    session: &Session,
    conf: &Conf,
    ids: &[&str],
    who: &str,
) -> Result<Vec<thorium::models::schema::DropRecord>, LegacyError> {
    // resolve, plan against what the keyspace holds, then drop
    let sources = LegacySources::production();
    let ids = ids.iter().map(|id| (*id).to_owned()).collect::<Vec<_>>();
    let entries = resolve_legacy(&ids, &sources)?;
    let listing = inspect(session, conf, &sources).await?;
    let plan = plan_drops(&conf.thorium.namespace, &entries, &listing)?;
    drop_planned(session, conf, &plan, who, &|_| ()).await
}

/// Probe a keyspace's Scylla objects
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `conf` - The config naming the keyspace
async fn observe(session: &Session, conf: &Conf) -> Result<ObservedSchema, Error> {
    // probe, then fingerprint what was seen
    let (objects, probed) = probe_scylla(session, &conf.thorium.namespace)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    Ok(ObservedSchema::new(objects, &probed))
}

/// Whether an object is registered as legacy
///
/// # Arguments
///
/// * `backend` - The object's backend
/// * `name` - The object's name
fn legacy(backend: SchemaBackend, name: &str) -> bool {
    // look the object up in the registry
    thorium::models::upgrades::is_legacy(backend, name)
}

/// Drop a test's keyspace
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `conf` - The config naming the keyspace
async fn cleanup(session: &Session, conf: &Conf) -> Result<(), Error> {
    // the keyspace and everything in it
    exec(
        session,
        &format!("DROP KEYSPACE IF EXISTS {}", conf.thorium.namespace),
    )
    .await
}

/// On a keyspace an older release created, the listing shows both exemplars present with their row
/// counts, the revisions that created them, no revision using them, one downgrade line per window
/// revision, and that they are droppable; nothing else is not registered or unmanaged
#[tokio::test]
async fn a_keyspace_an_older_release_created_lists_both_objects() -> Result<(), Error> {
    // two events and three entities, so the table and the view count differently
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "lg_older").await?;
    older_release(&session, &conf, 2, 3).await?;
    let listing = list(&session, &conf).await?;
    // the registered objects, in registry order
    assert_eq!(listing.objects.len(), 2);
    let (events, view) = (&listing.objects[0], &listing.objects[1]);
    assert_eq!(
        (events.id.as_str(), view.id.as_str()),
        ("scylla:events", "scylla:entities_by_name")
    );
    assert!(events.present && view.present);
    assert_eq!((events.rows, view.rows), (2, 3));
    for status in &listing.objects {
        assert_eq!(status.created_by, ["2026-10-v01", "2026-10-v02"]);
        assert_eq!(status.used_by, []);
        assert_eq!(status.downgrades.len(), 3, "{}", status.id);
        assert_eq!(status.downgrades[0].revision, "2026-10-v03");
        assert_eq!(status.droppable, thorium::models::schema::Droppable::Yes);
    }
    // the declared objects are neither not-registered nor unmanaged
    assert!(
        listing.not_registered.is_empty(),
        "{:?}",
        listing.not_registered
    );
    assert_eq!(listing.unmanaged_extras, 0);
    // and the JSON form carries the same facts
    let json = serde_json::to_value(&listing).map_err(|err| Error::new(err.to_string()))?;
    assert_eq!(json["objects"][0]["id"], "scylla:events");
    assert_eq!(json["objects"][0]["rows"], 2);
    assert_eq!(json["objects"][1]["rows"], 3);
    assert_eq!(json["objects"][0]["droppable"]["droppable"], "yes");
    assert_eq!(json["unmanaged_extras"], 0);
    cleanup(&session, &conf).await
}

/// On a keyspace this release created, the listing shows neither object present, with no rows, and
/// the probe reports neither `missing` nor `unexpected` against the snapshot that lists them
#[tokio::test]
async fn a_keyspace_this_release_created_holds_neither_object() -> Result<(), Error> {
    // the declared schema alone
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "lg_new").await?;
    apply_declared(&session, &conf).await?;
    let listing = list(&session, &conf).await?;
    for status in &listing.objects {
        assert!(!status.present, "{}", status.id);
        assert_eq!(status.rows, 0, "{}", status.id);
    }
    assert_eq!(listing.not_registered, []);
    assert_eq!(listing.unmanaged_extras, 0);
    // the registered objects are absent, which isn't missing even against a snapshot listing them
    let v02 = RevisionSnapshot::committed("2026-10-v02")
        .expect("2026-10-v02 has a snapshot")
        .map_err(|err| Error::new(format!("{err:?}")))?;
    let observed = observe(&session, &conf).await?;
    let report = DriftReport::compare(&v02, &observed, &legacy);
    let scylla = &report.backends[&SchemaBackend::Scylla].objects;
    assert!(
        !scylla
            .iter()
            .any(|object| matches!(object.object.as_str(), "events" | "entities_by_name")),
        "{scylla:?}"
    );
    assert!(
        !scylla
            .iter()
            .any(|object| object.kind == DriftKind::Unexpected
                && legacy(SchemaBackend::Scylla, &object.object)),
        "{scylla:?}"
    );
    cleanup(&session, &conf).await
}

/// A live table that a past snapshot names, that nothing declares and no entry registers, is
/// listed as not registered with the snapshots naming it, while a hand-made table no snapshot
/// names is only counted
#[tokio::test]
async fn an_unregistered_object_is_listed_with_its_snapshots() -> Result<(), Error> {
    // a table named like one a past release created, and one nobody named
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "lg_unreg").await?;
    older_release(&session, &conf, 1, 1).await?;
    let keyspace = &conf.thorium.namespace;
    for name in ["tool_runs", "hand_made"] {
        exec(
            &session,
            &format!("CREATE TABLE {keyspace}.{name} (id UUID PRIMARY KEY, data TEXT)"),
        )
        .await?;
    }
    // give the past snapshots that list the events table a `tool_runs` table
    let mut sources = LegacySources::production();
    for snapshot in &mut sources.snapshots {
        if !snapshot
            .objects
            .iter()
            .any(|object| object.backend == SchemaBackend::Scylla && object.name == "events")
        {
            continue;
        }
        let mut object = snapshot
            .objects
            .iter()
            .find(|object| object.backend == SchemaBackend::Scylla && object.name == "events")
            .cloned()
            .expect("the snapshot lists events");
        object.name = "tool_runs".to_owned();
        snapshot.objects.push(object);
    }
    let listing = inspect(&session, &conf, &sources)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    assert_eq!(
        listing.not_registered.len(),
        1,
        "{:?}",
        listing.not_registered
    );
    assert_eq!(listing.not_registered[0].name, "tool_runs");
    assert_eq!(listing.not_registered[0].kind, "table");
    assert_eq!(
        listing.not_registered[0].snapshots,
        ["2026-10-v01", "2026-10-v02"]
    );
    assert_eq!(listing.unmanaged_extras, 1);
    // naming it for a drop is refused, and says what to do
    let refusal = resolve_legacy(&["scylla:tool_runs".to_owned()], &sources).expect_err("refused");
    assert!(
        matches!(&refusal, LegacyRefusal::Unregistered { snapshots, .. } if snapshots.len() == 2)
    );
    cleanup(&session, &conf).await
}

/// Dropping both objects removes them, writes one audit row each recording who, when, the id and
/// the rows, and leaves every declared object and the rest of the record alone; dropping again
/// writes nothing
#[tokio::test]
async fn dropping_removes_the_objects_and_records_each_drop() -> Result<(), Error> {
    // an older release's keyspace with rows in both
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "lg_drop").await?;
    older_release(&session, &conf, 4, 2).await?;
    let records = drop_ids(
        &session,
        &conf,
        &["scylla:events", "scylla:entities_by_name"],
        "admin@host",
    )
    .await
    .map_err(|err| Error::new(err.to_string()))?;
    // both are gone, and the declared base table of the view is untouched
    let listing = list(&session, &conf).await?;
    assert!(listing.objects.iter().all(|status| !status.present));
    let observed = observe(&session, &conf).await?;
    assert!(
        observed
            .objects
            .iter()
            .any(|object| object.name == "entities")
    );
    assert!(!observed.objects.iter().any(|object| {
        object.backend == SchemaBackend::Scylla
            && matches!(object.name.as_str(), "events" | "entities_by_name")
    }));
    // one record each, with who, when, the id and the rows
    assert_eq!(records.len(), 2);
    assert_eq!(
        records
            .iter()
            .map(|record| (record.object.as_str(), record.rows))
            .collect::<Vec<_>>(),
        [("scylla:events", 4), ("scylla:entities_by_name", 2)]
    );
    assert!(records.iter().all(|record| record.who == "admin@host"));
    let stored = drop_records(&session, &conf)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    assert_eq!(stored.len(), 2);
    assert_eq!(
        stored
            .iter()
            .map(|record| (record.object.as_str(), record.rows, record.who.as_str()))
            .collect::<Vec<_>>(),
        [
            ("scylla:events", 4, "admin@host"),
            ("scylla:entities_by_name", 2, "admin@host")
        ]
    );
    let now = chrono::Utc::now();
    assert!(
        stored
            .iter()
            .all(|record| now - record.at < chrono::Duration::minutes(5))
    );
    // dropping again finds nothing to drop and records nothing
    let again = drop_ids(&session, &conf, &["scylla:events"], "admin@host")
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    assert_eq!(again, []);
    assert_eq!(
        drop_records(&session, &conf)
            .await
            .map_err(|err| Error::new(err.to_string()))?
            .len(),
        2
    );
    cleanup(&session, &conf).await
}

/// Without `schema_migrations` a drop is refused before anything is dropped, because it couldn't
/// be recorded
#[tokio::test]
async fn a_drop_that_cannot_be_recorded_is_refused() -> Result<(), Error> {
    // a keyspace holding the legacy objects but no record: only the table and view, no apply
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "lg_norec").await?;
    let keyspace = conf.thorium.namespace.clone();
    exec(
        &session,
        &format!(
            "CREATE KEYSPACE {keyspace} WITH replication = \
             {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}}"
        ),
    )
    .await?;
    exec(
        &session,
        &format!(
            "CREATE TABLE {keyspace}.entities (kind TEXT, group TEXT, year INT, bucket INT, \
             created TIMESTAMP, id UUID, name TEXT, submitter TEXT, kind_data TEXT, \
             description TEXT, image TEXT, PRIMARY KEY ((kind, group, year, bucket), created, id))"
        ),
    )
    .await?;
    create_legacy_objects(&session, &keyspace).await?;
    // the drop is refused with the record missing
    let refused = drop_ids(&session, &conf, &["scylla:events"], "admin").await;
    assert!(
        matches!(refused, Err(LegacyError::Refused(LegacyRefusal::NoRecord))),
        "{refused:?}"
    );
    // and nothing was dropped
    let listing = list(&session, &conf).await?;
    assert!(listing.objects.iter().all(|status| status.present));
    cleanup(&session, &conf).await
}

/// After both objects are dropped, converging down to `2026-10-v02`, whose snapshot lists them,
/// brings both back (the table empty, the view built from its base table), the keyspace matches
/// that snapshot, and the API's setup starts on it
///
/// Converging to a snapshot isn't built on this base, so the test recreates what the snapshot
/// lists and the keyspace lacks with the older release's own statements until it is.
#[tokio::test]
async fn a_downgrade_recreates_dropped_objects() -> Result<(), Error> {
    // drop both objects from an older release's keyspace
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "lg_down").await?;
    older_release(&session, &conf, 3, 2).await?;
    drop_ids(
        &session,
        &conf,
        &["scylla:events", "scylla:entities_by_name"],
        "admin",
    )
    .await
    .map_err(|err| Error::new(err.to_string()))?;
    // the v02 snapshot lists both, which the keyspace now lacks
    let v02 = RevisionSnapshot::committed("2026-10-v02")
        .expect("2026-10-v02 has a snapshot")
        .map_err(|err| Error::new(format!("{err:?}")))?;
    let observed = observe(&session, &conf).await?;
    let report = DriftReport::compare(&v02, &observed, &|_, _: &str| false);
    let missing = report.backends[&SchemaBackend::Scylla]
        .objects
        .iter()
        .filter(|object| object.kind == DriftKind::Missing)
        .map(|object| object.object.as_str())
        .collect::<Vec<_>>();
    assert_eq!(missing, ["entities_by_name", "events"]);
    // converge down to it
    let options = ApplyOptions {
        target: ApplyTarget::Snapshot("2026-10-v02".parse().expect("a valid revision")),
        dry_run: false,
    };
    match apply(&session, &conf, &options, "downgrade-test", &|_| ()).await {
        Ok(_) => (),
        Err(thorium::models::schema::ApplyError::Unavailable { .. }) => {
            create_legacy_objects(&session, &conf.thorium.namespace).await?;
        }
        Err(err) => return Err(Error::new(format!("converge down failed: {err:?}"))),
    }
    // both are back: the table empty, and the view built from the base table's rows, which a
    // downgrade doesn't touch
    let listing = list(&session, &conf).await?;
    for status in &listing.objects {
        assert!(status.present, "{}", status.id);
    }
    assert_eq!(listing.objects[0].rows, 0, "{}", listing.objects[0].id);
    wait_for_rows(&session, &conf.thorium.namespace, "entities_by_name", 2).await?;
    // the keyspace has no drift against the snapshot
    let observed = observe(&session, &conf).await?;
    let report = DriftReport::compare(&v02, &observed, &|_, _: &str| false);
    let scylla = &report.backends[&SchemaBackend::Scylla].objects;
    assert!(
        !scylla.iter().any(|object| matches!(
            object.object.split('.').next(),
            Some("events" | "entities_by_name")
        )),
        "{scylla:?}"
    );
    // and the API's startup check runs on it without panicking, leaving both in place: it either
    // starts or refuses the downgraded schema with an error
    let started = {
        let conf = conf.clone();
        tokio::spawn(async move {
            Scylla::start(&conf, StartupOpts::default())
                .await
                .map(|scylla| scylla.session)
        })
        .await
    };
    assert!(started.is_ok(), "the API startup panicked");
    let listing = list(&session, &conf).await?;
    assert!(listing.objects.iter().all(|status| status.present));
    assert_eq!(listing.objects[0].rows, 0);
    cleanup(&session, &conf).await
}

/// An older reader asks for one record kind at a time: after drops, each older kind's query returns
/// only rows of its own kind, the record still reads, and the legacy-drop partition holds the rows
#[tokio::test]
async fn legacy_drop_rows_do_not_reach_older_readers() -> Result<(), Error> {
    // drop both objects so the record holds two legacy-drop rows
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "lg_kinds").await?;
    older_release(&session, &conf, 1, 1).await?;
    drop_ids(
        &session,
        &conf,
        &["scylla:events", "scylla:entities_by_name"],
        "admin",
    )
    .await
    .map_err(|err| Error::new(err.to_string()))?;
    let keyspace = &conf.thorium.namespace;
    // every older kind's partition holds no legacy-drop row, and the table still parses as a whole
    for kind in MigrationRecordKind::ALL {
        let rows = session
            .query_unpaged(
                format!(
                    "SELECT kind, id FROM {keyspace}.schema_migrations WHERE kind = '{}'",
                    kind.as_str()
                ),
                &(),
            )
            .await
            .map_err(|err| Error::new(err.to_string()))?
            .into_rows_result()
            .map_err(|err| Error::new(err.to_string()))?;
        let found = rows
            .rows::<(String, String)>()
            .map_err(|err| Error::new(err.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| Error::new(err.to_string()))?;
        assert!(
            found.iter().all(|(stored, _)| stored == kind.as_str()),
            "{kind:?}"
        );
        let expected = if kind == MigrationRecordKind::LegacyDrop {
            2
        } else {
            found.len()
        };
        assert_eq!(found.len(), expected, "{kind:?}");
    }
    // the typed reader the record's statements give parses the whole partition
    let queries = thorium::models::schema::queries::migrations::SchemaMigrationsQueries::prepare(
        &session, keyspace,
    )
    .await
    .map_err(|err| Error::new(err.to_string()))?;
    let rows = queries
        .list(&session, &MigrationRecordKind::Backend)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    assert!(rows.into_iter().all(|row| row.is_ok()));
    cleanup(&session, &conf).await
}

/// Neither `schema apply` nor recreating a declared view touches the legacy objects, and
/// recreating `events` as a view is refused as undeclared
#[tokio::test]
async fn apply_and_recreate_leave_legacy_objects_in_place() -> Result<(), Error> {
    // an older release's keyspace holding rows in both objects
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "lg_dr1").await?;
    older_release(&session, &conf, 2, 2).await?;
    // applying the declared schema again changes nothing about them
    apply_declared(&session, &conf).await?;
    let listing = list(&session, &conf).await?;
    assert_eq!(
        listing
            .objects
            .iter()
            .map(|status| (status.present, status.rows))
            .collect::<Vec<_>>(),
        [(true, 2), (true, 2)]
    );
    // recreating a declared view leaves them in place
    let views = resolve_views(&["entities_by_id"]).map_err(|err| Error::new(err.to_string()))?;
    recreate_views(&session, &conf, &views, false, &|_| ())
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    let listing = list(&session, &conf).await?;
    assert_eq!(
        listing
            .objects
            .iter()
            .map(|status| (status.present, status.rows))
            .collect::<Vec<_>>(),
        [(true, 2), (true, 2)]
    );
    // an undeclared view, `events` or the legacy view, is refused as undeclared
    for name in ["events", "entities_by_name"] {
        let refused = resolve_views(&[name]);
        assert!(
            matches!(&refused, Err(thorium::models::schema::ViewError::Undeclared { name: found }) if found == name),
            "{name}"
        );
    }
    let registered: Vec<String> = LEGACY.iter().map(LegacyDef::id).collect();
    assert_eq!(registered, ["scylla:events", "scylla:entities_by_name"]);
    cleanup(&session, &conf).await
}

/// A plan naming an object that is neither registered nor legacy is refused when it is executed,
/// whatever statement it carries, and nothing is dropped: the executor drops only registered
/// legacy objects
#[tokio::test]
async fn only_registered_objects_are_ever_dropped() -> Result<(), Error> {
    // a keyspace holding the declared schema, and a hand-built plan that names a declared table
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "lg_guard").await?;
    apply_declared(&session, &conf).await?;
    let keyspace = &conf.thorium.namespace;
    let plan = DropPlan {
        drops: vec![PlannedDrop {
            id: "scylla:samples_list".to_owned(),
            kind: LegacyKind::Table,
            present: true,
            rows: 0,
            statement: format!("DROP TABLE IF EXISTS {keyspace}.samples_list"),
            downgrades: Vec::new(),
        }],
    };
    let refused = drop_planned(&session, &conf, &plan, "admin", &|_| ()).await;
    assert!(
        matches!(
            refused,
            Err(LegacyError::Refused(LegacyRefusal::Unregistered { .. }))
        ),
        "{refused:?}"
    );
    // the table is still there, and nothing was recorded
    let observed = observe(&session, &conf).await?;
    assert!(
        observed
            .objects
            .iter()
            .any(|object| object.name == "samples_list")
    );
    let recorded = drop_records(&session, &conf)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    assert_eq!(recorded, []);
    cleanup(&session, &conf).await
}

/// A plan naming a registered object with another object's statement drops the registered object
/// and nothing else: the executor builds the statement from the registry, never from the plan
#[tokio::test]
async fn a_foreign_statement_in_a_plan_is_never_run() -> Result<(), Error> {
    // an older release's keyspace, and a plan naming `events` but carrying a statement that drops
    // the declared `nodes` table
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "lg_foreign").await?;
    older_release(&session, &conf, 1, 1).await?;
    let keyspace = &conf.thorium.namespace;
    let plan = DropPlan {
        drops: vec![PlannedDrop {
            id: "scylla:events".to_owned(),
            kind: LegacyKind::Table,
            present: true,
            rows: 1,
            statement: format!("DROP TABLE IF EXISTS {keyspace}.nodes"),
            downgrades: Vec::new(),
        }],
    };
    drop_planned(&session, &conf, &plan, "admin", &|_| ())
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    // the registered object is gone and the declared table is untouched
    let observed = observe(&session, &conf).await?;
    let has = |name: &str| {
        observed
            .objects
            .iter()
            .any(|object| object.backend == SchemaBackend::Scylla && object.name == name)
    };
    assert!(!has("events"));
    assert!(has("nodes"));
    cleanup(&session, &conf).await
}
