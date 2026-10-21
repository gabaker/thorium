//! Tests that every registered statement prepares and runs against the schema the API's startup
//! creates, and that the startup's prepare check names a statement that doesn't prepare

use chrono::Utc;
use scylla::client::session::Session;
use scylla::frame::response::result::{CollectionType, ColumnType, NativeType};
use scylla::value::{CqlTimestamp, CqlValue};
use thorium::Error;
use thorium::models::backends::setup::Scylla;
use thorium::models::schema::statements::{prepare_all, registry};
use thorium::test_utilities::{self, CONF};
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

/// Create a keyspace beside the run's own namespace with the API's own startup, which creates
/// the declared schema and prepares the API's statements
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `suffix` - What makes this test's keyspace name unique
async fn started_keyspace(session: &Session, suffix: &str) -> Result<String, Error> {
    // replace any keyspace an earlier run left behind
    let keyspace = format!("{}_{suffix}", CONF.thorium.namespace);
    exec(session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await?;
    // run the API's Scylla startup against the new keyspace
    let mut conf = CONF.clone();
    conf.thorium.namespace.clone_from(&keyspace);
    tokio::spawn(async move { Scylla::new(&conf).await.session })
        .await
        .map_err(|err| Error::new(format!("the API's startup failed: {err}")))?;
    Ok(keyspace)
}

/// A sample value of a bind marker's CQL type
///
/// # Arguments
///
/// * `typ` - The marker's type, as the prepared statement reports it
/// * `text` - The text every text value takes, unique to the run
fn sample(typ: &ColumnType<'_>, text: &str) -> Result<CqlValue, String> {
    // one value of each scalar type, and one element in each collection
    let value = match typ {
        ColumnType::Native(NativeType::Text | NativeType::Ascii) => CqlValue::Text(text.to_owned()),
        ColumnType::Native(NativeType::TinyInt) => CqlValue::TinyInt(1),
        ColumnType::Native(NativeType::SmallInt) => CqlValue::SmallInt(1),
        ColumnType::Native(NativeType::Int) => CqlValue::Int(60),
        ColumnType::Native(NativeType::BigInt) => CqlValue::BigInt(1),
        ColumnType::Native(NativeType::Boolean) => CqlValue::Boolean(true),
        ColumnType::Native(NativeType::Float) => CqlValue::Float(1.0),
        ColumnType::Native(NativeType::Double) => CqlValue::Double(1.0),
        ColumnType::Native(NativeType::Uuid) => CqlValue::Uuid(Uuid::new_v4()),
        ColumnType::Native(NativeType::Timestamp) => {
            CqlValue::Timestamp(CqlTimestamp(Utc::now().timestamp_millis()))
        }
        ColumnType::Collection { typ, .. } => match typ {
            CollectionType::List(inner) => CqlValue::List(vec![sample(inner, text)?]),
            CollectionType::Set(inner) => CqlValue::Set(vec![sample(inner, text)?]),
            CollectionType::Map(key, value) => {
                CqlValue::Map(vec![(sample(key, text)?, sample(value, text)?)])
            }
            other => return Err(format!("no sample value for {other:?}")),
        },
        other => return Err(format!("no sample value for {other:?}")),
    };
    Ok(value)
}

/// Every registered statement (the API's, the census, the search streamer's, and thoradm's
/// full-row statements) prepares against the schema the API's startup creates, and runs once
/// with a sample value of each marker's type
#[tokio::test]
async fn every_registered_statement_prepares_and_runs() -> Result<(), Error> {
    // build the schema with the API's own startup
    let session = test_utilities::scylla_session().await?;
    let keyspace = started_keyspace(&session, "registry").await?;
    // the startup's prepare check passes over the whole registry
    let prepared = prepare_all(&session, &keyspace)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    let statements = registry(&keyspace);
    assert_eq!(prepared, statements.len());
    // the search streamer's statements are part of the registry the tests check
    assert!(
        statements
            .iter()
            .any(|statement| statement.name.starts_with("search.")),
        "the search streamer's statements aren't registered"
    );
    // run each statement once, binding a sample of each marker's type, and keep every failure
    let text = format!("registry-{}", Uuid::new_v4());
    let mut failures = Vec::new();
    for statement in &statements {
        let outcome = async {
            // prepare the statement in the started keyspace
            let prepared = session
                .prepare(statement.cql.as_str())
                .await
                .map_err(|err| err.to_string())?;
            // a sample value for each of its markers, failing on a type with no sample
            let binds = prepared
                .get_variable_col_specs()
                .iter()
                .map(|spec| sample(spec.typ(), &text))
                .collect::<Result<Vec<_>, _>>()?;
            // run it once with those values
            session
                .execute_unpaged(&prepared, binds)
                .await
                .map_err(|err| err.to_string())?;
            Ok::<(), String>(())
        }
        .await;
        // keep the failure under the statement's name
        if let Err(err) = outcome {
            failures.push(format!("{}: {err}", statement.name));
        }
    }
    // every statement prepared and ran
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    // drop the test's keyspace
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// The startup's prepare check fails naming every statement that reads a column the keyspace
/// lacks, and only those
#[tokio::test]
async fn the_prepare_check_names_statements_that_fail() -> Result<(), Error> {
    // build the schema with the API's own startup, then drop a column no view reads
    let session = test_utilities::scylla_session().await?;
    let keyspace = started_keyspace(&session, "registry_broken").await?;
    exec(
        &session,
        &format!("ALTER TABLE {keyspace}.nodes DROP heart_beat"),
    )
    .await?;
    // the check fails, naming each statement that reads or writes the dropped column
    let err = prepare_all(&session, &keyspace)
        .await
        .expect_err("the prepare check passed without a column its statements name");
    let mut names = err
        .failures
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "nodes.INSERT",
            "nodes.SCAN",
            "nodes.get",
            "nodes.get_many",
            "nodes.list_details",
            "nodes.list_details_ties",
            "nodes.update_heart_beat",
        ],
        "{err}"
    );
    // the error message names the keyspace and each failed statement
    let message = err.to_string();
    assert!(message.contains(&keyspace), "{message}");
    assert!(message.contains("nodes.update_heart_beat"), "{message}");
    // the API's startup refuses the keyspace instead of serving with statements that can't run
    let mut conf = CONF.clone();
    conf.thorium.namespace.clone_from(&keyspace);
    let started = tokio::spawn(async move { Scylla::new(&conf).await.session }).await;
    assert!(started.is_err(), "the API started without nodes.heart_beat");
    // drop the test's broken keyspace
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}
