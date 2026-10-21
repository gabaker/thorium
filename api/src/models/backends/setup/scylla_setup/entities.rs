//! Setup the entities by name materialized view in Scylla

use scylla::client::session::Session;

use crate::Conf;

/// Setup an entities by name material view for Thorium
///
/// Nothing reads this view and the schema declarations leave it out; setup creates it so a new
/// keyspace holds the same objects as one an older release created.
///
/// # Arguments
///
/// * `session` - The scylla session to use
/// * `config` - The Thorium config
pub(super) async fn setup_entities_name_mat_view(session: &Session, config: &Conf) {
    // create entities by name material view
    let table_create = format!(
        "CREATE MATERIALIZED VIEW IF NOT EXISTS {ns}.entities_by_name AS \
            SELECT name, group, kind, created, year, bucket, id, submitter, kind_data, description, image FROM {ns}.entities \
            WHERE name IS NOT NULL \
            AND group IS NOT NULL \
            AND kind IS NOT NULL \
            AND created IS NOT NULL \
            AND year IS NOT NULL \
            AND bucket IS NOT NULL \
            AND id IS NOT NULL
            PRIMARY KEY (name, group, kind, created, year, bucket, id)",
        ns = config.thorium.namespace,
    );
    session
        .query_unpaged(table_create, &[])
        .await
        .expect("failed to add entities by name materialized view");
}
