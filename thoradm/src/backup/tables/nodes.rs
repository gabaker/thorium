//! Backup and restore support for nodes

use ahash::AHasher;
use bytecheck::CheckBytes;
use chrono::prelude::*;
use rkyv::{Archive, Deserialize, Serialize};
use scylla::SerializeRow;
use std::hash::Hasher;
use thorium::Conf;
use thorium::models::{NodeHealth, Resources};

use super::Versioned;
use crate::backup::{Backup, Restore, Scrub, Utils};
use thorium::models::schema::ScyllaProjection;
use thorium::models::schema::scylla::NodesTable;

/// A nodes row as format 2 archives it; `None` is a cell the source row held unset
#[derive(Debug, Archive, Serialize, Deserialize, ScyllaProjection)]
#[archive_attr(derive(Debug, CheckBytes))]
#[thorium_projection(of = NodesTable)]
pub struct Node {
    /// The cluster this node is in
    pub cluster: String,
    /// The name of this node
    pub node: String,
    /// The current health of this node
    pub health: Option<NodeHealth>,
    /// When this node last checked in
    pub heart_beat: Option<DateTime<Utc>>,
    /// The resources this node has
    pub resources: Option<Resources>,
}

impl Utils for Node {
    /// The name of the table we are backing up
    fn name() -> &'static str {
        "nodes"
    }
}

#[async_trait::async_trait]
impl Backup for Node {
    /// The table this archive struct backs up, read through its generated statements
    type Table = NodesTable;

    /// The columns of this table's partition key, used to name rows that fail to decode
    fn partition_key() -> &'static [&'static str] {
        &["cluster"]
    }

    /// Hash this partitions info to see if we have changed partitions
    fn hash_partition(&self) -> u64 {
        // build a new hasher
        let mut hasher = AHasher::default();
        // ingest our partition key
        hasher.write(self.cluster.as_bytes());
        // finish this hash and get its value
        hasher.finish()
    }
}

/// Implement scrub support for the samples list table
impl Scrub for Node {}

/// The values a restored row binds by column name to the table's generated full-row insert
#[derive(SerializeRow)]
pub struct NodeValues {
    /// The `cluster` column
    pub cluster: String,
    /// The `node` column
    pub node: String,
    /// The `health` column
    pub health: NodeHealth,
    /// The `heart_beat` column
    pub heart_beat: Option<DateTime<Utc>>,
    /// The `resources` column
    pub resources: Option<Resources>,
}

/// Implement restore support for the samples list table
#[async_trait::async_trait]
impl Restore for Node {
    // The partition size is constant, so the partition config is just
    // the size itself
    type PartitionConf = u16;

    /// The row the generated full-row insert binds by name
    type Values = NodeValues;

    /// Get the partition size for this data type
    ///
    /// # Arguments
    ///
    /// * `conf` - The Thorium config
    fn partition_conf(_config: &Conf) -> u16 {
        0
    }

    /// Returns the values the restore's insert binds for this row: a restored node is
    /// registered but hasn't checked in since, so the scheduler counts none of its capacity until
    /// it does; its workers were Redis state the restore drops
    ///
    /// # Arguments
    ///
    /// * `_partition_size` - Unused, as nodes aren't bucketed
    /// * `_format` - The format version the row was archived in
    fn values(self, _partition_size: &u16, _format: u32) -> Self::Values {
        // the health and heartbeat are reset; the rest is as the row holds it
        NodeValues {
            cluster: self.cluster,
            node: self.node,
            health: NodeHealth::Registered,
            heart_beat: None,
            resources: self.resources,
        }
    }
}

/// A nodes row as format 1 archives it
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[archive_attr(derive(Debug, CheckBytes))]
pub struct NodeV1 {
    /// The cluster this node is in
    pub cluster: String,
    /// The name of this node
    pub node: String,
    /// The current health of this node
    pub health: NodeHealth,
    /// When this node last checked in
    pub heart_beat: Option<DateTime<Utc>>,
    /// The resources this node has
    pub resources: Resources,
}

impl From<NodeV1> for Node {
    /// Converts a format-1 row, whose required fields are always set
    ///
    /// # Arguments
    ///
    /// * `row` - The format-1 row
    fn from(row: NodeV1) -> Self {
        // format 1 could only hold rows with these cells set
        Node {
            cluster: row.cluster,
            node: row.node,
            health: Some(row.health),
            heart_beat: row.heart_beat,
            resources: Some(row.resources),
        }
    }
}

/// Format 1 archives `Node` rows as [`NodeV1`]
impl Versioned for Node {
    /// Format 1 rows decode as [`NodeV1`], which converts into `Node`
    type V1 = NodeV1;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the layout of the archived [`Node`] row, which written backups depend on
    #[test]
    fn node_layout_is_pinned() {
        // the archived row's size and the offset of every field must stay as backups hold them
        assert_layout!(ArchivedNode, 128, {
            cluster: 88,
            node: 96,
            health: 104,
            heart_beat: 0,
            resources: 16,
        });
    }

    /// Pins the layout of the archived [`NodeV1`] row, which written backups depend on
    #[test]
    fn node_v1_layout_is_pinned() {
        // the archived row's size and the offset of every field must stay as backups hold them
        assert_layout!(ArchivedNodeV1, 112, {
            cluster: 32,
            node: 40,
            health: 16,
            heart_beat: 0,
            resources: 48,
        });
    }
}
