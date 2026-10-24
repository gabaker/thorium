//! The upgrade types of the first Helm release's operator, as that release reads a
//! `ThoriumCluster`'s status, spec, and upgrade state
//!
//! These are copies of the types the previous release deserializes with, so a test can check that
//! what a downgrade leaves behind parses with them. They have no catch-all variants, so a value
//! the newer operator added makes a parse fail the way it does for the real previous operator.

#![allow(
    dead_code,
    reason = "the copies hold every field the previous release reads, whether or not a test reads it"
)]

use serde::Deserialize;
use std::collections::BTreeMap;

/// A revision id as the previous release validates it: exactly `YYYY-MM-vNN` with a month from
/// 01 to 12
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct RevisionId(String);

impl RevisionId {
    /// Get this revision id as a string
    #[must_use]
    pub fn as_str(&self) -> &str {
        // the id as it was parsed
        &self.0
    }
}

impl TryFrom<String> for RevisionId {
    type Error = String;

    /// Parse a revision id, rejecting anything that isn't `YYYY-MM-vNN`
    ///
    /// # Arguments
    ///
    /// * `raw` - The string to parse
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        // split the id into its year, month, and number
        let bytes = raw.as_bytes();
        let digits = |range: std::ops::Range<usize>| bytes[range].iter().all(u8::is_ascii_digit);
        // check every part of the id is where it should be
        let valid = bytes.len() == 11
            && digits(0..4)
            && bytes[4] == b'-'
            && digits(5..7)
            && bytes[7] == b'-'
            && bytes[8] == b'v'
            && digits(9..11)
            && (b"01".as_slice()..=b"12".as_slice()).contains(&&bytes[5..7]);
        if valid {
            Ok(RevisionId(raw))
        } else {
            Err(format!("invalid revision id {raw}"))
        }
    }
}

/// The kind of an upgrade step
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum StepKind {
    /// A change to Kubernetes resources or Thorium's config
    Config,
    /// A check or change to a backing service
    Infra,
    /// A change to stored data
    Data,
    /// Work an admin must do by hand
    Manual,
}

/// A Thorium component an upgrade step pauses
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum UpgradeComponent {
    /// The Thorium API
    Api,
    /// The k8s scaler
    Scaler,
    /// The bare metal scaler
    BaremetalScaler,
    /// The event handler
    EventHandler,
    /// The search streamer
    SearchStreamer,
}

/// The state of a step in an upgrade plan
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum StepState {
    /// The step hasn't run yet
    Pending,
    /// The step is running
    Running,
    /// The step finished
    Done,
    /// The step had nothing to do
    Skipped,
    /// The step can't continue until something changes
    Blocked,
    /// The step failed
    Failed,
}

/// A step planned to bring a cluster to its target revision
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PlannedStep {
    /// The revision this step belongs to
    pub revision: String,
    /// The id of this step
    pub step: String,
    /// What kind of step this is
    pub kind: StepKind,
    /// What this step does
    pub description: String,
    /// The components paused while this step runs
    #[serde(default)]
    pub pause: Vec<UpgradeComponent>,
    /// The state of this step
    pub state: StepState,
    /// Details about the state of this step
    #[serde(default)]
    pub message: Option<String>,
}

/// An approval for a step that needs one
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct UpgradeApproval {
    /// The id of the approved step
    pub step: String,
    /// Where the backup taken before this step is
    pub backup: String,
}

/// A step recorded in a cluster's upgrade history
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AppliedStep {
    /// The revision this step belongs to
    pub revision: String,
    /// The id of this step
    pub step: String,
    /// How the step finished, or `Blocked` when it blocked on an admin
    pub outcome: StepState,
    /// Details about how the step finished or what it blocked on
    #[serde(default)]
    pub message: Option<String>,
    /// When the step finished or blocked
    #[serde(default)]
    pub at: Option<String>,
    /// What applied this step
    #[serde(default)]
    pub by: Option<String>,
}

/// The upgrade state recorded for a Thorium deployment
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct UpgradeState {
    /// The version of this layout
    #[serde(default)]
    pub schema_version: u32,
    /// The revision this deployment is at, validated the way the previous release validates it
    pub revision: RevisionId,
    /// The Thorium version that recorded the current revision
    #[serde(default)]
    pub applied_by_version: Option<String>,
    /// The steps applied to this deployment, oldest first
    #[serde(default)]
    pub applied: Vec<AppliedStep>,
    /// Data schema markers by backend
    #[serde(default)]
    pub schema: BTreeMap<String, String>,
}

/// The upgrade progress reported in a `ThoriumCluster`'s status
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct UpgradeStatus {
    /// The revision the cluster is at
    pub current: Option<String>,
    /// The revision the cluster is upgrading to
    pub target: Option<String>,
    /// The newest revision the operator knows about
    pub latest: Option<String>,
    /// The steps left to reach the target
    #[serde(default)]
    pub steps: Vec<PlannedStep>,
}

/// The phase a `ThoriumCluster` is in
///
/// The first Helm release's operator knows only these three phases and fails to list any
/// `ThoriumCluster` whose status holds another one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum ClusterPhase {
    /// The operator is deploying or updating this cluster
    Provisioning,
    /// Every component of this cluster has been deployed
    Ready,
    /// The last reconcile of this cluster failed
    Error,
}

/// The observed state of a `ThoriumCluster`
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ThoriumClusterStatus {
    /// The current phase
    pub phase: Option<ClusterPhase>,
    /// Details about the current phase
    pub message: Option<String>,
    /// When the phase or message last changed
    pub last_transition: Option<String>,
    /// The `metadata.generation` of the spec this status describes
    pub observed_generation: Option<i64>,
    /// A hash of the last completed bootstrap
    pub bootstrap_hash: Option<String>,
    /// The revision this cluster is at and any upgrade steps left to run
    pub upgrade: Option<UpgradeStatus>,
}

/// How the operator moves a `ThoriumCluster` between revisions
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct UpgradeSpec {
    /// The revision to upgrade this cluster to
    pub target_revision: Option<String>,
    /// Target the operator's latest revision when no target is set
    #[serde(default)]
    pub auto_target_dev: bool,
    /// Approvals for steps that change data or need manual work
    #[serde(default)]
    pub approvals: Vec<UpgradeApproval>,
}
