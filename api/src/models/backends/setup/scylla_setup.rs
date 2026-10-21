//! Setup Scylla for Thorium

use chrono::Duration;
use chrono::prelude::*;
use futures::{poll, task::Poll};
use scylla::client::session::Session;
use std::time::Duration as StdDuration;

mod entities;
mod events;

use events::EventsPreparedStatements;

use crate::models::schema::queries::ScyllaQueries;
use crate::models::schema::scylla::{AssociationsTable, ResultsTable, ThoriumKeyspace};
use crate::models::schema::statements::prepare_all;
use crate::models::schema::{Keyspace, ObjectDef, ScyllaObject, TableDef, connect};
use crate::{Conf, setup};

/// The diffferent groups of prepared statements for scylla
pub struct ScyllaPreparedStatements {
    /// The events related prepared statements
    pub events: EventsPreparedStatements,
}

impl ScyllaPreparedStatements {
    /// Create our scylla prepared statements
    ///
    /// # Arguments
    ///
    /// * `session` - A scylla session
    /// * `config` - The Thorium config
    pub async fn new(session: &Session, config: &Conf) -> Self {
        // create the entities by name view, which the schema declarations leave out
        entities::setup_entities_name_mat_view(session, config).await;
        // setup our preapred statements
        let events = EventsPreparedStatements::new(session, config).await;
        // build our grouped prepared statement object
        ScyllaPreparedStatements { events }
    }
}

/// The scylla client and prepared statments
pub struct Scylla {
    /// The scylla session object
    pub session: Session,
    /// prepared statements for scylla
    pub prep: ScyllaPreparedStatements,
    /// The generated statements, one queries struct per table and view
    pub queries: ScyllaQueries,
}

impl Scylla {
    /// Create a new scylla client
    ///
    /// # Arguments
    ///
    /// * `config` - The Thorium config
    pub async fn new(config: &Conf) -> Self {
        // loop and try to complete this future
        for _ in 0..4 {
            // get the correct timeout for scylla
            let timeout = Utc::now() + Duration::seconds(i64::from(config.scylla.setup_time));
            // get a clone of our config and log
            let config_clone = config.clone();
            // build the future for our setup
            let mut future = tokio::spawn(async move { build(config_clone).await });
            // timeout appears to just hang so were going to check it manually
            loop {
                // check if this future has completed yet
                if let Poll::Ready(join_result) = poll!(&mut future) {
                    // if this future has errored out then panic with that error
                    match join_result {
                        Ok(client) => return client,
                        // there was an error so print it and try again
                        Err(err) => {
                            // print our error and try again
                            setup!(
                                config.thorium.tracing.local.level,
                                format!("Scylla setup error {:#?}", err)
                            );
                            // try to connect to scylla again
                            break;
                        }
                    }
                }
                // check if we are past our timeout yet
                if Utc::now() > timeout {
                    setup!(
                        config.thorium.tracing.local.level,
                        format!(
                            "Failed to connect to scylla in {} seconds",
                            config.scylla.setup_time
                        )
                    );
                    break;
                }
                // sleep for 3 seconds
                tokio::time::sleep(StdDuration::from_millis(100)).await;
            }
        }
        // panic if we fail to connect
        panic!("Failed to connect/setup Scylla");
    }
}

/// Create a new session to scylla
///
/// Arguments
///
/// * `config` - The thorium config
pub async fn new_session(config: &Conf) -> Session {
    // connecting to scylla
    setup!(
        config.thorium.tracing.local.level,
        format!("Connecting to scylla at {}", config.scylla.nodes.join(", "))
    );
    // log the user we authenticate as, if any
    if let Some(creds) = &config.scylla.auth {
        setup!(
            config.thorium.tracing.local.level,
            format!("Authenticating to Scylla as {}", creds.username)
        );
    }
    // connect with the shared session settings
    connect(config)
        .await
        .expect("Failed to build scylla session")
}

/// Setup a keyspace for Thorium
///
/// # Arguments
///
/// * `session` - The scylla session to use
/// * `config` - The Thorium config
async fn setup_keyspace(session: &Session, config: &Conf) {
    // build keyspace create command
    let keyspace_cmd = format!(
        "CREATE KEYSPACE IF NOT EXISTS {ns} WITH REPLICATION = \
            {{'class' : 'NetworkTopologyStrategy', 'replication_factor': {repl_factor}}}",
        ns = &config.thorium.namespace,
        repl_factor = &config.scylla.replication
    );
    // setup scylla keyspaces
    session
        .query_unpaged(keyspace_cmd, &[])
        .await
        .expect("Failed to setup keyspace");
}

/// Check if a table in Thorium's keyspace has a column
///
/// # Arguments
///
/// * `session` - The scylla session to use
/// * `config` - The Thorium config
/// * `table` - The table to check
/// * `column` - The column to look for
async fn column_exists(session: &Session, config: &Conf, table: &str, column: &str) -> bool {
    // Scylla stores unquoted identifiers in lower case, so look them up that way
    let keyspace = config.thorium.namespace.to_lowercase();
    let table = table.to_lowercase();
    let column = column.to_lowercase();
    // look for this column in the schema tables
    session
        .query_unpaged(
            "SELECT column_name FROM system_schema.columns \
                WHERE keyspace_name = ? AND table_name = ? AND column_name = ?",
            (&keyspace, &table, &column),
        )
        .await
        .unwrap_or_else(|err| panic!("Failed to read the columns of {table}: {err}"))
        .into_rows_result()
        .is_ok_and(|rows| rows.rows_num() > 0)
}

/// Add a column to a table that was created before the column was added to its definition
///
/// Tables are created with `CREATE TABLE IF NOT EXISTS`, so a keyspace created by an older
/// release keeps its old columns, and statements naming newer columns fail to prepare. A column
/// that already exists, with any type, is left alone.
///
/// # Arguments
///
/// * `session` - The scylla session to use
/// * `config` - The Thorium config
/// * `table` - The declared table to add the column to
/// * `column` - The column to add
async fn add_missing_column(session: &Session, config: &Conf, table: &TableDef, column: &str) {
    // nothing needs to change when the column already exists
    if column_exists(session, config, table.name, column).await {
        return;
    }
    // add the column to the existing table with its declared type
    let alter = table
        .alter_add_cql(&config.thorium.namespace, column)
        .unwrap_or_else(|| panic!("{} declares no column {column}", table.name));
    if let Err(err) = session.query_unpaged(alter, &[]).await {
        // another API instance starting at the same time may have added it first
        assert!(
            column_exists(session, config, table.name, column).await,
            "Failed to add column {column} to {}: {err}",
            table.name
        );
    }
}

/// Get the definition of a declared table
///
/// # Arguments
///
/// * `object` - The declared table's definition
fn table_def(object: ObjectDef) -> TableDef {
    match object {
        ObjectDef::Table(table) => table,
        ObjectDef::View(view) => panic!("{} is a view, not a table", view.name),
    }
}

/// Create every declared table and view that doesn't exist yet, in creation order, and add the
/// columns keyspaces from older releases lack
///
/// # Arguments
///
/// * `session` - The scylla session to use
/// * `config` - The Thorium config
async fn create_declared_objects(session: &Session, config: &Conf) {
    // every object goes in the namespace's keyspace, created in declaration order
    let keyspace = &config.thorium.namespace;
    for object in <ThoriumKeyspace as Keyspace>::OBJECTS {
        // a table's default TTL comes from the setting its declaration names
        let ttl = match object {
            ObjectDef::Table(table) => table
                .ttl
                .and_then(|source| config.table_ttl(source.config_path)),
            ObjectDef::View(_) => None,
        };
        // create the object if it's missing
        let create = object.create_cql(keyspace, ttl);
        session
            .query_unpaged(create, &[])
            .await
            .unwrap_or_else(|err| panic!("Failed to create {}: {err}", object.name()));
    }
    // add the columns tables created before April 2026 lack
    let results = table_def(<ResultsTable as ScyllaObject>::DEF);
    add_missing_column(session, config, &results, "entities").await;
    // add the columns tables created before August 2026 lack
    let associations = table_def(<AssociationsTable as ScyllaObject>::DEF);
    add_missing_column(session, config, &associations, "submissions_source").await;
    add_missing_column(session, config, &associations, "submissions_other").await;
}

/// Build a session and setup tables/materialized views/prepared statements
async fn build(config: Conf) -> Scylla {
    // Create a new session for scylla
    let session = new_session(&config).await;
    // setup our keyspace if it doesn't already exist
    setup_keyspace(&session, &config).await;
    // create the declared tables and views from their definitions
    create_declared_objects(&session, &config).await;
    // get our prepared statements
    let prep = ScyllaPreparedStatements::new(&session, &config).await;
    // fail before serving when any registered statement doesn't prepare, naming each one
    if let Err(err) = prepare_all(&session, &config.thorium.namespace).await {
        panic!("{err}");
    }
    // prepare the generated statements
    let queries = ScyllaQueries::prepare(&session, &config.thorium.namespace)
        .await
        .expect("Failed to prepare the generated scylla statements");
    // build our scylla client
    Scylla {
        session,
        prep,
        queries,
    }
}
