//! The materialized views a restore drops before loading their base tables
//!
//! Each table's declared views are dropped just before its rows are written, and `schema apply`,
//! which the restore runs once every table is loaded, creates them again from the schema and waits
//! for them to build. A view an interrupted restore left dropped comes back the same way, with no
//! saved statements: the schema is where views are defined.

use scylla::client::session::Session;
use scylla::statement::Consistency;
use scylla::statement::unprepared::Statement;
use std::collections::BTreeSet;
use tracing::info;

use super::coverage::Registry;
use crate::Error;

/// Returns the names of the views a keyspace holds
///
/// # Arguments
///
/// * `scylla` - The session to ask
/// * `keyspace` - The keyspace
pub async fn existing_views(scylla: &Session, keyspace: &str) -> Result<BTreeSet<String>, Error> {
    // the schema lists every view of the keyspace
    let rows = scylla
        .query_unpaged(
            "SELECT view_name FROM system_schema.views WHERE keyspace_name = ?",
            (keyspace,),
        )
        .await?
        .into_rows_result()
        .map_err(|error| Error::new(format!("failed to read the views of {keyspace}: {error}")))?;
    let names = rows
        .rows::<(String,)>()
        .map_err(|error| Error::new(format!("failed to read the views of {keyspace}: {error}")))?
        .filter_map(Result::ok)
        .map(|(name,)| name)
        .collect();
    Ok(names)
}

/// Drops the declared views the registry lists for a table, before its rows are written
///
/// A view only keyspaces older releases created is left alone: it isn't declared, so a restore
/// never drops it, and Scylla keeps it in step with its base table while the rows load.
///
/// # Arguments
///
/// * `scylla` - The session to the target
/// * `keyspace` - The target's keyspace
/// * `table` - The table about to be restored
pub async fn drop_views_of(scylla: &Session, keyspace: &str, table: &str) -> Result<(), Error> {
    // exactly the declared views, each logged so the drop is visible
    for view in Registry::views_of(table).filter(|view| !Registry::is_legacy_view(view)) {
        super::utils::drop_materialized_view(keyspace, view, scylla)
            .await
            .map_err(|err| Error::new(format!("failed to drop view {keyspace}.{view}: {err}")))?;
        info!("Dropped view {keyspace}.{view}");
    }
    Ok(())
}

/// Returns whether every node reports a view built
///
/// Each node writes its own status row, so the table is read at consistency one and a view counts
/// as built once every node the session knows has reported success; the table's replication would
/// need several nodes for the default consistency.
///
/// # Arguments
///
/// * `scylla` - The session to the target
/// * `keyspace` - The view's keyspace
/// * `view` - The view
pub async fn view_built(scylla: &Session, keyspace: &str, view: &str) -> Result<bool, Error> {
    // each node reports its own build status
    let mut query = Statement::new(
        "SELECT status FROM system_distributed.view_build_status WHERE keyspace_name = ? AND view_name = ?",
    );
    // each node writes its own row, so one replica answers
    query.set_consistency(Consistency::One);
    let rows = scylla
        .query_unpaged(query, (keyspace, view))
        .await?
        .into_rows_result()
        .map_err(|error| {
            Error::new(format!(
                "failed to read the build of {keyspace}.{view}: {error}"
            ))
        })?;
    // every node's status
    let statuses: Vec<String> = rows
        .rows::<(String,)>()
        .map_err(|error| {
            Error::new(format!(
                "failed to read the build of {keyspace}.{view}: {error}"
            ))
        })?
        .filter_map(Result::ok)
        .map(|(status,)| status)
        .collect();
    // built once every known node reports success
    let nodes = scylla.get_cluster_state().get_nodes_info().len().max(1);
    Ok(statuses.len() >= nodes && statuses.iter().all(|status| status == "SUCCESS"))
}
