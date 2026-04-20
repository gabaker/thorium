//! Extracts entities from results for a windows memory dump from volatility3

use std::path::Path;
use thorium::{Error, models::EntityRequest};

mod processes;

/// Extract all possible entities from a windows memory dump
pub async fn extract_all<P: AsRef<Path>>(name: &str, path: P) -> Result<Vec<EntityRequest>, Error> {
    // get the root path to our results
    let root = path.as_ref();
    // extract windows processes
    let reqs = processes::extract(name, root).await?;
    Ok(reqs)
}
