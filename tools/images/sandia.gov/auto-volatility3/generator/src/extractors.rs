//! Extracts entities from results from volatility3

use std::path::Path;
use thorium::Error;

use crate::OsKinds;

mod windows;

/// Extract entities based on what OS kind this is
pub async fn entities<P: AsRef<Path>>(
    os_kinds: Vec<OsKinds>,
    name: &str,
    path: P,
) -> Result<(), Error> {
    // start with an empty vec of extracted entities
    let mut all_reqs = Vec::with_capacity(100);
    // extract each of the different os kinds entities
    for os_kind in os_kinds {
        // extract entities based on what OS this is from
        let mut entity_reqs = match os_kind {
            OsKinds::Windows => windows::extract_all(name, &path).await?,
            OsKinds::Linux | OsKinds::Mac => vec![],
        };
        // add this operating systems entities
        all_reqs.append(&mut entity_reqs);
    }
    // serialize our entities so we can write them to disk
    let serialized = serde_json::to_string(&all_reqs)?;
    // write our serialized entities to disk
    tokio::fs::write("/tmp/thorium/entities.json", serialized).await?;
    Ok(())
}
