//! Tests of the migration-state tables' declarations and the statements registered on them

use super::statements::registry;
use super::{Keyspace, ObjectDef, scylla::ThoriumKeyspace};

/// The migration-state tables are the first declared objects, so every creation in declaration
/// order makes `schema_migrations` before anything it records
#[test]
fn state_tables_are_declared_first() {
    // the first two objects are the two state tables, both base tables
    let objects = <ThoriumKeyspace as Keyspace>::OBJECTS;
    let first = objects
        .iter()
        .take(2)
        .map(ObjectDef::name)
        .collect::<Vec<_>>();
    assert_eq!(first, ["schema_migrations", "schema_migration_issues"]);
    assert!(
        objects
            .iter()
            .take(2)
            .all(|object| matches!(object, ObjectDef::Table(_)))
    );
}

/// `schema_migrations` is partitioned by record kind and clustered by id, with every column the
/// record kinds use
#[test]
fn schema_migrations_has_its_declared_columns() {
    // render the table's statement and compare it with the design's columns and key
    let object = <ThoriumKeyspace as Keyspace>::OBJECTS
        .iter()
        .find(|object| object.name() == "schema_migrations")
        .expect("schema_migrations is declared");
    assert_eq!(
        object.create_cql("{ns}", None),
        "CREATE TABLE IF NOT EXISTS {ns}.schema_migrations (kind text, id text, fingerprint text, \
         apply_key text, layouts map<text, bigint>, revision text, version text, entry text, \
         result text, value text, status text, attempt text, lease text, started timestamp, \
         finished timestamp, job text, backup text, counts text, report text, updated timestamp, \
         PRIMARY KEY ((kind), id)) \
         WITH CLUSTERING ORDER BY (id ASC)"
    );
}

/// `schema_migration_issues` is partitioned by pass and clustered by transform, reason, and
/// worker, with the worker and issue columns
#[test]
fn schema_migration_issues_has_its_declared_columns() {
    // render the table's statement and compare it with the design's columns and key
    let object = <ThoriumKeyspace as Keyspace>::OBJECTS
        .iter()
        .find(|object| object.name() == "schema_migration_issues")
        .expect("schema_migration_issues is declared");
    assert_eq!(
        object.create_cql("{ns}", None),
        "CREATE TABLE IF NOT EXISTS {ns}.schema_migration_issues (migration text, transform text, \
         reason text, worker text, attempt text, checkpoint text, lease text, class text, \
         count bigint, first_seen timestamp, last_seen timestamp, samples set<text>, \
         resolved_count bigint, accepted boolean, updated timestamp, PRIMARY KEY ((migration), \
         transform, reason, worker)) WITH CLUSTERING ORDER BY (transform ASC, reason ASC, \
         worker ASC)"
    );
}

/// Every statement the record's writers and readers run is in the one statement registry, so the
/// validation and the prepare check cover them
#[test]
fn state_statements_are_registered() {
    // collect the registered names of the statements on the two state tables
    let names = registry("{ns}")
        .into_iter()
        .filter(|statement| statement.object.starts_with("schema_migration"))
        .map(|statement| statement.name)
        .collect::<Vec<_>>();
    // the full-row statements thoradm backs up with, then the declared statements, in order
    assert_eq!(
        names,
        [
            "schema_migrations.INSERT",
            "schema_migrations.SCAN",
            "schema_migrations.CENSUS",
            "schema_migration_issues.INSERT",
            "schema_migration_issues.SCAN",
            "schema_migration_issues.CENSUS",
            "schema_migrations.insert",
            "schema_migrations.insert_expiring",
            "schema_migrations.get",
            "schema_migrations.list",
            "schema_migrations.delete",
            "schema_migrations.delete_kind",
            "schema_migration_issues.insert",
            "schema_migration_issues.list",
            "schema_migration_issues.get_group",
            "schema_migration_issues.update_worker",
            "schema_migration_issues.update_issue",
            "schema_migration_issues.resolve",
            "schema_migration_issues.accept",
            "schema_migration_issues.delete_pass",
        ]
    );
}

/// Every record kind has its own stored text, which parses back to it; the legacy-drop kind is
/// stored as `legacy_drop`, and text no kind owns is an error rather than a catch-all kind
#[test]
fn every_record_kind_has_its_stored_text() {
    use super::MigrationRecordKind;
    use std::collections::BTreeSet;
    // the kinds a reader knows, and the text each is stored as
    let texts = MigrationRecordKind::ALL
        .iter()
        .map(MigrationRecordKind::as_str)
        .collect::<Vec<_>>();
    assert_eq!(
        texts,
        [
            "backend",
            "run",
            "revision",
            "history",
            "config",
            "consumed_code",
            "archived_history",
            "job_result",
            "legacy_drop"
        ]
    );
    assert_eq!(texts.iter().collect::<BTreeSet<_>>().len(), texts.len());
    // each parses back to itself
    for kind in MigrationRecordKind::ALL {
        assert_eq!(
            kind.as_str().parse::<MigrationRecordKind>().ok(),
            Some(kind)
        );
    }
    // there is no catch-all: text no kind owns is an error a scanning reader must tolerate
    assert!("legacy_drops".parse::<MigrationRecordKind>().is_err());
    assert!("".parse::<MigrationRecordKind>().is_err());
}
