//! The one registry of every statement Thorium runs against Scylla
//!
//! The registry holds the API's queries structs (their census and backup statements included),
//! the search streamer's statements when the `search-streamer` feature is on, and each declared
//! table's full-row insert, token-range scan, and census, which thoradm backs up and restores
//! with. The statement validation and the prepare check both read it, so no consumer registers
//! statements of its own.

use scylla::client::session::Session;
use std::fmt;

use super::definition::{ScyllaObject, TableStatements};
use super::queries::ScyllaQueries;
use super::scylla::{
    AssociationsTable, CommentsTable, CommitishListTable, CommitishesTable, EntitiesTable,
    LogsTable, NetworkPoliciesTable, NodesTable, NotificationsTable, RepoDataTable, ReposListTable,
    ResultsStreamTable, ResultsTable, S3IdsTable, SamplesListTable, TagsTable,
};

/// One statement in the registry
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredStatement {
    /// The statement's name, such as `samples_list.insert` or `search.results.results`
    pub name: String,
    /// The declared object the statement targets
    pub object: String,
    /// The statement's text for one keyspace
    pub cql: String,
}

/// Register the statements of one source, named `<object>.<statement>` after a prefix
///
/// # Arguments
///
/// * `prefix` - What the source's names start with before the object, such as `search.`
/// * `statements` - The source's statements, named `<object>.<statement>` after the prefix
fn register(prefix: &str, statements: Vec<(String, String)>) -> Vec<RegisteredStatement> {
    // pair each statement with the object its name starts with
    statements
        .into_iter()
        .map(|(name, cql)| {
            // the object is the part of the name between the prefix and the statement
            let object = name
                .strip_prefix(prefix)
                .and_then(|rest| rest.split_once('.'))
                .map_or_else(String::new, |(object, _)| object.to_owned());
            RegisteredStatement { name, object, cql }
        })
        .collect()
}

/// The full-row statements of one table that consumers run: the insert restores write with,
/// the token-range scan backups read with, and the per-partition census
///
/// # Arguments
///
/// * `ns` - The keyspace the statements target
fn full_row<T: TableStatements>(ns: &str) -> Vec<RegisteredStatement> {
    // name each statement after its table and its constant
    let object = <T as ScyllaObject>::NAME;
    [
        ("INSERT", T::INSERT),
        ("SCAN", T::SCAN),
        ("CENSUS", T::CENSUS),
    ]
    .into_iter()
    .map(|(kind, cql)| RegisteredStatement {
        name: format!("{object}.{kind}"),
        object: object.to_owned(),
        cql: super::with_keyspace(cql, ns),
    })
    .collect()
}

/// Every registered statement for a keyspace: each declared table's full-row statements, then
/// the API's queries structs, then the search streamer's statements
///
/// # Arguments
///
/// * `ns` - The keyspace the statements target
#[must_use]
pub fn registry(ns: &str) -> Vec<RegisteredStatement> {
    // each declared table's full-row statements, in declaration order
    let mut statements = [
        full_row::<SamplesListTable>(ns),
        full_row::<S3IdsTable>(ns),
        full_row::<CommentsTable>(ns),
        full_row::<ReposListTable>(ns),
        full_row::<RepoDataTable>(ns),
        full_row::<CommitishesTable>(ns),
        full_row::<CommitishListTable>(ns),
        full_row::<ResultsTable>(ns),
        full_row::<ResultsStreamTable>(ns),
        full_row::<TagsTable>(ns),
        full_row::<EntitiesTable>(ns),
        full_row::<AssociationsTable>(ns),
        full_row::<NetworkPoliciesTable>(ns),
        full_row::<NodesTable>(ns),
        full_row::<NotificationsTable>(ns),
        full_row::<LogsTable>(ns),
    ]
    .concat();
    // the API's statements, including its census and backup statements
    statements.extend(register("", ScyllaQueries::statements(ns)));
    // the search streamer's statements, compiled in only for the streamer
    #[cfg(feature = "search-streamer")]
    statements.extend(register("search.", super::queries::search::statements(ns)));
    statements
}

/// The registered statements that failed to prepare in a keyspace
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepareCheckError {
    /// The keyspace the statements were prepared in
    pub keyspace: String,
    /// Each failed statement's name with the error Scylla returned
    pub failures: Vec<(String, String)>,
}

impl fmt::Display for PrepareCheckError {
    /// List every statement that failed to prepare with its error
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // how many statements failed, and where
        write!(
            f,
            "{} registered statements don't prepare in keyspace {}",
            self.failures.len(),
            self.keyspace
        )?;
        // then each failed statement with its error
        for (name, error) in &self.failures {
            write!(f, "\n  {name}: {error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for PrepareCheckError {}

/// Prepare every registered statement in a keyspace, failing with each one that doesn't prepare
///
/// Returns how many statements prepared.
///
/// # Arguments
///
/// * `session` - The Scylla session to prepare on
/// * `ns` - The keyspace the statements target
pub async fn prepare_all(session: &Session, ns: &str) -> Result<usize, PrepareCheckError> {
    // prepare each statement in turn, keeping every failure so all of them are reported at once
    let statements = registry(ns);
    let mut failures = Vec::new();
    for statement in &statements {
        if let Err(err) = session.prepare(statement.cql.as_str()).await {
            failures.push((statement.name.clone(), err.to_string()));
        }
    }
    if failures.is_empty() {
        Ok(statements.len())
    } else {
        Err(PrepareCheckError {
            keyspace: ns.to_owned(),
            failures,
        })
    }
}
