//! The Scylla tables and views for network policies

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::models::NetworkPolicyRule;
use crate::models::schema::{JsonText, ScyllaTable, ScyllaView};

/// Network policies by group, the ground truth for network policies
#[derive(ScyllaTable)]
#[thorium_table(name = "network_policies")]
pub struct NetworkPoliciesTable {
    /// The group this policy is in
    #[thorium(partition)]
    pub group: String,
    /// The policy's name
    #[thorium(cluster = "asc")]
    pub name: String,
    /// The policy's id
    pub id: Uuid,
    /// The policy's name in Kubernetes
    pub k8s_name: String,
    /// When this policy was created
    pub created: DateTime<Utc>,
    /// The ingress rules; `None` is stored as the text `null`, as today
    pub ingress: JsonText<Option<Vec<NetworkPolicyRule>>>,
    /// The egress rules; `None` is stored as the text `null`, as today
    pub egress: JsonText<Option<Vec<NetworkPolicyRule>>>,
    /// Whether this policy is applied to every image in its groups
    pub forced_policy: bool,
    /// Whether this policy is added to new images by default
    pub default_policy: bool,
}
/// Network policies by name, for getting a policy across groups
#[derive(ScyllaView)]
#[thorium_view(name = "network_policies_by_name", base = NetworkPoliciesTable)]
pub struct NetworkPoliciesByNameView {
    /// The policy's name
    #[thorium(partition)]
    pub name: String,
    /// The group this policy is in
    #[thorium(cluster = "asc")]
    pub group: String,
    /// The policy's id
    pub id: Uuid,
    /// The policy's name in Kubernetes
    pub k8s_name: String,
    /// When this policy was created
    pub created: DateTime<Utc>,
    /// The ingress rules
    pub ingress: JsonText<Option<Vec<NetworkPolicyRule>>>,
    /// The egress rules
    pub egress: JsonText<Option<Vec<NetworkPolicyRule>>>,
    /// Whether this policy is forced
    pub forced_policy: bool,
    /// Whether this policy is a default policy
    pub default_policy: bool,
}
/// The default network policies in each group
#[derive(ScyllaView)]
#[thorium_view(name = "network_policies_default", base = NetworkPoliciesTable, filter = "default_policy = true")]
pub struct NetworkPoliciesDefaultView {
    /// The group this policy is in
    #[thorium(partition)]
    pub group: String,
    /// The policy's name
    #[thorium(cluster = "asc")]
    pub name: String,
    /// Whether this policy is a default policy (always true in this view)
    #[thorium(cluster = "asc")]
    pub default_policy: bool,
    /// The policy's id
    pub id: Uuid,
}
