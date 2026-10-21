//! Backup and restore support for nodes

use ahash::AHasher;
use bytecheck::CheckBytes;
use chrono::prelude::*;
use rkyv::{Archive, Deserialize, Serialize};
use scylla::DeserializeRow;
use scylla::client::session::Session;
use scylla::errors::PrepareError;
use scylla::statement::prepared::PreparedStatement;
use std::hash::Hasher;
use thorium::Conf;
use thorium::models::{NodeHealth, Resources};

use super::Versioned;
use crate::backup::{Backup, Restore, Scrub, Utils};

/// A nodes row as format 2 archives it; `None` is a cell the source row held unset
#[derive(Debug, Archive, Serialize, Deserialize, DeserializeRow)]
#[archive_attr(derive(Debug, CheckBytes))]
#[scylla(flavor = "enforce_order", skip_name_checks)]
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
    /// The columns of this table's partition key, used to name rows that fail to decode
    fn partition_key() -> &'static [&'static str] {
        &["cluster"]
    }

    /// The prepared statement to use when retrieving data from Scylla
    ///
    /// # Arguments
    ///
    /// * `scylla` - The scylla session to build a prepared statement with
    /// * `ns` - The namespace for this prepared statement
    async fn prepared_statement(
        scylla: &Session,
        ns: &str,
    ) -> Result<PreparedStatement, PrepareError> {
        // build logs get prepared statement
        scylla
            .prepare(format!(
                "SELECT cluster, node, health, heart_beat, resources \
                FROM {}.{} \
                Where token(cluster) >= ? AND token(cluster) <= ?",
                ns,
                Self::name(),
            ))
            .await
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

/// Implement restore support for the samples list table
#[async_trait::async_trait]
impl Restore for Node {
    // The partition size is constant, so the partition config is just
    // the size itself
    type PartitionConf = u16;

    /// The cluster, node, health, last heartbeat, and resources
    type Values = (
        String,
        String,
        NodeHealth,
        Option<DateTime<Utc>>,
        Option<Resources>,
    );

    /// The prepared statement to use when restoring data to scylla
    ///
    /// # Arguments
    ///
    /// * `scylla` - A scylla client
    /// * `ns` - The namespace in scylla this table is from
    async fn prepared_statement(
        scylla: &Session,
        ns: &str,
    ) -> Result<PreparedStatement, PrepareError> {
        scylla
            .prepare(format!(
                "INSERT INTO {}.{} \
                (cluster, node, health, heart_beat, resources) \
                VALUES (?, ?, ?, ?, ?)",
                ns,
                Self::name(),
            ))
            .await
    }

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
        (
            self.cluster,
            self.node,
            NodeHealth::Registered,
            None,
            self.resources,
        )
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
    type V1 = NodeV1;
}
