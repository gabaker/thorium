//! Tests that the API starts on keyspaces created by releases older than its tables

use scylla::client::session::Session;
use thorium::Error;
use thorium::models::backends::setup::Scylla;
use thorium::test_utilities::{self, CONF};

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
    // seed a row in each table
    exec(
        session,
        &format!(
            "INSERT INTO {keyspace}.associations (group, year, bucket, source, created, target, \
            direction, kind, submitter) VALUES ('minerva', 2026, 7, 'corn', \
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
        async move { Scylla::new(&conf).await.session }
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
    let second = tokio::spawn(async move { Scylla::new(&conf).await.session }).await;
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
        async move { Scylla::new(&conf).await.session }
    })
    .await;
    assert!(fresh.is_ok(), "setup failed on a fresh keyspace");
    // setup adds the missing columns to a keyspace from an older release
    old_keyspace(&session, &keyspace).await?;
    let old = tokio::spawn(async move { Scylla::new(&conf).await.session }).await;
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
