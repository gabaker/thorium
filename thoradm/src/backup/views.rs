//! The materialized views a restore drops before loading their base tables
//!
//! Each table's declared views are dropped just before its rows are written, and `schema apply`,
//! which the restore runs once every table is loaded, creates them again from the schema and waits
//! for them to build. A view an interrupted restore left dropped comes back the same way, with no
//! saved statements: the schema is where views are defined.

use scylla::client::session::Session;
use std::collections::{BTreeMap, BTreeSet};
use thorium::models::schema::{HostStatus, cluster_hosts, member_build, view_build_statuses};
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

/// Returns whether every cluster member reports a view built
///
/// Each member writes its own status row, which is read at consistency one; a row left by a host
/// that is no longer a member (a removed or replaced node) is ignored, and a member that is down
/// and hasn't reported leaves the view unbuilt.
///
/// # Arguments
///
/// * `scylla` - The session to the target
/// * `keyspace` - The view's keyspace
/// * `view` - The view
pub async fn view_built(scylla: &Session, keyspace: &str, view: &str) -> Result<bool, Error> {
    // each node reports its own build status
    let statuses = view_build_statuses(scylla, keyspace, view)
        .await
        .map_err(|error| {
            Error::new(format!(
                "failed to read the build of {keyspace}.{view}: {error}"
            ))
        })?;
    // judge the rows of the cluster's current members alone
    Ok(members_built(&cluster_hosts(scylla), &statuses))
}

/// Returns whether every cluster member's own status row reports a view built, ignoring the rows
/// of hosts that are no longer members
///
/// # Arguments
///
/// * `hosts` - The cluster's members, by host id, each named by its address and host id
/// * `statuses` - The view's status rows, from `view_build_status`
#[must_use]
fn members_built(hosts: &BTreeMap<String, String>, statuses: &[HostStatus]) -> bool {
    // a session always knows one member at least, so with none the view can't be judged built
    !hosts.is_empty() && member_build(hosts, statuses).waiting.is_empty()
}

#[cfg(test)]
mod tests {
    use super::members_built;
    use std::collections::BTreeMap;
    use thorium::models::schema::HostStatus;

    /// Build a member list from host ids
    ///
    /// # Arguments
    ///
    /// * `ids` - The members' host ids
    fn hosts(ids: &[&str]) -> BTreeMap<String, String> {
        // name each member by a placeholder address and its host id
        ids.iter()
            .map(|id| ((*id).to_owned(), format!("10.0.0.1 ({id})")))
            .collect()
    }

    /// Build status rows from host ids and statuses
    ///
    /// # Arguments
    ///
    /// * `rows` - Each row's host id and status
    fn rows(rows: &[(&str, &str)]) -> Vec<HostStatus> {
        // keep each row's host and status
        rows.iter()
            .map(|(host_id, status)| HostStatus {
                host_id: (*host_id).to_owned(),
                status: (*status).to_owned(),
            })
            .collect()
    }

    /// A view every member built counts as built
    #[test]
    fn every_member_built_is_built() {
        // both members reported success
        let built = rows(&[("a", "SUCCESS"), ("b", "SUCCESS")]);
        assert!(members_built(&hosts(&["a", "b"]), &built));
    }

    /// A former member's unfinished row doesn't hold back a view every member built
    #[test]
    fn a_former_members_row_is_ignored() {
        // a removed node left a row from a build it never finished
        let stale = rows(&[("a", "SUCCESS"), ("b", "SUCCESS"), ("gone", "STARTED")]);
        assert!(members_built(&hosts(&["a", "b"]), &stale));
    }

    /// A former member's finished row doesn't stand in for a member that hasn't reported
    #[test]
    fn a_former_members_row_does_not_count() {
        // one member and a removed node reported success, the other member nothing
        let padded = rows(&[("a", "SUCCESS"), ("gone", "SUCCESS")]);
        assert!(!members_built(&hosts(&["a", "b"]), &padded));
    }

    /// A member still building, or down without a row, leaves the view unbuilt
    #[test]
    fn a_member_not_done_leaves_it_unbuilt() {
        // one member still building
        let building = rows(&[("a", "SUCCESS"), ("b", "STARTED")]);
        assert!(!members_built(&hosts(&["a", "b"]), &building));
        // one member down, with no row
        let silent = rows(&[("a", "SUCCESS")]);
        assert!(!members_built(&hosts(&["a", "b"]), &silent));
    }

    /// With no known members a view is never judged built
    #[test]
    fn no_members_is_not_built() {
        // rows but no members
        assert!(!members_built(&BTreeMap::new(), &rows(&[("a", "SUCCESS")])));
    }
}
