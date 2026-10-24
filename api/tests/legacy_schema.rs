//! Tests that the API starts on keyspaces created by releases older than its tables

use chrono::{DateTime, Utc};
use scylla::client::session::Session;
use thorium::Error;
use thorium::models::backends::setup::Scylla;
use thorium::models::schema::ConfigRecord;
use thorium::test_utilities::{self, CONF};
use thorium::utils::StartupOpts;
use thorium::utils::helpers::partition;

/// The associations table as releases before August 2026 created it
const OLD_ASSOCIATIONS: &str = "CREATE TABLE {ns}.associations ( \
    group TEXT, year INT, bucket INT, created TIMESTAMP, direction TEXT, kind TEXT, \
    source TEXT, target TEXT, submitter TEXT, extra_source TEXT, extra_target TEXT, \
    PRIMARY KEY ((group, year, bucket, source), created, target, direction)) \
    WITH CLUSTERING ORDER BY (created DESC)";

/// The results table as releases before April 2026 created it
const OLD_RESULTS: &str = "CREATE TABLE {ns}.results ( \
    id UUID, uploaded TIMESTAMP, tool TEXT, tool_version TEXT, cmd TEXT, result TEXT, \
    files SET<TEXT>, display_type TEXT, children MAP<TEXT, UUID>, \
    PRIMARY KEY (id, uploaded)) WITH CLUSTERING ORDER BY (uploaded DESC)";

/// The events table as older releases created it
const OLD_EVENTS: &str = "CREATE TABLE IF NOT EXISTS {ns}.events (\
    event_type TEXT, \
    year INT, \
    bucket INT, \
    timestamp TIMESTAMP, \
    id UUID, \
    parent UUID, \
    user TEXT, \
    trigger_depth SMALLINT, \
    data TEXT, \
    PRIMARY KEY ((event_type, year, bucket), timestamp, id))
    WITH default_time_to_live = {ttl}";

/// The entities by name view as older releases created it
const OLD_ENTITIES_BY_NAME: &str = "CREATE MATERIALIZED VIEW IF NOT EXISTS {ns}.entities_by_name AS \
    SELECT name, group, kind, created, year, bucket, id, submitter, kind_data, description, image FROM {ns}.entities \
    WHERE name IS NOT NULL \
    AND group IS NOT NULL \
    AND kind IS NOT NULL \
    AND created IS NOT NULL \
    AND year IS NOT NULL \
    AND bucket IS NOT NULL \
    AND id IS NOT NULL
    PRIMARY KEY (name, group, kind, created, year, bucket, id)";

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

/// Get the columns of a table, sorted by name
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace the table is in
/// * `table` - The table to list the columns of
async fn columns(session: &Session, keyspace: &str, table: &str) -> Result<Vec<String>, Error> {
    // read the column names from the schema tables
    let rows = session
        .query_unpaged(
            "SELECT column_name FROM system_schema.columns \
            WHERE keyspace_name = ? AND table_name = ?",
            (keyspace, table),
        )
        .await
        .map_err(|err| Error::new(err.to_string()))?
        .into_rows_result()
        .map_err(|err| Error::new(err.to_string()))?;
    let mut names = rows
        .rows::<(String,)>()
        .map_err(|err| Error::new(err.to_string()))?
        .map(|row| row.map(|(name,)| name))
        .collect::<Result<Vec<String>, _>>()
        .map_err(|err| Error::new(err.to_string()))?;
    names.sort();
    Ok(names)
}

/// Create a keyspace with the associations and results tables an older release created, holding
/// one row each
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to create, replacing any left by an earlier run
async fn old_keyspace(session: &Session, keyspace: &str) -> Result<(), Error> {
    // replace any keyspace an earlier run left behind
    exec(session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await?;
    exec(
        session,
        &format!(
            "CREATE KEYSPACE {keyspace} WITH replication = \
            {{'class': 'SimpleStrategy', 'replication_factor': 1}}"
        ),
    )
    .await?;
    exec(session, &OLD_ASSOCIATIONS.replace("{ns}", keyspace)).await?;
    exec(session, &OLD_RESULTS.replace("{ns}", keyspace)).await?;
    // seed a row in each table, bucketed as the configured associations partition size gives
    let created = DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
        .expect("a valid time")
        .with_timezone(&Utc);
    let size = ConfigRecord::partition_sizes_of(&CONF)["associations"];
    let bucket = partition(created, 2026, size);
    exec(
        session,
        &format!(
            "INSERT INTO {keyspace}.associations (group, year, bucket, source, created, target, \
            direction, kind, submitter) VALUES ('minerva', 2026, {bucket}, 'corn', \
            '2026-01-02T03:04:05Z', 'husk', 'To', 'FileFor', 'io')"
        ),
    )
    .await?;
    exec(
        session,
        &format!(
            "INSERT INTO {keyspace}.results (id, uploaded, tool, tool_version, cmd, result) \
            VALUES (5b6e8a52-6c3f-4d1e-9c1a-0b9d2e3f4a5b, '2026-01-02T03:04:05Z', 'harvest', \
            '1.0', 'reap', '{{}}')"
        ),
    )
    .await?;
    Ok(())
}

/// The API's setup adds the columns that keyspaces from older releases lack, keeps their rows,
/// and changes nothing when it runs again
#[tokio::test]
async fn setup_adds_columns_older_keyspaces_lack() -> Result<(), Error> {
    // build a keyspace shaped like an older release, beside the run's own namespace
    let keyspace = format!("{}_legacy", CONF.thorium.namespace);
    let session = test_utilities::scylla_session().await?;
    old_keyspace(&session, &keyspace).await?;
    // start the API's Scylla setup against the old keyspace
    let mut conf = CONF.clone();
    conf.thorium.namespace.clone_from(&keyspace);
    let first = tokio::spawn({
        let conf = conf.clone();
        async move {
            Scylla::start(&conf, StartupOpts { apply_schema: true })
                .await
                .expect("the API starts")
                .session
        }
    })
    .await;
    assert!(
        first.is_ok(),
        "setup failed on a keyspace from an older release"
    );
    // the missing columns now exist
    let associations = columns(&session, &keyspace, "associations").await?;
    let results = columns(&session, &keyspace, "results").await?;
    for column in ["submissions_source", "submissions_other"] {
        assert!(
            associations.iter().any(|name| name == column),
            "associations lacks {column}"
        );
    }
    assert!(
        results.iter().any(|name| name == "entities"),
        "results lacks entities"
    );
    // the seeded rows read back unchanged, with the new columns empty
    let rows = session
        .query_unpaged(
            format!(
                "SELECT kind, submitter, submissions_source, submissions_other \
                FROM {keyspace}.associations"
            ),
            &(),
        )
        .await
        .map_err(|err| Error::new(err.to_string()))?
        .into_rows_result()
        .map_err(|err| Error::new(err.to_string()))?
        .rows::<(String, String, Option<uuid::Uuid>, Option<uuid::Uuid>)>()
        .map_err(|err| Error::new(err.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| Error::new(err.to_string()))?;
    assert_eq!(
        rows,
        vec![("FileFor".to_owned(), "io".to_owned(), None, None)]
    );
    let rows = session
        .query_unpaged(
            format!("SELECT tool, cmd, entities FROM {keyspace}.results"),
            &(),
        )
        .await
        .map_err(|err| Error::new(err.to_string()))?
        .into_rows_result()
        .map_err(|err| Error::new(err.to_string()))?
        .rows::<(
            String,
            String,
            Option<std::collections::HashMap<String, i64>>,
        )>()
        .map_err(|err| Error::new(err.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| Error::new(err.to_string()))?;
    assert_eq!(rows, vec![("harvest".to_owned(), "reap".to_owned(), None)]);
    // a second start succeeds and leaves the columns as they were
    let second = tokio::spawn(async move {
        Scylla::start(&conf, StartupOpts { apply_schema: true })
            .await
            .expect("the API starts")
            .session
    })
    .await;
    assert!(second.is_ok(), "a second setup failed");
    assert_eq!(
        columns(&session, &keyspace, "associations").await?,
        associations
    );
    assert_eq!(columns(&session, &keyspace, "results").await?, results);
    // remove our keyspace
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await?;
    Ok(())
}

/// The API's setup starts with a namespace holding upper case letters, which Scylla stores in
/// lower case, both on a fresh keyspace and on one from an older release
#[tokio::test]
async fn setup_handles_upper_case_namespaces() -> Result<(), Error> {
    // name a keyspace with upper case letters beside the run's own namespace
    let keyspace = format!("{}_MixedCase", CONF.thorium.namespace);
    let stored = keyspace.to_lowercase();
    let session = test_utilities::scylla_session().await?;
    let mut conf = CONF.clone();
    conf.thorium.namespace.clone_from(&keyspace);
    // setup creates a fresh keyspace
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await?;
    let fresh = tokio::spawn({
        let conf = conf.clone();
        async move {
            Scylla::start(&conf, StartupOpts { apply_schema: true })
                .await
                .expect("the API starts")
                .session
        }
    })
    .await;
    assert!(fresh.is_ok(), "setup failed on a fresh keyspace");
    // setup adds the missing columns to a keyspace from an older release
    old_keyspace(&session, &keyspace).await?;
    let old = tokio::spawn(async move {
        Scylla::start(&conf, StartupOpts { apply_schema: true })
            .await
            .expect("the API starts")
            .session
    })
    .await;
    assert!(
        old.is_ok(),
        "setup failed on a keyspace from an older release"
    );
    let associations = columns(&session, &stored, "associations").await?;
    assert!(
        associations.iter().any(|name| name == "submissions_source"),
        "associations lacks submissions_source"
    );
    let results = columns(&session, &stored, "results").await?;
    assert!(
        results.iter().any(|name| name == "entities"),
        "results lacks entities"
    );
    // remove our keyspace
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await?;
    Ok(())
}

/// Check whether a keyspace holds a table or a materialized view
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to look in
/// * `name` - The table or view to look for
async fn object_exists(session: &Session, keyspace: &str, name: &str) -> Result<bool, Error> {
    // look for the name among the keyspace's tables, then among its views
    for (schema_table, column) in [("tables", "table_name"), ("views", "view_name")] {
        let rows = session
            .query_unpaged(
                format!(
                    "SELECT {column} FROM system_schema.{schema_table} \
                    WHERE keyspace_name = ? AND {column} = ?"
                ),
                (keyspace, name),
            )
            .await
            .map_err(|err| Error::new(err.to_string()))?
            .into_rows_result()
            .map_err(|err| Error::new(err.to_string()))?;
        if rows.rows_num() > 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Count the rows of a table or view
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace the table or view is in
/// * `name` - The table or view to count
async fn count_rows(session: &Session, keyspace: &str, name: &str) -> Result<i64, Error> {
    // count every row of the table or view
    let (count,) = session
        .query_unpaged(format!("SELECT COUNT(*) FROM {keyspace}.{name}"), &())
        .await
        .map_err(|err| Error::new(err.to_string()))?
        .into_rows_result()
        .map_err(|err| Error::new(err.to_string()))?
        .first_row::<(i64,)>()
        .map_err(|err| Error::new(err.to_string()))?;
    Ok(count)
}

/// The API's setup of an empty keyspace creates neither the `events` table nor the
/// `entities_by_name` view, which nothing reads or writes
#[tokio::test]
async fn setup_creates_no_events_table_or_entities_by_name_view() -> Result<(), Error> {
    // start the API's Scylla setup against an empty keyspace beside the run's own namespace
    let keyspace = format!("{}_unused", CONF.thorium.namespace);
    let session = test_utilities::scylla_session().await?;
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await?;
    let mut conf = CONF.clone();
    conf.thorium.namespace.clone_from(&keyspace);
    let setup = tokio::spawn(async move {
        Scylla::start(&conf, StartupOpts { apply_schema: true })
            .await
            .expect("the API starts")
            .session
    })
    .await;
    assert!(setup.is_ok(), "setup failed on an empty keyspace");
    // the declared objects exist and the unused ones don't
    assert!(object_exists(&session, &keyspace, "entities").await?);
    assert!(object_exists(&session, &keyspace, "entities_by_id").await?);
    assert!(
        !object_exists(&session, &keyspace, "events").await?,
        "setup created the events table"
    );
    assert!(
        !object_exists(&session, &keyspace, "entities_by_name").await?,
        "setup created the entities_by_name view"
    );
    // remove our keyspace
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await?;
    Ok(())
}

/// The API's setup starts on a keyspace an older release created with the `events` table and the
/// `entities_by_name` view, and leaves both in place with their rows
#[tokio::test]
async fn setup_leaves_the_events_table_and_entities_by_name_view_in_place() -> Result<(), Error> {
    // build the declared objects in a keyspace beside the run's own namespace
    let keyspace = format!("{}_kept", CONF.thorium.namespace);
    let session = test_utilities::scylla_session().await?;
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await?;
    let mut conf = CONF.clone();
    conf.thorium.namespace.clone_from(&keyspace);
    let first = tokio::spawn({
        let conf = conf.clone();
        async move {
            Scylla::start(&conf, StartupOpts { apply_schema: true })
                .await
                .expect("the API starts")
                .session
        }
    })
    .await;
    assert!(first.is_ok(), "setup failed on an empty keyspace");
    // add the events table and the entities by name view as an older release created them
    let ttl = CONF.thorium.events.retention.to_string();
    exec(
        &session,
        &OLD_EVENTS.replace("{ns}", &keyspace).replace("{ttl}", &ttl),
    )
    .await?;
    exec(&session, &OLD_ENTITIES_BY_NAME.replace("{ns}", &keyspace)).await?;
    // seed an event and an entity the view holds
    exec(
        &session,
        &format!(
            "INSERT INTO {keyspace}.events (event_type, year, bucket, timestamp, id, user, \
            trigger_depth, data) VALUES ('NewTags', 2026, 7, '2026-01-02T03:04:05Z', \
            0c1d2e3f-4a5b-4c6d-8e7f-8091a2b3c4d5, 'io', 0, '{{}}') USING TTL 0"
        ),
    )
    .await?;
    exec(
        &session,
        &format!(
            "INSERT INTO {keyspace}.entities (kind, group, year, bucket, created, id, name, \
            submitter) VALUES ('Device', 'minerva', 2026, 7, '2026-01-02T03:04:05Z', \
            1d2e3f4a-5b6c-4d7e-8f90-a1b2c3d4e5f6, 'loom', 'io')"
        ),
    )
    .await?;
    // a second start succeeds on the keyspace holding both
    let second = tokio::spawn(async move {
        Scylla::start(&conf, StartupOpts { apply_schema: true })
            .await
            .expect("the API starts")
            .session
    })
    .await;
    assert!(
        second.is_ok(),
        "setup failed on a keyspace holding the events table and entities_by_name view"
    );
    // both are still there with their rows
    assert!(object_exists(&session, &keyspace, "events").await?);
    assert!(object_exists(&session, &keyspace, "entities_by_name").await?);
    assert_eq!(count_rows(&session, &keyspace, "events").await?, 1);
    assert_eq!(
        count_rows(&session, &keyspace, "entities_by_name").await?,
        1
    );
    // remove our keyspace
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await?;
    Ok(())
}
