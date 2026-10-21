//! The Scylla objects older releases created and newer ones don't, built in a test namespace as
//! a keyspace from an older release holds them

use super::harness::Harness;

/// The events table as older releases created it
const OLD_EVENTS: &str = "CREATE TABLE IF NOT EXISTS {ns}.events (\
    event_type TEXT, \
    year INT, \
    bucket INT, \
    timestamp TIMESTAMP, \
    id UUID, \
    parent UUID, \
    user TEXT, \
    trigger_depth SMALLINT, \
    data TEXT, \
    PRIMARY KEY ((event_type, year, bucket), timestamp, id))
    WITH default_time_to_live = {ttl}";

/// The entities by name view as older releases created it
const OLD_ENTITIES_BY_NAME: &str = "CREATE MATERIALIZED VIEW IF NOT EXISTS {ns}.entities_by_name AS \
    SELECT name, group, kind, created, year, bucket, id, submitter, kind_data, description, image FROM {ns}.entities \
    WHERE name IS NOT NULL \
    AND group IS NOT NULL \
    AND kind IS NOT NULL \
    AND created IS NOT NULL \
    AND year IS NOT NULL \
    AND bucket IS NOT NULL \
    AND id IS NOT NULL
    PRIMARY KEY (name, group, kind, created, year, bucket, id)";

/// Creates the events table in the harness's namespace, as an older release left it
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
pub async fn create_events(harness: &Harness) {
    // the table's default TTL comes from the config, as older releases set it
    let statement = OLD_EVENTS
        .replace("{ns}", &harness.conf.thorium.namespace)
        .replace("{ttl}", &harness.conf.thorium.events.retention.to_string());
    harness
        .scylla()
        .await
        .query_unpaged(statement, &[])
        .await
        .expect("failed to create the events table");
}

/// Creates the entities by name view in the harness's namespace, as an older release left it
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
pub async fn create_entities_by_name(harness: &Harness) {
    // the view selects from the entities table the API's setup created
    let statement = OLD_ENTITIES_BY_NAME.replace("{ns}", &harness.conf.thorium.namespace);
    harness
        .scylla()
        .await
        .query_unpaged(statement, &[])
        .await
        .expect("failed to create the entities_by_name view");
}
