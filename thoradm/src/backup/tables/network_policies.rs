//! Backup and restore support for network policies

use ahash::AHasher;
use chrono::{DateTime, Utc};
use rkyv::{Archive, CheckBytes, Deserialize, Serialize};
use scylla::{
    DeserializeRow, client::session::Session, errors::PrepareError,
    statement::prepared::PreparedStatement,
};
use std::hash::Hasher;
use thorium::Conf;
use uuid::Uuid;

use super::Versioned;
use crate::backup::{Backup, Restore, Scrub, Utils};

/// A network policies row as format 2 archives it; `None` is a cell the source row held unset
#[derive(Debug, Archive, Serialize, Deserialize, DeserializeRow)]
#[archive_attr(derive(Debug, CheckBytes))]
#[scylla(flavor = "enforce_order", skip_name_checks)]
pub struct NetworkPolicy {
    /// The group the network policy is in
    pub group: String,
    /// The network policy's name
    pub name: String,
    /// The network policy's ID
    pub id: Option<Uuid>,
    /// The network policy's unique name in K8's
    pub k8s_name: Option<String>,
    /// The date the network policy is created
    pub created: Option<DateTime<Utc>>,
    /// The ingress settings for this network policy
    pub ingress: Option<String>,
    /// The egress settings for this network policy
    pub egress: Option<String>,
    /// Whether this network policy is forced in its group
    pub forced_policy: Option<bool>,
    /// Whether this network policy is default in its group
    pub default_policy: Option<bool>,
}

impl Utils for NetworkPolicy {
    fn name() -> &'static str {
        "network_policies"
    }
}

#[async_trait::async_trait]
impl Backup for NetworkPolicy {
    /// The columns of this table's partition key, used to name rows that fail to decode
    fn partition_key() -> &'static [&'static str] {
        &["group"]
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
        // build network policies get prepared statement
        scylla
            .prepare(format!(
                "SELECT group, name, id, k8s_name, created, ingress, egress, forced_policy, default_policy \
                FROM {}.{} \
                WHERE token(group) >= ? AND token(group) <= ?",
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
        hasher.write(self.group.as_bytes());
        // finish this hash and get its value
        hasher.finish()
    }
}

/// Implement scrub support for the network policies table
impl Scrub for NetworkPolicy {}

/// Implement restore support for the network policies table
#[async_trait::async_trait]
impl Restore for NetworkPolicy {
    // Network policies are not partitioned by time, so just return unit type
    type PartitionConf = ();

    /// The group, name, id, Kubernetes name, creation time, ingress, egress, and whether it is
    /// forced and default
    type Values = (
        String,
        String,
        Option<Uuid>,
        Option<String>,
        Option<DateTime<Utc>>,
        Option<String>,
        Option<String>,
        Option<bool>,
        Option<bool>,
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
                (group, name, id, k8s_name, created, ingress, egress, forced_policy, default_policy) \
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                ns,
                Self::name(),
            ))
            .await
    }

    /// Get the partition size for this data type
    ///
    /// Network policies are not partitioned by time, so we just return
    /// unit type here
    ///
    /// # Arguments
    ///
    /// * `conf` - The Thorium config
    fn partition_conf(_config: &Conf) -> Self::PartitionConf {}

    /// Returns the values the restore's insert binds for this row
    ///
    /// # Arguments
    ///
    /// * `_partition_conf` - Unused, as network policies aren't bucketed
    /// * `_format` - The format version the row was archived in
    fn values(self, _partition_conf: &Self::PartitionConf, _format: u32) -> Self::Values {
        // every column as the row holds it
        (
            self.group,
            self.name,
            self.id,
            self.k8s_name,
            self.created,
            self.ingress,
            self.egress,
            self.forced_policy,
            self.default_policy,
        )
    }
}

/// A network policies row as format 1 archives it
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[archive_attr(derive(Debug, CheckBytes))]
pub struct NetworkPolicyV1 {
    /// The group the network policy is in
    pub group: String,
    /// The network policy's name
    pub name: String,
    /// The network policy's ID
    pub id: Uuid,
    /// The network policy's unique name in K8's
    pub k8s_name: String,
    /// The date the network policy is created
    pub created: DateTime<Utc>,
    /// The ingress settings for this network policy
    pub ingress: String,
    /// The egress settings for this network policy
    pub egress: String,
    /// Whether this network policy is forced in its group
    pub forced_policy: bool,
    /// Whether this network policy is default in its group
    pub default_policy: bool,
}

impl From<NetworkPolicyV1> for NetworkPolicy {
    /// Converts a format-1 row, whose required fields are always set
    ///
    /// # Arguments
    ///
    /// * `row` - The format-1 row
    fn from(row: NetworkPolicyV1) -> Self {
        // format 1 could only hold rows with these cells set
        NetworkPolicy {
            group: row.group,
            name: row.name,
            id: Some(row.id),
            k8s_name: Some(row.k8s_name),
            created: Some(row.created),
            ingress: Some(row.ingress),
            egress: Some(row.egress),
            forced_policy: Some(row.forced_policy),
            default_policy: Some(row.default_policy),
        }
    }
}

/// Format 1 archives `NetworkPolicy` rows as [`NetworkPolicyV1`]
impl Versioned for NetworkPolicy {
    type V1 = NetworkPolicyV1;
}
