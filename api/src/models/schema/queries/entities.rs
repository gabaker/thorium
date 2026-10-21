//! The statements on the entities, associations, network policies, nodes, notifications, and
//! logs tables and their views

use crate::models::schema::scylla::{
    AssociationsTable, EntitiesByIdView, EntitiesTable, LogsTable, NetworkPoliciesByNameView,
    NetworkPoliciesDefaultView, NetworkPoliciesTable, NodesTable, NotificationsTable,
};
use crate::models::{
    AssociationListRow, EntityListRow, EntityListSupplementRow, EntityRow, LogLineRow,
    NetworkPolicyGroupRow, NetworkPolicyListRow, NetworkPolicyRow, NodeListLine, NodeRow,
    NotificationBackupRow, NotificationRow,
};

/// Statements on the entities table
#[thorium::table_queries(table = EntitiesTable)]
pub struct EntitiesQueries {
    /// Insert or overwrite an entity's row for a group
    #[insert(columns = all, ttl = none)]
    insert: Insert,
    /// Delete an entity from several groups
    #[delete(by_key, eq(kind), r#in(group), eq(year, bucket, created, id))]
    delete: Delete,
    /// The cursor listing entities of some kinds newest first across groups, against the
    /// table's ascending clustering order
    #[cursor(kind = time, group_by = group, r#in(kind), year = year, bucket = bucket,
             range = created, tie = id, order = "desc", project = EntityListRow,
             limit = per_partition)]
    list: Cursor,
    /// The number of entities in each partition of a token range, for the census
    #[census(project = crate::models::entities::EntityCensusRow)]
    census: Census,
}

/// Statements on the `entities_by_id` view, which finds entities by id
#[thorium::view_queries(view = EntitiesByIdView)]
pub struct EntitiesByIdQueries {
    /// An entity's rows in a set of groups
    #[select(project = EntityRow, eq(id), r#in(group))]
    get: Select,
    /// The rows of a set of entities in a set of groups
    #[select(project = EntityRow, r#in(id, group), chunk = 50)]
    get_many: Select,
    /// The names and kinds of a set of entities, in every group
    #[select(project = EntityListSupplementRow, r#in(id))]
    get_names_kinds_by_ids: Select,
}

/// Statements on the associations table
#[thorium::table_queries(table = AssociationsTable)]
pub struct AssociationsQueries {
    /// Insert or overwrite one direction of an association for a group
    #[insert(columns = all, ttl = none)]
    insert: Insert,
    /// Delete one direction of an association from several groups
    #[delete(
        by_key,
        r#in(group),
        eq(year, bucket, source, created, target, direction)
    )]
    delete: Delete,
    /// The cursor listing a source's associations newest first across groups
    #[cursor(kind = time, group_by = group, eq(source), year = year, bucket = bucket,
             range = created, tie = [target, direction], project = AssociationListRow,
             limit = per_partition)]
    list: Cursor,
    /// The number of associations in each partition of a token range, for the census
    #[census(project = crate::models::associations::AssociationCensusRow)]
    census: Census,
}

/// Statements on the `network_policies` table
#[thorium::table_queries(table = NetworkPoliciesTable)]
pub struct NetworkPoliciesQueries {
    /// Insert or overwrite a network policy's row for a group
    #[insert(columns = all, ttl = none)]
    insert: Insert,
    /// Delete a network policy from several groups
    #[delete(by_key, r#in(group), eq(name))]
    delete: Delete,
    /// Delete every network policy in a group
    #[delete(partition, eq(group))]
    delete_all_in_group: Delete,
    /// The cursor listing network policies by name across groups
    #[cursor(kind = grouped, group_by = group, sort = name, tie_breaker = id,
             project = NetworkPolicyListRow, limit = per_partition)]
    list: Cursor,
}

/// Statements on the `network_policies_by_name` view, which finds network policies by name
#[thorium::view_queries(view = NetworkPoliciesByNameView)]
pub struct NetworkPoliciesByNameQueries {
    /// A network policy's rows in a set of groups
    #[select(project = NetworkPolicyRow, eq(name), r#in(group))]
    get: Select,
    /// The rows of a set of network policies in a set of groups
    #[select(project = NetworkPolicyRow, r#in(name, group), chunk = 50)]
    get_many: Select,
    /// Whether a network policy is in a group
    #[select(project = NetworkPolicyGroupRow, eq(name, group), limit = 1)]
    exists: Select,
}

/// Statements on the `network_policies_default` view, which lists a group's default policies
#[thorium::view_queries(view = NetworkPoliciesDefaultView)]
pub struct NetworkPoliciesDefaultQueries {
    /// Every default network policy in a group
    #[select(project = NetworkPolicyListRow, eq(group))]
    get_default: Select,
}

/// Statements on the nodes table
#[thorium::table_queries(table = NodesTable)]
pub struct NodesQueries {
    /// Register a node, leaving its last heart beat in place
    #[insert(columns = [cluster, node, health, resources], ttl = none)]
    insert: Insert,
    /// A node's info
    #[select(project = NodeRow, eq(cluster, node))]
    get: Select,
    /// The info of a set of nodes in a set of clusters
    #[select(project = NodeRow, r#in(cluster, node))]
    get_many: Select,
    /// Update a node's health and resources
    #[update(set(health, resources), by_key, eq(cluster, node))]
    update: Update,
    /// Update a node's health and resources and record its heart beat
    #[update(set(health, resources, heart_beat), by_key, eq(cluster, node))]
    update_heart_beat: Update,
    /// The cursor listing the names of the nodes in one cluster at a time
    #[cursor(kind = simple, partition = cluster, tie = node, project = NodeListLine, limit = limit)]
    list: Cursor,
    /// The cursor listing the details of the nodes in one cluster at a time
    #[cursor(kind = simple, partition = cluster, tie = node, project = NodeRow, limit = limit)]
    list_details: Cursor,
}

/// Statements on the notifications table
#[thorium::table_queries(table = NotificationsTable)]
pub struct NotificationsQueries {
    /// Insert a notification that expires with the table's default TTL
    #[insert(columns = all, ttl = none)]
    insert: Insert,
    /// Insert a notification that never expires
    #[insert(columns = all, ttl = zero)]
    insert_no_expire: Insert,
    /// Every notification for an entity
    #[select(project = NotificationRow, eq(kind, key))]
    get: Select,
    /// Delete one notification
    #[delete(by_key, eq(kind, key, created, id))]
    delete: Delete,
    /// Delete every notification for an entity
    #[delete(partition, eq(kind, key))]
    delete_all: Delete,
    /// Every notification in a token range with the seconds its message has left to live, for
    /// backups
    #[select(project = NotificationBackupRow, token_range)]
    backup: Select,
    /// Restore a notification with the seconds it had left to live, 0 for never expiring
    #[insert(columns = all, ttl = bind)]
    restore: Insert,
}

/// Statements on the logs table
#[thorium::table_queries(table = LogsTable)]
pub struct LogsQueries {
    /// Insert one stage log line
    #[insert(columns = all, ttl = none)]
    insert: Insert,
    /// The lines of a stage at or after a position, up to a limit per bucket
    #[select(
        project = LogLineRow,
        eq(reaction, stage),
        r#in(bucket),
        range(position, ge),
        per_partition_limit
    )]
    get: Select,
}
