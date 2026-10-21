//! The scylla utils for network policies

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::models::schema::ScyllaProjection;
use crate::models::schema::scylla::{
    NetworkPoliciesByNameView, NetworkPoliciesDefaultView, NetworkPoliciesTable,
};

/// A single row of a network policy from Scylla
#[derive(Debug, Deserialize, ScyllaProjection)]
#[thorium_projection(of = NetworkPoliciesByNameView)]
pub struct NetworkPolicyRow {
    /// The groups the network policy is available to
    pub group: String,
    /// The name of the network policy
    pub name: String,
    /// The network policy's unique ID
    pub id: Uuid,
    /// The name of the network policy converted to something K8's will accept
    pub k8s_name: String,
    /// The time this policy was created
    pub created: DateTime<Utc>,
    /// The rules for ingress to the tool as a raw serialized string
    #[thorium(column = "ingress")]
    pub ingress_raw: String,
    /// The rules for egress from the tool as a raw serialized string
    #[thorium(column = "egress")]
    pub egress_raw: String,
    /// This network policy should always be applied to all tools spawned in its group(s)
    ///
    /// Forced policies are not actually saved to individual images and are applied in the
    /// scaler when images are spawned
    pub forced_policy: bool,
    /// This policy should be added by default if none are given when creating an image
    /// in the policy's group(s)
    ///
    /// Default policies are saved to all new images in their groups and can be seen in
    /// in the images' data just like any other policy. Unlike forced policies, default
    /// policies are "applied" in the API on image creation, so editing a policy to no
    /// longer be default in its groups will not remove the policy from images
    pub default_policy: bool,
}

/// A single row retrieved when listing network policies
#[derive(Debug, Deserialize, ScyllaProjection)]
#[thorium_projection(of(NetworkPoliciesTable, NetworkPoliciesDefaultView))]
pub struct NetworkPolicyListRow {
    /// One of the network policy's groups
    pub group: String,
    /// The name of the network policy
    pub name: String,
    /// The policy's unique ID
    pub id: Uuid,
}

/// The name and a group of a network policy, read to check which groups have it
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = NetworkPoliciesByNameView)]
pub struct NetworkPolicyGroupRow {
    /// The name of the network policy
    pub name: String,
    /// The group the network policy is in
    pub group: String,
}
