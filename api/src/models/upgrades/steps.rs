//! The steps of the catalog's edges: their kinds, the components they pause, where they sit in
//! expand, migrate, contract, and what they target

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Reversibility, SchemaBackend};

#[cfg(feature = "scylla-utils")]
use super::TransformDef;

/// The most objects one issue group may hold before a run of a step fails, unless the step sets
/// its own limit
pub const DEFAULT_GROUP_MAX: u64 = 5;

/// The kind of an upgrade step, which decides whether it may run unattended
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum StepKind {
    /// A change to Kubernetes resources or Thorium's config, run automatically
    Config,
    /// A check or change to a backing service that loses no data, run automatically
    Infra,
    /// A change to stored data that can't be undone, which needs an approval and a backup
    Data,
    /// Work an admin must do by hand, which needs an approval and a backup
    Manual,
    /// A value this build doesn't know, written by a newer operator
    #[serde(other)]
    Unknown,
}

impl StepKind {
    /// Whether a step of this kind needs an approval with a backup before it runs
    #[must_use]
    pub fn needs_approval(self) -> bool {
        // only irreversible data changes and manual work need an admin's sign off, and a kind
        // this build doesn't know is treated as needing one
        matches!(self, Self::Data | Self::Manual | Self::Unknown)
    }
}

/// A Thorium component that must be scaled to zero while an upgrade step runs
///
/// The operator records the component's replica count and scales its Deployment to zero before
/// the step runs, and restores it only after the new release's Deployments are applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
    /// A value this build doesn't know, written by a newer operator
    #[serde(other)]
    Unknown,
}

impl UpgradeComponent {
    /// The name of this component's Deployment
    ///
    /// A component this build doesn't know names no Deployment the charts create, so it is
    /// reported as `unknown`.
    #[must_use]
    pub fn deployment_name(self) -> &'static str {
        // map each component to the Deployment the charts create for it
        match self {
            Self::Api => "api",
            Self::Scaler => "scaler",
            Self::BaremetalScaler => "baremetal-scaler",
            Self::EventHandler => "event-handler",
            Self::SearchStreamer => "search-streamer",
            Self::Unknown => "unknown",
        }
    }
}

/// One step of an edge in the catalog
///
/// Step literals set the fields they need and take the rest from [`StepDef::BASE`].
#[derive(Debug, Clone, Copy)]
pub struct StepDef {
    /// The id of this step, unique within the catalog
    pub id: &'static str,
    /// What kind of step this is
    pub kind: StepKind,
    /// What this step does
    pub description: &'static str,
    /// The components the operator pauses (scales to zero) before this step runs and restores
    /// after the new release's Deployments are applied
    pub pause: &'static [UpgradeComponent],
    /// The thoradm migration that performs this step, if thoradm runs it
    pub thoradm_migration: Option<&'static str>,
    /// How to perform this step by hand when it can't run automatically
    pub manual_procedure: Option<&'static str>,
    /// The objects and fields this step brings to the target revision's definition, used to block
    /// it when they drift
    pub targets: &'static [SchemaTarget],
    /// Step ids this step makes redundant when both are in one plan
    pub supersedes: &'static [&'static str],
    /// Where this step sits in expand, migrate, contract (`None` for config and check steps)
    pub phase: Option<MigrationPhase>,
    /// The object transform this step runs, for a step that rewrites records
    #[cfg(feature = "scylla-utils")]
    pub transform: Option<&'static TransformDef>,
    /// How this step is undone on a downgrade (required on release edges that ship a reverse
    /// edge)
    pub reversibility: Option<Reversibility>,
    /// On a revert or reverse edge, the forward step this step undoes
    pub reverses: Option<&'static str>,
    /// The most objects one issue group may hold before a run of this step fails:
    /// [`DEFAULT_GROUP_MAX`] by default, 0 for families whose readers aren't hardened against bad
    /// objects, at most 20
    pub group_max: u64,
    /// Ids of earlier steps whose output this step's check reads; when one of them in the same
    /// plan finishes, the operator runs this step's check again before its approval
    pub reads_after: &'static [&'static str],
    /// Whether this step only checks or waits and changes no data: such a step counts as finished
    /// only within the upgrade attempt that ran it, so an attempt started after an upgrade stopped
    /// short of its target runs it again
    pub check_only: bool,
}

impl StepDef {
    /// A step with every field at its default, which step literals fill the fields they don't set
    /// from (`StepDef { id, kind, description, ..StepDef::BASE }`)
    pub const BASE: StepDef = StepDef {
        id: "",
        kind: StepKind::Config,
        description: "",
        pause: &[],
        thoradm_migration: None,
        manual_procedure: None,
        targets: &[],
        supersedes: &[],
        phase: None,
        #[cfg(feature = "scylla-utils")]
        transform: None,
        reversibility: None,
        reverses: None,
        group_max: DEFAULT_GROUP_MAX,
        reads_after: &[],
        check_only: false,
    };
}

/// Where a step sits in the expand, migrate, contract sequence
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum MigrationPhase {
    /// Additive schema, applied first with no approval
    Expand,
    /// Backfills and transforms, with the affected components paused, checkpointed
    Migrate,
    /// Drops and the end of a rename, once no supported reader needs the old form
    Contract,
    /// A value this build doesn't know, written by a newer operator
    #[serde(other)]
    Unknown,
}

/// An object or field a step brings to the target revision's definition
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchemaTarget {
    /// The backend the object belongs to
    pub backend: SchemaBackend,
    /// The object's name
    pub object: &'static str,
    /// The column, field, or mapping property, when the step targets less than the whole object
    pub field: Option<&'static str>,
}

/// The set of objects a transform rewrites or reads; two transforms on the same family are
/// assumed to conflict
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Family {
    /// A Scylla table
    ScyllaTable(&'static str),
    /// A Redis key family, such as `users`
    Redis(&'static str),
    /// A prefix in an S3 bucket
    S3Prefix {
        /// The bucket
        bucket: &'static str,
        /// The key prefix
        prefix: &'static str,
    },
}

/// What a step's check found: the same answer for operator executors and thoradm migrations
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Work {
    /// Nothing needs changing, and why (the step is skipped without an approval)
    Nothing(String),
    /// Data needs changing, and what
    Needed(String),
}
