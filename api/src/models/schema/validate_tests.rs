//! Tests of the static statement validation, over the registry and over broken fixtures

use std::collections::BTreeSet;

use super::queries::ScyllaQueries;
use super::scylla::ThoriumKeyspace;
use super::statements::{RegisteredStatement, registry};
use super::tests::{FixtureCountsTable, FixtureSamplesTable};
use super::validate::{Rule, validate_statement, validate_statements};
use super::{Keyspace, ObjectDef, ScyllaObject, TableStatements, with_keyspace};

/// Fixture queries structs whose hand-written statements break the validation rules
// the fixtures' prepared statements and methods are generated but never run
#[allow(dead_code)]
mod fixtures {
    use chrono::{DateTime, Utc};
    use uuid::Uuid;

    /// Conditional statements on the fixture submissions table
    #[thorium::table_queries(table = crate::models::schema::tests::FixtureSamplesTable)]
    pub struct FixtureConditionalQueries {
        /// Rename a submission only while it still has its old name
        #[query(
            "UPDATE {ns}.fixture_samples SET name = ? WHERE group = ? AND year = ? AND bucket = ? \
             AND uploaded = ? AND id = ? IF name = ?",
            binds(
                name: &str,
                group: &str,
                year: i32,
                bucket: i32,
                uploaded: DateTime<Utc>,
                id: Uuid,
                old: &str
            )
        )]
        rename_if: Raw,
        /// Insert a submission only when it doesn't exist yet
        #[query(
            "INSERT INTO {ns}.fixture_samples (group, year, bucket, uploaded, id, sha256, sha1, \
             md5, submitter) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) IF NOT EXISTS",
            binds(
                group: &str,
                year: i32,
                bucket: i32,
                uploaded: DateTime<Utc>,
                id: Uuid,
                sha256: &str,
                sha1: &str,
                md5: &str,
                submitter: &str
            )
        )]
        insert_if_absent: Raw,
    }

    /// A counter update on the fixture counts table
    #[thorium::table_queries(table = crate::models::schema::tests::FixtureCountsTable)]
    pub struct FixtureCounterQueries {
        /// Add one to a total
        #[query(
            "UPDATE {ns}.fixture_counts SET total = total + 1 WHERE id = ?",
            binds(id: Uuid)
        )]
        increment: Raw,
    }
}

/// The objects the fixture statements are checked against
const FIXTURE_OBJECTS: [ObjectDef; 2] = [
    <FixtureSamplesTable as ScyllaObject>::DEF,
    <FixtureCountsTable as ScyllaObject>::DEF,
];

/// Build a registered fixture statement
///
/// # Arguments
///
/// * `name` - The statement's registered name
/// * `object` - The object it's registered with
/// * `cql` - Its text
fn fixture(name: &str, object: &str, cql: &str) -> RegisteredStatement {
    // fixtures keep the keyspace placeholder the registry is validated with
    RegisteredStatement {
        name: name.to_owned(),
        object: object.to_owned(),
        cql: with_keyspace(cql, "{ns}"),
    }
}

/// Register the statements of a fixture queries struct under its object
///
/// # Arguments
///
/// * `object` - The object the struct targets
/// * `statements` - The struct's statements
fn fixture_queries(object: &str, statements: Vec<(&str, String)>) -> Vec<RegisteredStatement> {
    // name each statement `<object>.<field>`, as the registry does
    statements
        .into_iter()
        .map(|(name, cql)| fixture(&format!("{object}.{name}"), object, &cql))
        .collect()
}

/// Every registered statement validates against the declared schema, with no exceptions, and
/// the registry holds every source's statements once
#[test]
fn every_registered_statement_validates() {
    // the whole registry, with the keyspace placeholder for its keyspace
    let statements = registry("{ns}");
    // every statement passes every rule
    let errors = validate_statements(&statements, <ThoriumKeyspace as Keyspace>::OBJECTS);
    assert!(
        errors.is_empty(),
        "{} registered statements break a rule:\n{}",
        errors.len(),
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    );
    // no two statements share a name
    let names = statements
        .iter()
        .map(|statement| statement.name.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(names.len(), statements.len());
    // every declared table's full-row insert, scan, and census are registered
    for object in <ThoriumKeyspace as Keyspace>::OBJECTS {
        if let ObjectDef::Table(table) = object {
            for kind in ["INSERT", "SCAN", "CENSUS"] {
                let name = format!("{}.{kind}", table.name);
                assert!(names.contains(name.as_str()), "{name} isn't registered");
            }
        }
    }
    // the census and backup statements are registered too
    for name in [
        "samples_list.census",
        "tags.backup",
        "notifications.backup",
        "notifications.restore",
    ] {
        assert!(names.contains(name), "{name} isn't registered");
    }
    // the search streamer's statements are registered when the streamer is compiled in
    #[cfg(feature = "search-streamer")]
    for name in ["search.results_ids.enumerate", "search.tags_by_item.event"] {
        assert!(names.contains(name), "{name} isn't registered");
    }
}

/// Every statement the API's queries structs declare, and the search streamer's when it's
/// compiled in, is in the registry with its own text
#[test]
fn every_source_statement_is_registered() {
    // the registry's statements by name and text
    let registered = registry("{ns}")
        .into_iter()
        .map(|statement| (statement.name, statement.cql))
        .collect::<BTreeSet<_>>();
    // the API's statements, with their census and backup statements
    #[cfg_attr(not(feature = "search-streamer"), allow(unused_mut))]
    let mut sources = ScyllaQueries::statements("{ns}");
    // the search streamer's statements, already named under `search.`
    #[cfg(feature = "search-streamer")]
    sources.extend(super::queries::search::statements("{ns}"));
    // each source statement is registered, so none is left out of validation and preparing
    let missing = sources
        .iter()
        .filter(|statement| !registered.contains(*statement))
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>();
    assert!(missing.is_empty(), "not registered: {}", missing.join(", "));
}

/// The fixture tables' generated statements and well-formed hand-written statements validate
#[test]
fn well_formed_fixture_statements_validate() {
    // the generated full-row statements of both fixture tables
    let mut statements = Vec::new();
    for (object, cqls) in [
        (
            "fixture_samples",
            [
                <FixtureSamplesTable as TableStatements>::INSERT,
                <FixtureSamplesTable as TableStatements>::SCAN,
                <FixtureSamplesTable as TableStatements>::CENSUS,
            ],
        ),
        (
            "fixture_counts",
            [
                <FixtureCountsTable as TableStatements>::INSERT,
                <FixtureCountsTable as TableStatements>::SCAN,
                <FixtureCountsTable as TableStatements>::CENSUS,
            ],
        ),
    ] {
        for (index, cql) in cqls.iter().enumerate() {
            statements.push(fixture(&format!("{object}.{index}"), object, cql));
        }
    }
    // tie reads in the declared order and in the reverse order, a bounded range, an element
    // update of a map, and a collection append
    statements.extend([
        fixture(
            "fixture_samples.ties",
            "fixture_samples",
            "SELECT id FROM {ns}.fixture_samples WHERE group = ? AND year = ? AND bucket = ? AND \
             uploaded = ? AND id <= ? LIMIT ?",
        ),
        fixture(
            "fixture_samples.ties_reversed",
            "fixture_samples",
            "SELECT id FROM {ns}.fixture_samples WHERE group = ? AND year = ? AND bucket = ? AND \
             uploaded = ? AND id >= ? ORDER BY uploaded ASC, id ASC LIMIT ?",
        ),
        fixture(
            "fixture_samples.between",
            "fixture_samples",
            "SELECT id FROM {ns}.fixture_samples WHERE group = ? AND year = ? AND bucket IN ? AND \
             uploaded < ? AND uploaded > ? PER PARTITION LIMIT ?",
        ),
        fixture(
            "fixture_counts.set_kind",
            "fixture_counts",
            "UPDATE {ns}.fixture_counts SET per_kind[?] = ? WHERE id = ?",
        ),
        fixture(
            "fixture_counts.add_kinds",
            "fixture_counts",
            "UPDATE {ns}.fixture_counts SET per_kind = per_kind + ? WHERE id = ?",
        ),
    ]);
    // a string literal spelling a keyword, and a statement written in capitals, whose unquoted
    // names Scylla reads in lower case
    statements.extend([
        fixture(
            "fixture_samples.literal_if",
            "fixture_samples",
            "UPDATE {ns}.fixture_samples SET name = 'if' WHERE group = ? AND year = ? AND \
             bucket = ? AND uploaded = ? AND id = ?",
        ),
        fixture(
            "fixture_samples.capitals",
            "fixture_samples",
            "SELECT ID, SHA256 FROM {ns}.FIXTURE_SAMPLES WHERE GROUP = ? AND YEAR = ? AND \
             BUCKET = ? AND UPLOADED = ? AND ID <= ? LIMIT ?",
        ),
    ]);
    // every well-formed statement passes every rule
    let errors = validate_statements(&statements, &FIXTURE_OBJECTS);
    assert!(errors.is_empty(), "{errors:?}");
}

/// Each broken fixture statement is rejected with its name and the rule it breaks, including
/// conditional and counter statements declared through `#[query(...)]`
#[test]
// the test lists every broken fixture statement
#[allow(clippy::too_many_lines)]
fn broken_statements_are_rejected_with_their_rule() {
    use fixtures::{FixtureConditionalQueries, FixtureCounterQueries};
    // hand-written statements that each break one rule
    let mut cases = vec![
        (
            fixture(
                "fixture_samples.wrong_column",
                "fixture_samples",
                "SELECT sha512 FROM {ns}.fixture_samples WHERE group = ? AND year = ? AND bucket = ?",
            ),
            Rule::UnknownColumn,
        ),
        (
            fixture(
                "fixture_samples.wrong_where_column",
                "fixture_samples",
                "DELETE FROM {ns}.fixture_samples WHERE group = ? AND year = ? AND bucket = ? AND \
                 uploaded_at = ?",
            ),
            Rule::UnknownColumn,
        ),
        (
            fixture(
                "fixture_missing.get",
                "fixture_missing",
                "SELECT id FROM {ns}.fixture_missing WHERE id = ?",
            ),
            Rule::UnknownObject,
        ),
        (
            fixture(
                "fixture_samples.registered_elsewhere",
                "fixture_samples",
                "SELECT total FROM {ns}.fixture_counts WHERE id = ?",
            ),
            Rule::WrongObject,
        ),
        (
            fixture(
                "fixture_samples.missing_bucket",
                "fixture_samples",
                "SELECT id FROM {ns}.fixture_samples WHERE group = ? AND year = ?",
            ),
            Rule::PartitionKey,
        ),
        (
            fixture(
                "fixture_samples.unrestricted",
                "fixture_samples",
                "SELECT id FROM {ns}.fixture_samples",
            ),
            Rule::PartitionKey,
        ),
        (
            fixture(
                "fixture_samples.partial_token",
                "fixture_samples",
                "SELECT id FROM {ns}.fixture_samples WHERE token(group, year) >= ? AND \
                 token(group, year) <= ?",
            ),
            Rule::PartitionKey,
        ),
        (
            fixture(
                "fixture_samples.by_sha256",
                "fixture_samples",
                "SELECT id FROM {ns}.fixture_samples WHERE group = ? AND year = ? AND bucket = ? \
                 AND sha256 = ?",
            ),
            Rule::NonKeyRestriction,
        ),
        (
            fixture(
                "fixture_samples.year_range",
                "fixture_samples",
                "SELECT id FROM {ns}.fixture_samples WHERE group = ? AND year > ? AND bucket = ?",
            ),
            Rule::RangeColumn,
        ),
        (
            fixture(
                "fixture_samples.ties_upward",
                "fixture_samples",
                "SELECT id FROM {ns}.fixture_samples WHERE group = ? AND year = ? AND bucket = ? AND \
                 uploaded = ? AND id >= ? LIMIT ?",
            ),
            Rule::ClusteringOrder,
        ),
        (
            fixture(
                "fixture_samples.ties_reversed_downward",
                "fixture_samples",
                "SELECT id FROM {ns}.fixture_samples WHERE group = ? AND year = ? AND bucket = ? AND \
                 uploaded = ? AND id <= ? ORDER BY uploaded ASC, id ASC LIMIT ?",
            ),
            Rule::ClusteringOrder,
        ),
        (
            fixture(
                "fixture_samples.update_if",
                "fixture_samples",
                "UPDATE {ns}.fixture_samples SET name = ? WHERE group = ? AND year = ? AND \
                 bucket = ? AND uploaded = ? AND id = ? IF EXISTS",
            ),
            Rule::Conditional,
        ),
        (
            fixture(
                "fixture_samples.insert_if_not_exists",
                "fixture_samples",
                "INSERT INTO {ns}.fixture_samples (group, year, bucket, uploaded, id) VALUES (?, \
                 ?, ?, ?, ?) IF NOT EXISTS",
            ),
            Rule::Conditional,
        ),
        (
            fixture(
                "fixture_samples.delete_if",
                "fixture_samples",
                "DELETE FROM {ns}.fixture_samples WHERE group = ? AND year = ? AND bucket = ? IF \
                 name = ?",
            ),
            Rule::Conditional,
        ),
        (
            fixture(
                "fixture_samples.lowercase_if",
                "fixture_samples",
                "UPDATE {ns}.fixture_samples SET name = ? WHERE group = ? AND year = ? AND \
                 bucket = ? AND uploaded = ? AND id = ? if exists",
            ),
            Rule::Conditional,
        ),
        (
            fixture(
                "fixture_samples.order_by_unknown",
                "fixture_samples",
                "SELECT id FROM {ns}.fixture_samples WHERE group = ? AND year = ? AND bucket = ? \
                 ORDER BY uploaded_at DESC",
            ),
            Rule::UnknownColumn,
        ),
        (
            fixture(
                "fixture_samples.group_by_unknown",
                "fixture_samples",
                "SELECT id FROM {ns}.fixture_samples WHERE group = ? AND year = ? AND bucket = ? \
                 GROUP BY uploaded_at",
            ),
            Rule::UnknownColumn,
        ),
        (
            fixture(
                "fixture_samples.writetime_unknown",
                "fixture_samples",
                "SELECT writetime(sha512) FROM {ns}.fixture_samples WHERE group = ? AND year = ? \
                 AND bucket = ?",
            ),
            Rule::UnknownColumn,
        ),
        (
            fixture(
                "fixture_samples.quoted_capitals",
                "fixture_samples",
                "SELECT \"Id\" FROM {ns}.fixture_samples WHERE group = ? AND year = ? AND bucket = ?",
            ),
            Rule::UnknownColumn,
        ),
        (
            fixture(
                "fixture_samples.delete_by_token",
                "fixture_samples",
                "DELETE FROM {ns}.fixture_samples WHERE token(group, year, bucket) >= ?",
            ),
            Rule::PartitionKey,
        ),
        (
            fixture(
                "fixture_samples.token_out_of_order",
                "fixture_samples",
                "SELECT id FROM {ns}.fixture_samples WHERE token(year, group, bucket) >= ? AND \
                 token(year, group, bucket) <= ?",
            ),
            Rule::PartitionKey,
        ),
        (
            fixture(
                "fixture_counts.total_from_itself",
                "fixture_counts",
                "UPDATE {ns}.fixture_counts SET total = 1 + total WHERE id = ?",
            ),
            Rule::Counter,
        ),
        (
            fixture(
                "fixture_samples.name_element",
                "fixture_samples",
                "UPDATE {ns}.fixture_samples SET name[?] = ? WHERE group = ? AND year = ? AND \
                 bucket = ? AND uploaded = ? AND id = ?",
            ),
            Rule::Syntax,
        ),
        (
            fixture(
                "fixture_samples.extra_value",
                "fixture_samples",
                "INSERT INTO {ns}.fixture_samples (group, year) VALUES (?, ?, ?)",
            ),
            Rule::Syntax,
        ),
        (
            fixture(
                "fixture_samples.allow_filtering",
                "fixture_samples",
                "SELECT id FROM {ns}.fixture_samples WHERE group = ? AND year = ? AND bucket = ? \
                 ALLOW FILTERING",
            ),
            Rule::Syntax,
        ),
        (
            fixture(
                "fixture_samples.truncate",
                "fixture_samples",
                "TRUNCATE {ns}.fixture_samples",
            ),
            Rule::Syntax,
        ),
    ];
    // the same rules reject statements written through `#[query(...)]`
    let conditional = fixture_queries(
        "fixture_samples",
        FixtureConditionalQueries::statements("{ns}"),
    );
    let counter = fixture_queries("fixture_counts", FixtureCounterQueries::statements("{ns}"));
    assert_eq!((conditional.len(), counter.len()), (2, 1));
    cases.extend(
        conditional
            .into_iter()
            .map(|statement| (statement, Rule::Conditional)),
    );
    cases.extend(
        counter
            .into_iter()
            .map(|statement| (statement, Rule::Counter)),
    );
    // each case fails with its own name and the expected rule, both in its message
    for (statement, rule) in &cases {
        let err = validate_statement(statement, &FIXTURE_OBJECTS)
            .expect_err(&format!("{} validated", statement.name));
        assert_eq!(
            (err.statement.as_str(), err.rule),
            (statement.name.as_str(), *rule),
            "{err}"
        );
        let message = err.to_string();
        assert!(
            message.contains(&statement.name) && message.contains(rule.name()),
            "{message}"
        );
    }
    // every rule has a broken fixture
    for rule in [
        Rule::Syntax,
        Rule::UnknownObject,
        Rule::WrongObject,
        Rule::UnknownColumn,
        Rule::PartitionKey,
        Rule::NonKeyRestriction,
        Rule::RangeColumn,
        Rule::ClusteringOrder,
        Rule::Conditional,
        Rule::Counter,
    ] {
        assert!(
            cases.iter().any(|(_, expected)| *expected == rule),
            "no fixture breaks {}",
            rule.name()
        );
    }
}
