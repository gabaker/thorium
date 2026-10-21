//! Backup and restore support for network policies

use ahash::AHasher;
use chrono::{DateTime, Utc};
use rkyv::{Archive, CheckBytes, Deserialize, Serialize};
use scylla::SerializeRow;
use std::hash::Hasher;
use thorium::Conf;
use uuid::Uuid;

use super::Versioned;
use crate::backup::{Backup, Restore, Scrub, Utils};
use thorium::models::schema::ScyllaProjection;
use thorium::models::schema::scylla::NetworkPoliciesTable;

/// A network policies row as format 2 archives it; `None` is a cell the source row held unset
#[derive(Debug, Archive, Serialize, Deserialize, ScyllaProjection)]
#[archive_attr(derive(Debug, CheckBytes))]
#[thorium_projection(of = NetworkPoliciesTable)]
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
    /// The table this archive struct backs up, read through its generated statements
    type Table = NetworkPoliciesTable;

    /// The columns of this table's partition key, used to name rows that fail to decode
    fn partition_key() -> &'static [&'static str] {
        &["group"]
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

/// The values a restored row binds by column name to the table's generated full-row insert
#[derive(SerializeRow)]
pub struct NetworkPolicyValues {
    /// The `group` column
    pub group: String,
    /// The `name` column
    pub name: String,
    /// The `id` column
    pub id: Option<Uuid>,
    /// The `k8s_name` column
    pub k8s_name: Option<String>,
    /// The `created` column
    pub created: Option<DateTime<Utc>>,
    /// The `ingress` column
    pub ingress: Option<String>,
    /// The `egress` column
    pub egress: Option<String>,
    /// The `forced_policy` column
    pub forced_policy: Option<bool>,
    /// The `default_policy` column
    pub default_policy: Option<bool>,
}

/// Implement restore support for the network policies table
#[async_trait::async_trait]
impl Restore for NetworkPolicy {
    // Network policies are not partitioned by time, so just return unit type
    type PartitionConf = ();

    /// The row the generated full-row insert binds by name
    type Values = NetworkPolicyValues;

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
        NetworkPolicyValues {
            group: self.group,
            name: self.name,
            id: self.id,
            k8s_name: self.k8s_name,
            created: self.created,
            ingress: self.ingress,
            egress: self.egress,
            forced_policy: self.forced_policy,
            default_policy: self.default_policy,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the layout of the archived [`NetworkPolicy`] row, which written backups depend on
    #[test]
    fn network_policy_layout_is_pinned() {
        // the archived row's size and the offset of every field must stay as backups hold them
        assert_layout!(ArchivedNetworkPolicy, 92, {
            group: 52,
            name: 60,
            id: 72,
            k8s_name: 0,
            created: 12,
            ingress: 28,
            egress: 40,
            forced_policy: 68,
            default_policy: 70,
        });
    }

    /// Pins the layout of the archived [`NetworkPolicyV1`] row, which written backups depend on
    #[test]
    fn network_policy_v1_layout_is_pinned() {
        // the archived row's size and the offset of every field must stay as backups hold them
        assert_layout!(ArchivedNetworkPolicyV1, 72, {
            group: 16,
            name: 24,
            id: 0,
            k8s_name: 32,
            created: 56,
            ingress: 40,
            egress: 48,
            forced_policy: 68,
            default_policy: 69,
        });
    }
}
