//! The statements on the migration-state tables, which thoradm writes and the API's startup check
//! and thoradm read
//!
//! They are prepared on their own, apart from [`super::ScyllaQueries`], because thoradm prepares
//! them in a keyspace it has just created or converged, before any API statement can prepare.

use scylla::client::session::Session;
use scylla::errors::PrepareError;

use crate::models::schema::scylla::{
    SchemaMigrationIssueRow, SchemaMigrationIssuesTable, SchemaMigrationRow, SchemaMigrationsTable,
};

/// Statements on the `schema_migrations` table
#[thorium::table_queries(table = SchemaMigrationsTable)]
pub struct SchemaMigrationsQueries {
    /// Insert or overwrite one record row that never expires
    #[insert(columns = all, ttl = none)]
    insert: Insert,
    /// Insert or overwrite one record row that expires after a bound number of seconds, such as a
    /// Job's result
    #[insert(columns = all, ttl = bind)]
    insert_expiring: Insert,
    /// One record row by its kind and id
    #[select(project = SchemaMigrationRow, eq(kind, id))]
    get: Select,
    /// Every row of one record kind, oldest id first
    #[select(project = SchemaMigrationRow, eq(kind))]
    list: Select,
    /// Delete one record row
    #[delete(by_key, eq(kind, id))]
    delete: Delete,
    /// Delete every row of one record kind, such as the history a restore replaces
    #[delete(partition, eq(kind))]
    delete_kind: Delete,
}

/// Statements on the `schema_migration_issues` table
#[thorium::table_queries(table = SchemaMigrationIssuesTable)]
pub struct SchemaMigrationIssuesQueries {
    /// Insert or overwrite one worker or issue row
    #[insert(columns = all, ttl = none)]
    insert: Insert,
    /// Every worker and issue row of one pass
    #[select(project = SchemaMigrationIssueRow, eq(migration))]
    list: Select,
    /// Every worker's row of one issue group
    #[select(project = SchemaMigrationIssueRow, eq(migration, transform, reason))]
    get_group: Select,
    /// Record a worker's checkpoint and lease token for an attempt
    #[update(
        set(attempt, checkpoint, lease, updated),
        by_key,
        eq(migration, transform, reason, worker)
    )]
    update_worker: Update,
    /// Record what a worker counted in an issue group, leaving its resolved count and
    /// acceptance alone
    #[update(
        set(attempt, class, count, samples, first_seen, last_seen, updated),
        by_key,
        eq(migration, transform, reason, worker)
    )]
    update_issue: Update,
    /// Record the objects a later run took out of an issue group
    #[update(
        set(count, resolved_count, updated),
        by_key,
        eq(migration, transform, reason, worker)
    )]
    resolve: Update,
    /// Record whether an approval accepted an issue group's code
    #[update(
        set(accepted, updated),
        by_key,
        eq(migration, transform, reason, worker)
    )]
    accept: Update,
    /// Delete every worker and issue row of one pass
    #[delete(partition, eq(migration))]
    delete_pass: Delete,
}

/// The statements on both migration-state tables, prepared against one keyspace
pub struct SchemaStateQueries {
    /// Statements on the `schema_migrations` table
    pub schema_migrations: SchemaMigrationsQueries,
    /// Statements on the `schema_migration_issues` table
    pub schema_migration_issues: SchemaMigrationIssuesQueries,
}

impl SchemaStateQueries {
    /// Prepare the statements on both migration-state tables
    ///
    /// # Arguments
    ///
    /// * `session` - The Scylla session to prepare on
    /// * `ns` - The keyspace the statements target
    // the driver's own error is returned as is, so callers convert it the way they already do
    #[allow(clippy::result_large_err)]
    pub async fn prepare(session: &Session, ns: &str) -> Result<Self, PrepareError> {
        // prepare each table's statements in turn
        let schema_migrations = SchemaMigrationsQueries::prepare(session, ns).await?;
        let schema_migration_issues = SchemaMigrationIssuesQueries::prepare(session, ns).await?;
        Ok(SchemaStateQueries {
            schema_migrations,
            schema_migration_issues,
        })
    }

    /// Every statement on the migration-state tables, named `<table>.<statement>`, in
    /// declaration order
    ///
    /// # Arguments
    ///
    /// * `ns` - The keyspace the statements target
    #[must_use]
    pub fn statements(ns: &str) -> Vec<(String, String)> {
        // list each table's statements under its name
        let groups = [
            ("schema_migrations", SchemaMigrationsQueries::statements(ns)),
            (
                "schema_migration_issues",
                SchemaMigrationIssuesQueries::statements(ns),
            ),
        ];
        groups
            .into_iter()
            .flat_map(|(object, statements)| {
                statements
                    .into_iter()
                    .map(move |(name, cql)| (format!("{object}.{name}"), cql))
            })
            .collect()
    }
}
