//! Some utilities to help thoradm do what it needs to do

use scylla::client::session::Session;
use scylla::errors::ExecutionError;
use scylla::response::query_result::QueryResult;

/// Drop a materialized view in scylla
///
/// # Arguments
///
/// * `name` - The name of the materialized view to delete
/// * `cluster` - The cluster we are deleting a materialized view from
/// * `scylla` - The scylla session to use
pub async fn drop_materialized_view(
    ns: &str,
    name: &str,
    scylla: &Session,
) -> Result<QueryResult, ExecutionError> {
    // Drop the target table
    let table_drop = format!(
        "drop materialized view if exists {ns}.{name}",
        ns = ns,
        name = name
    );
    // execute our query
    scylla.query_unpaged(table_drop, &[]).await
}

/// Returns whether a path is a symbolic link, without following it; a missing path isn't one
///
/// A backup holds only real files and directories, so a link inside one leads somewhere the
/// backup doesn't control and is never followed.
///
/// # Arguments
///
/// * `path` - The path to check
pub async fn is_link(path: &std::path::Path) -> std::io::Result<bool> {
    // read the path's own metadata, so a link reads as a link instead of what it points at
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => Ok(metadata.file_type().is_symlink()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Returns the first of some paths that is a symbolic link, if any is
///
/// # Arguments
///
/// * `paths` - The paths to check, in order
pub async fn first_link(paths: &[&std::path::Path]) -> std::io::Result<Option<std::path::PathBuf>> {
    // check each path without following it
    for path in paths {
        if is_link(path).await? {
            return Ok(Some(path.to_path_buf()));
        }
    }
    Ok(None)
}

/// The shared functions across all traits
pub trait Utils {
    /// The name of the table we are operating on
    fn name() -> &'static str;

    /// Get the pretty name of the table
    fn pretty_name() -> String {
        Self::name().replace('_', " ")
    }
}
