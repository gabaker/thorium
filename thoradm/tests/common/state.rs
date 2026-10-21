//! What a restore could change in a namespace, read so a refused restore can be shown to have
//! changed nothing

use super::harness::{BUCKET_ROLES, Harness, bucket_for, list_keys, redis_pool};
use std::collections::BTreeMap;
use thoradm::backup::coverage::{Registry, Store};

/// What a restore could change: row counts, the namespace's Redis keys, and bucket listings
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetState {
    /// Rows per backed-up table
    pub rows: BTreeMap<String, usize>,
    /// Every Redis key of the namespace, sorted
    pub redis_keys: Vec<String>,
    /// Every object key per bucket role, sorted
    pub objects: BTreeMap<String, Vec<String>>,
}

/// Reads everything a restore could change in the harness's namespace
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
///
/// # Panics
///
/// Panics if a store can't be read.
pub async fn target_state(harness: &Harness) -> TargetState {
    // count the rows of every table a backup holds
    let scylla = harness.scylla().await;
    let mut rows = BTreeMap::new();
    for entry in Registry::backed_up(Store::ScyllaTable) {
        let query = format!(
            "SELECT * FROM {}.{}",
            harness.conf.thorium.namespace, entry.name
        );
        let count = scylla
            .query_unpaged(query, &[])
            .await
            .unwrap()
            .into_rows_result()
            .unwrap()
            .rows_num();
        rows.insert(entry.name.to_owned(), count);
    }
    // list the namespace's Redis keys
    let pool = redis_pool(&harness.conf).await.unwrap();
    let mut conn = pool.get().await.unwrap();
    let mut redis_keys: Vec<String> = redis::cmd("KEYS")
        .arg(format!("{}:*", harness.conf.thorium.namespace))
        .query_async(&mut *conn)
        .await
        .unwrap();
    redis_keys.sort();
    // list every bucket
    let s3 = harness.s3();
    let mut objects = BTreeMap::new();
    for role in BUCKET_ROLES {
        let bucket = bucket_for(&harness.conf, role);
        let mut keys = list_keys(&s3, &bucket).await.unwrap();
        keys.sort();
        objects.insert((*role).to_owned(), keys);
    }
    TargetState {
        rows,
        redis_keys,
        objects,
    }
}
