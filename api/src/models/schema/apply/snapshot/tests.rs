//! Tests of the statements rendered from a snapshot's canonical forms

use serde_json::{Value, json};
use std::collections::BTreeSet;

use super::{table_cql, view_cql};
use crate::models::schema::scylla::ThoriumKeyspace;
use crate::models::schema::{Keyspace, ObjectDef, SnapshotObject};
use crate::models::upgrades::SchemaBackend;

/// The keyspace the statements are rendered for
const KEYSPACE: &str = "testing_render";

/// Split text at the commas that sit outside every `<...>` pair, so a map type stays whole
///
/// # Arguments
///
/// * `text` - The text to split
fn split_top(text: &str) -> Vec<String> {
    // walk the text keeping the depth of nested type arguments
    let mut parts = Vec::new();
    let mut depth = 0_i32;
    let mut current = String::new();
    for c in text.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            _ => (),
        }
        if c == ',' && depth == 0 {
            parts.push(current.trim().to_owned());
            current.clear();
        } else {
            current.push(c);
        }
    }
    parts.push(current.trim().to_owned());
    parts
}

/// The column definitions of a `CREATE TABLE` statement as a set, and the text from its
/// `PRIMARY KEY` on
///
/// # Arguments
///
/// * `cql` - The statement
fn table_parts(cql: &str) -> (BTreeSet<String>, String) {
    // the columns sit between the first parenthesis and the primary key clause
    let start = cql.find(" (").expect("a column list") + 2;
    let key = cql.find(", PRIMARY KEY").expect("a primary key");
    let columns = split_top(&cql[start..key]).into_iter().collect();
    (columns, cql[key..].to_owned())
}

/// The selected columns of a `CREATE MATERIALIZED VIEW` statement as a set, and the text from its
/// `FROM` on
///
/// # Arguments
///
/// * `cql` - The statement
fn view_parts(cql: &str) -> (BTreeSet<String>, String) {
    // the columns sit between `SELECT` and `FROM`
    let start = cql.find("SELECT ").expect("a select list") + 7;
    let from = cql.find(" FROM ").expect("a base table");
    let columns = split_top(&cql[start..from]).into_iter().collect();
    (columns, cql[from..].to_owned())
}

/// Every declared Scylla object with its canonical form
fn declared() -> Vec<(ObjectDef, Value)> {
    // pair each declared definition with the entry a snapshot of this build holds
    let entries = SnapshotObject::declared();
    ThoriumKeyspace::OBJECTS
        .iter()
        .map(|def| {
            let entry = entries
                .iter()
                .find(|entry| entry.backend == SchemaBackend::Scylla && entry.name == def.name())
                .expect("a declared object has a snapshot entry");
            (*def, entry.canonical.clone())
        })
        .collect()
}

/// Every declared table renders from its canonical form to the same columns, keys, clustering
/// order, and options its declaration renders to
#[test]
fn tables_render_like_their_declarations() {
    // compare each declared table's two statements, ignoring the columns' order
    let mut tables = 0;
    for (def, canonical) in declared() {
        let ObjectDef::Table(table) = def else {
            continue;
        };
        let expected = table_parts(&table.create_cql(KEYSPACE, Some(60)));
        let rendered = table_parts(&table_cql(KEYSPACE, &canonical, Some(60)));
        assert_eq!(rendered, expected, "{}", table.name);
        tables += 1;
    }
    assert!(tables > 15, "only {tables} tables compared");
}

/// Every declared view renders from its canonical form to the same selected columns, base table,
/// conditions, key, and clustering order its declaration renders to
#[test]
fn views_render_like_their_declarations() {
    // compare each declared view's two statements, ignoring the selected columns' order
    let mut views = 0;
    for (def, canonical) in declared() {
        let ObjectDef::View(view) = def else {
            continue;
        };
        let expected = view_parts(&view.create_cql(KEYSPACE));
        let rendered = view_parts(&view_cql(KEYSPACE, &canonical));
        assert_eq!(rendered, expected, "{}", view.name);
        views += 1;
    }
    assert!(views >= 12, "only {views} views compared");
}

/// A table with no default TTL renders without a table option beyond its clustering order, and a
/// table with none of either renders no `WITH`
#[test]
fn options_appear_only_when_there_is_something_to_set() {
    // a table with a clustering column, and one with only a partition key
    let clustered = json!({
        "kind": "table", "name": "a",
        "columns": [{"name": "k", "type": "text"}, {"name": "c", "type": "int"}],
        "partition_key": ["k"],
        "clustering_key": [{"name": "c", "order": "desc"}],
        "static": [],
    });
    let flat = json!({
        "kind": "table", "name": "b",
        "columns": [{"name": "k", "type": "text"}],
        "partition_key": ["k"],
        "clustering_key": [],
        "static": [],
    });
    assert_eq!(
        table_cql(KEYSPACE, &clustered, None),
        format!(
            "CREATE TABLE IF NOT EXISTS {KEYSPACE}.a (k text, c int, PRIMARY KEY ((k), c)) \
             WITH CLUSTERING ORDER BY (c DESC)"
        )
    );
    assert_eq!(
        table_cql(KEYSPACE, &clustered, Some(30)),
        format!(
            "CREATE TABLE IF NOT EXISTS {KEYSPACE}.a (k text, c int, PRIMARY KEY ((k), c)) \
             WITH CLUSTERING ORDER BY (c DESC) AND default_time_to_live = 30"
        )
    );
    assert_eq!(
        table_cql(KEYSPACE, &flat, None),
        format!("CREATE TABLE IF NOT EXISTS {KEYSPACE}.b (k text, PRIMARY KEY ((k)))")
    );
    assert_eq!(
        table_cql(KEYSPACE, &flat, Some(5)),
        format!(
            "CREATE TABLE IF NOT EXISTS {KEYSPACE}.b (k text, PRIMARY KEY ((k))) \
             WITH default_time_to_live = 5"
        )
    );
}

/// A static column is declared `STATIC` and no other column is
#[test]
fn static_columns_are_marked() {
    // one static and one regular column under a clustering key
    let canonical = json!({
        "kind": "table", "name": "s",
        "columns": [
            {"name": "k", "type": "text"},
            {"name": "c", "type": "int"},
            {"name": "shared", "type": "text"},
            {"name": "v", "type": "set<text>"},
        ],
        "partition_key": ["k"],
        "clustering_key": [{"name": "c", "order": "asc"}],
        "static": ["shared"],
    });
    let cql = table_cql(KEYSPACE, &canonical, None);
    assert!(cql.contains("shared text STATIC"), "{cql}");
    assert!(cql.contains("v set<text>,"), "{cql}");
    assert!(
        !cql.contains("k text STATIC") && !cql.contains("v set<text> STATIC"),
        "{cql}"
    );
}

/// A view's filter replaces the `IS NOT NULL` of the column it restricts, and every other key
/// column keeps its own
#[test]
fn a_view_filter_replaces_its_columns_not_null() {
    // a view over a policy table that selects the default policies
    let canonical = json!({
        "kind": "view", "name": "v", "base": "t",
        "columns": [
            {"name": "default_policy", "type": "boolean"},
            {"name": "group", "type": "text"},
            {"name": "name", "type": "text"},
        ],
        "partition_key": ["group"],
        "clustering_key": [
            {"name": "name", "order": "asc"},
            {"name": "default_policy", "order": "asc"},
        ],
        "filter": "default_policy = true",
    });
    let cql = view_cql(KEYSPACE, &canonical);
    assert!(
        cql.contains("group IS NOT NULL AND name IS NOT NULL AND default_policy = true"),
        "{cql}"
    );
    assert!(!cql.contains("default_policy IS NOT NULL"), "{cql}");
    assert!(cql.contains(&format!("FROM {KEYSPACE}.t WHERE")), "{cql}");
    assert!(
        cql.ends_with("WITH CLUSTERING ORDER BY (name ASC, default_policy ASC)"),
        "{cql}"
    );
}
