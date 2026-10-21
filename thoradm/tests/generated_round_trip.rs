//! Every backed-up table round trips through the statements generated from its declaration
//!
//! Each table's rows are read through the statement its backup prepares, the generated scan of
//! the table (for tags and notifications, a generated token-range select of the columns they
//! archive), converted the way restore converts them, and written through the statement its
//! restore prepares, the generated full-row insert, into a fresh keyspace created from the
//! declared schema; both keyspaces then hold the same rows, column by column.

mod common;

use chrono::{DateTime, Utc};
use common::fixture;
use common::harness::Harness;
use futures::StreamExt;
use scylla::client::session::Session;
use scylla::value::{CqlValue, Row};
use thoradm::backup::manifest::FORMAT_VERSION;
use thoradm::backup::tables::{
    Association, Comment, Commitish, CommitishList, Entity, NetworkPolicy, Node, Notification,
    Output, OutputStream, RepoData, RepoList, S3Id, SamplesList, Tag,
};
use thoradm::backup::{Backup, Restore};
use thorium::models::NodeHealth;
use thorium::models::schema::queries::ScyllaQueries;
use thorium::models::schema::scylla::{
    AssociationsTable, CommentsTable, CommitishListTable, CommitishesTable, EntitiesTable,
    NetworkPoliciesTable, NodesTable, NotificationsTable, RepoDataTable, ReposListTable,
    ResultsStreamTable, ResultsTable, S3IdsTable, SamplesListTable, TagsTable, ThoriumKeyspace,
};
use thorium::models::schema::{Keyspace, ScyllaObject, TableStatements, with_keyspace};

/// The columns restore deliberately rewrites, which the round trip doesn't compare, with why
const REWRITTEN: [(&str, &str, &str); 4] = [
    (
        "nodes",
        "health",
        "restore resets a node's health to registered",
    ),
    (
        "nodes",
        "heart_beat",
        "restore clears a node's last heartbeat",
    ),
    (
        "tags",
        "key_lower",
        "restore recomputes it as the lower-cased key",
    ),
    (
        "tags",
        "value_lower",
        "restore recomputes it as the lower-cased value",
    ),
];

/// Get a generated statement's text by name
///
/// # Arguments
///
/// * `ns` - The keyspace the statement targets
/// * `name` - The statement's name, as `<object>.<statement>`
fn generated(ns: &str, name: &str) -> String {
    // find the statement by name among every generated statement
    ScyllaQueries::statements(ns)
        .into_iter()
        .find(|(statement, _)| statement == name)
        .map_or_else(|| panic!("no generated statement {name}"), |(_, cql)| cql)
}

/// Create a fresh keyspace holding every declared table and view
///
/// # Arguments
///
/// * `scylla` - The session to create it with
/// * `ns` - The keyspace's name
async fn fresh_keyspace(scylla: &Session, ns: &str) {
    // start from nothing, then create every object in creation order
    scylla
        .query_unpaged(format!("DROP KEYSPACE IF EXISTS {ns}"), ())
        .await
        .unwrap();
    scylla
        .query_unpaged(
            format!(
                "CREATE KEYSPACE {ns} WITH REPLICATION = {{'class' : 'NetworkTopologyStrategy', \
                 'replication_factor': 1}}"
            ),
            (),
        )
        .await
        .unwrap();
    for object in <ThoriumKeyspace as Keyspace>::OBJECTS {
        scylla
            .query_unpaged(object.create_cql(ns, None), ())
            .await
            .unwrap();
    }
}

/// Read every row of a table as text, one entry per compared column, in a stable order
///
/// # Arguments
///
/// * `scylla` - The session to read with
/// * `ns` - The keyspace to read
/// * `table` - The table's name
/// * `select_all` - The table's generated select-all statement
async fn rows(scylla: &Session, ns: &str, table: &str, select_all: &str) -> Vec<String> {
    // name the columns the select-all statement reads, in its order
    let columns = select_all
        .trim_start_matches("SELECT ")
        .split(" FROM ")
        .next()
        .unwrap()
        .split(", ")
        .collect::<Vec<_>>();
    // read every row of the table
    let result = scylla
        .query_unpaged(with_keyspace(select_all, ns), ())
        .await
        .unwrap()
        .into_rows_result()
        .unwrap();
    let mut rendered = result
        .rows::<Row>()
        .unwrap()
        .map(|row| {
            // render each compared column by name
            row.unwrap()
                .columns
                .into_iter()
                .zip(&columns)
                .filter(|(_, column)| {
                    !REWRITTEN
                        .iter()
                        .any(|(rewritten, name, _)| *rewritten == table && name == *column)
                })
                .map(|(value, column)| format!("{column}={value:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .collect::<Vec<_>>();
    rendered.sort();
    rendered
}

/// Round trip one table: its backup and restore prepare the expected statements, and the rows
/// read and written through them land unchanged in the fresh keyspace
///
/// # Arguments
///
/// * `harness` - The harness naming the source keyspace
/// * `fresh` - The fresh keyspace to write into
/// * `scan` - The statement the backup must prepare, for the source keyspace
/// * `insert` - The statement the restore must prepare, for the fresh keyspace
async fn round_trip<T, O>(harness: &Harness, fresh: &str, scan: &str, insert: &str)
where
    T: Backup + Restore,
    O: TableStatements,
{
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    let table = <O as ScyllaObject>::NAME;
    // the backup reads through the generated scan, and the restore writes through the generated
    // full-row insert
    let read = <T as Backup>::prepared_statement(&scylla, ns)
        .await
        .unwrap();
    assert_eq!(read.get_statement(), scan, "{table} backup statement");
    let write = <T as Restore>::prepared_statement(&scylla, fresh)
        .await
        .unwrap();
    assert_eq!(write.get_statement(), insert, "{table} restore statement");
    // read every row of the ring, converting and writing each one as restore does
    let partition_conf = T::partition_conf(&harness.conf);
    let pager = scylla
        .execute_iter(read, (i64::MIN, i64::MAX))
        .await
        .unwrap();
    let mut stream = pager.rows_stream::<T>().unwrap();
    let mut copied = 0;
    while let Some(row) = stream.next().await {
        let row = row.unwrap_or_else(|err| panic!("a {table} row can't be read: {err}"));
        scylla
            .execute_unpaged(&write, row.values(&partition_conf, FORMAT_VERSION))
            .await
            .unwrap_or_else(|err| panic!("a {table} row can't be written: {err}"));
        copied += 1;
    }
    assert!(copied > 0, "the fixture wrote no {table} rows");
    // both keyspaces hold the same rows, column by column
    let source = rows(&scylla, ns, table, O::SELECT_ALL).await;
    let restored = rows(&scylla, fresh, table, O::SELECT_ALL).await;
    assert_eq!(source.len(), restored.len(), "{table} rows");
    for (source, restored) in source.iter().zip(&restored) {
        assert_eq!(source, restored, "{table}");
    }
}

/// Round trip a table whose backup and restore use its generated scan and full-row insert
///
/// # Arguments
///
/// * `harness` - The harness naming the source keyspace
/// * `fresh` - The fresh keyspace to write into
async fn level_one<T, O>(harness: &Harness, fresh: &str)
where
    T: Backup + Restore,
    O: TableStatements,
{
    let ns = &harness.conf.thorium.namespace;
    let scan = with_keyspace(O::SCAN, ns);
    let insert = with_keyspace(O::INSERT, fresh);
    round_trip::<T, O>(harness, fresh, &scan, &insert).await;
}

/// Read the remaining TTL of every notification's message, by its key
///
/// # Arguments
///
/// * `scylla` - The session to read with
/// * `ns` - The keyspace to read
async fn notification_ttls(scylla: &Session, ns: &str) -> Vec<(String, Option<i32>)> {
    // read every notification's key with its message's remaining TTL
    let result = scylla
        .query_unpaged(
            format!("SELECT kind, key, created, id, TTL(msg) FROM {ns}.notifications"),
            (),
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap();
    // pair each key with its TTL, if the message has one, in a stable order
    let mut ttls = result
        .rows::<Row>()
        .unwrap()
        .map(|row| {
            let mut columns = row.unwrap().columns;
            let ttl = match columns.pop().flatten() {
                Some(CqlValue::Int(ttl)) => Some(ttl),
                _ => None,
            };
            (format!("{columns:?}"), ttl)
        })
        .collect::<Vec<_>>();
    ttls.sort();
    ttls
}

/// Check the columns restore rewrites hold their rewritten values in the fresh keyspace: tags'
/// lower-cased key and value are the lower-cased key and value, and every node is registered
/// with no heartbeat
///
/// # Arguments
///
/// * `scylla` - The session to read with
/// * `ns` - The source keyspace
/// * `fresh` - The fresh keyspace restore wrote into
async fn assert_rewritten(scylla: &Session, ns: &str, fresh: &str) {
    // read every restored tag's key and value with their lower-cased copies
    let result = scylla
        .query_unpaged(
            format!("SELECT key, value, key_lower, value_lower FROM {fresh}.tags"),
            (),
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap();
    let tags = result
        .rows::<(String, String, String, String)>()
        .unwrap()
        .map(Result::unwrap)
        .collect::<Vec<_>>();
    // the fixture must hold a mixed-case tag, or a copied key would pass for a lower-cased one
    assert!(
        tags.iter()
            .any(|(key, value, ..)| *key != key.to_lowercase() || *value != value.to_lowercase()),
        "the fixture wrote no mixed-case tag"
    );
    // each tag's lower-cased copies are its key and value lower-cased
    for (key, value, key_lower, value_lower) in &tags {
        assert_eq!(*key_lower, key.to_lowercase(), "tag key {key}");
        assert_eq!(*value_lower, value.to_lowercase(), "tag value {value}");
    }
    // the fixture must hold a node that has checked in, or a kept heartbeat would pass for none
    let result = scylla
        .query_unpaged(format!("SELECT heart_beat FROM {ns}.nodes"), ())
        .await
        .unwrap()
        .into_rows_result()
        .unwrap();
    assert!(
        result
            .rows::<(Option<DateTime<Utc>>,)>()
            .unwrap()
            .any(|row| row.unwrap().0.is_some()),
        "the fixture wrote no node with a heartbeat"
    );
    // every restored node is registered and hasn't checked in since
    let registered = serde_json::to_string(&NodeHealth::Registered).unwrap();
    let result = scylla
        .query_unpaged(
            format!("SELECT node, health, heart_beat FROM {fresh}.nodes"),
            (),
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap();
    let nodes = result
        .rows::<(String, String, Option<DateTime<Utc>>)>()
        .unwrap()
        .map(Result::unwrap)
        .collect::<Vec<_>>();
    assert!(!nodes.is_empty(), "no nodes were restored");
    for (node, health, heart_beat) in &nodes {
        assert_eq!(*health, registered, "node {node} health");
        assert_eq!(*heart_beat, None, "node {node} heartbeat");
    }
}

/// Rows read through the generated scan of every table thoradm backs up and written back
/// through its generated full-row insert into a fresh keyspace are identical, column by column
#[tokio::test]
async fn backed_up_tables_round_trip_through_generated_statements() {
    // seed every table with every value class its columns allow
    let harness = Harness::get().await;
    let _guard = harness.shared_guard().await;
    Box::pin(fixture::broad(harness)).await;
    Box::pin(fixture::value_classes(harness)).await;
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    let fresh = format!("{ns}_fresh");
    fresh_keyspace(&scylla, &fresh).await;
    // each table round trips through its generated statements
    level_one::<SamplesList, SamplesListTable>(harness, &fresh).await;
    level_one::<S3Id, S3IdsTable>(harness, &fresh).await;
    level_one::<Comment, CommentsTable>(harness, &fresh).await;
    level_one::<RepoList, ReposListTable>(harness, &fresh).await;
    level_one::<RepoData, RepoDataTable>(harness, &fresh).await;
    level_one::<Commitish, CommitishesTable>(harness, &fresh).await;
    level_one::<CommitishList, CommitishListTable>(harness, &fresh).await;
    level_one::<Output, ResultsTable>(harness, &fresh).await;
    level_one::<OutputStream, ResultsStreamTable>(harness, &fresh).await;
    level_one::<Entity, EntitiesTable>(harness, &fresh).await;
    level_one::<Association, AssociationsTable>(harness, &fresh).await;
    level_one::<NetworkPolicy, NetworkPoliciesTable>(harness, &fresh).await;
    level_one::<Node, NodesTable>(harness, &fresh).await;
    // tags are read without the lower-cased key and value, which restore recomputes
    let scan = generated(ns, "tags.backup");
    let insert = with_keyspace(TagsTable::INSERT, &fresh);
    round_trip::<Tag, TagsTable>(harness, &fresh, &scan, &insert).await;
    // notifications are read with their remaining TTL and written back with it
    let scan = generated(ns, "notifications.backup");
    let insert = generated(&fresh, "notifications.restore");
    round_trip::<Notification, NotificationsTable>(harness, &fresh, &scan, &insert).await;
    // a permanent notification stays permanent, and an expiring one keeps at most the TTL it had at
    // backup time; the source is read after the backup, so allow for the seconds that passed
    let source = notification_ttls(&scylla, ns).await;
    let restored = notification_ttls(&scylla, &fresh).await;
    assert_eq!(source.len(), restored.len());
    for ((key, before), (restored_key, after)) in source.iter().zip(&restored) {
        assert_eq!(key, restored_key);
        match (before, after) {
            (None, None) => {}
            (Some(before), Some(after)) => {
                // a TTL read later than the backup may differ from it by the time between the reads
                assert!(*after <= before + 60, "{key}: {after} > {before}");
            }
            _ => panic!("{key}: the TTL {before:?} became {after:?}"),
        }
    }
    // the columns the row comparison leaves out hold the values restore rewrites them to
    assert_rewritten(&scylla, ns, &fresh).await;
    // the fresh keyspace isn't the harness's, so drop it here
    scylla
        .query_unpaged(format!("DROP KEYSPACE IF EXISTS {fresh}"), ())
        .await
        .unwrap();
}
