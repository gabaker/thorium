//! Revision-based upgrades of a Thorium deployment
//!
//! A revision (`YYYY-MM-vNN`) names a point in Thorium's deployment history that needs a known
//! set of steps to reach from the revision before it. The catalog of revisions and steps lives
//! here so the operator (which plans and runs upgrades) and thoradm (which runs data
//! migrations) work from the same data. Decisions are made only from revisions; the Thorium
//! version a revision was released with is advisory.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

/// The `ConfigMap` in a Thorium namespace that records its upgrade state
pub const STATE_CONFIG_MAP: &str = "thorium-upgrade-state";

/// The key in [`STATE_CONFIG_MAP`] holding the JSON encoded [`UpgradeState`]
pub const STATE_KEY: &str = "state.json";

/// The version of the [`UpgradeState`] layout written by this build
pub const STATE_SCHEMA_VERSION: u32 = 1;

/// The label on [`STATE_CONFIG_MAP`] holding the recorded revision
pub const REVISION_LABEL: &str = "thorium.sandia.gov/state-revision";

/// The label on [`STATE_CONFIG_MAP`] holding the advisory Thorium version that applied it
pub const VERSION_LABEL: &str = "thorium.sandia.gov/state-version";

/// The label on [`STATE_CONFIG_MAP`] holding a shortened hash of the recorded state
pub const HASH_LABEL: &str = "thorium.sandia.gov/state-hash";

/// The annotation on [`STATE_CONFIG_MAP`] holding the full hash of the recorded state
pub const HASH_ANNOTATION: &str = "thorium.sandia.gov/state-hash";

/// The most applied steps kept in the state history, dropping the oldest first
pub const MAX_HISTORY: usize = 200;

/// The pattern every revision id matches
pub const REVISION_PATTERN: &str = r"^[0-9]{4}-[0-9]{2}-v[0-9]{2}$";

/// The longest value a Kubernetes label may hold
const MAX_LABEL_LEN: usize = 63;

/// How many hex characters of the state hash are kept in [`HASH_LABEL`]
const HASH_LABEL_LEN: usize = 32;

/// A revision id such as `2026-10-v02`
///
/// Ids are zero padded so sorting them as strings sorts them in upgrade order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RevisionId(String);

impl RevisionId {
    /// Get this revision id as a string
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for RevisionId {
    type Err = UpgradeError;

    /// Parse a revision id, rejecting anything that isn't `YYYY-MM-vNN`
    ///
    /// # Arguments
    ///
    /// * `raw` - The string to parse
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
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
            Ok(RevisionId(raw.to_owned()))
        } else {
            Err(UpgradeError::InvalidRevision(raw.to_owned()))
        }
    }
}

impl TryFrom<String> for RevisionId {
    type Error = UpgradeError;

    /// Parse a revision id from an owned string
    ///
    /// # Arguments
    ///
    /// * `raw` - The string to parse
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        raw.parse()
    }
}

impl From<RevisionId> for String {
    /// Get the string form of a revision id
    ///
    /// # Arguments
    ///
    /// * `id` - The revision id to convert
    fn from(id: RevisionId) -> Self {
        id.0
    }
}

impl fmt::Display for RevisionId {
    /// Write this revision id
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An error planning or recording an upgrade
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeError {
    /// A revision id isn't `YYYY-MM-vNN`
    InvalidRevision(String),
    /// A revision id is well formed but not in this build's catalog
    UnknownRevision(String),
    /// The cluster is at a newer revision than this build knows about
    AheadOfCatalog {
        /// The revision the cluster is at
        current: RevisionId,
        /// The newest revision this build knows about
        latest: RevisionId,
    },
    /// The recorded upgrade state uses a newer layout than this build reads
    SchemaAhead {
        /// The layout version of the recorded state
        found: u32,
        /// The newest layout version this build reads
        supported: u32,
    },
    /// The target revision is older than the revision the cluster is at
    Backwards {
        /// The revision the cluster is at
        current: RevisionId,
        /// The requested target revision
        target: RevisionId,
    },
}

impl fmt::Display for UpgradeError {
    /// Describe this error
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRevision(raw) => {
                write!(f, "Revision {raw:?} is not a revision id like 2026-10-v02")
            }
            Self::UnknownRevision(raw) => write!(
                f,
                "Revision {raw} is not known to this Thorium build (latest is {})",
                latest()
            ),
            Self::AheadOfCatalog { current, latest } => write!(
                f,
                "This cluster is at revision {current}, which is newer than this Thorium \
                 build's latest revision {latest}; deploy a Thorium build that knows \
                 revision {current} (downgrades are not supported)"
            ),
            Self::SchemaAhead { found, supported } => write!(
                f,
                "This cluster's upgrade state (schema version {found}) was written by a newer \
                 Thorium operator than this one (which reads up to schema version {supported}); \
                 downgrades are not supported, so deploy that newer Thorium operator again"
            ),
            Self::Backwards { current, target } => write!(
                f,
                "The target revision {target} is older than this cluster's revision \
                 {current}; upgrades never move backwards"
            ),
        }
    }
}

impl std::error::Error for UpgradeError {}

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
}

impl StepKind {
    /// Whether a step of this kind needs an approval with a backup before it runs
    #[must_use]
    pub fn needs_approval(self) -> bool {
        matches!(self, Self::Data | Self::Manual)
    }
}

/// A Thorium component an upgrade step may scale to zero while it runs
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
}

impl UpgradeComponent {
    /// The name of this component's Deployment
    #[must_use]
    pub fn deployment_name(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Scaler => "scaler",
            Self::BaremetalScaler => "baremetal-scaler",
            Self::EventHandler => "event-handler",
            Self::SearchStreamer => "search-streamer",
        }
    }
}

/// One step of a revision in the catalog
#[derive(Debug, Clone, Copy)]
pub struct StepDef {
    /// The id of this step, unique within the catalog
    pub id: &'static str,
    /// What kind of step this is
    pub kind: StepKind,
    /// What this step does
    pub description: &'static str,
    /// The components scaled to zero while this step runs
    pub quiesce: &'static [UpgradeComponent],
    /// The thoradm migration that performs this step, if thoradm runs it
    pub thoradm_migration: Option<&'static str>,
    /// How to perform this step by hand when it can't run automatically
    pub manual_procedure: Option<&'static str>,
}

/// A revision in the catalog
#[derive(Debug, Clone, Copy)]
pub struct CatalogEntry {
    /// The revision id
    pub id: &'static str,
    /// The Thorium version this revision was released with (advisory)
    pub released_with: &'static str,
    /// What this revision changes
    pub description: &'static str,
    /// The steps that bring a cluster from the previous revision to this one, in order
    pub steps: &'static [StepDef],
}

impl CatalogEntry {
    /// Get this entry's revision id
    ///
    /// # Panics
    ///
    /// Panics if the catalog holds a malformed id, which the catalog tests rule out.
    #[must_use]
    pub fn revision(&self) -> RevisionId {
        self.id.parse().expect("catalog revision ids are valid")
    }
}

/// The manual procedure for reindexing Elastic indexes whose `group` field isn't a keyword
pub const REINDEX_PROCEDURE: &str = "Back up Scylla (the search-streamer rebuilds every index from \
it), then scale the search-streamer \
Deployment to 0 replicas BEFORE deleting anything (Elastic recreates a deleted index with \
dynamic mappings if anything writes to it). Delete each Thorium index whose group field is not \
a keyword (the operator names them in the ThoriumCluster's status.upgrade.steps), then scale \
the search-streamer back up: it recreates every missing index with the right mappings and \
streams every result and tag from Scylla into it.";

/// The steps of revision `2026-10-v02`, the first revision deployed by the Helm charts
const HELM_STEPS: &[StepDef] = &[
    StepDef {
        id: "scylla-role",
        kind: StepKind::Infra,
        description: "Check that Thorium's Scylla role logs in with the configured credentials",
        quiesce: &[],
        thoradm_migration: None,
        manual_procedure: None,
    },
    StepDef {
        id: "elastic-identity",
        kind: StepKind::Infra,
        description: "Check that Thorium's Elastic user authenticates and holds the index \
                      privileges Thorium needs",
        quiesce: &[],
        thoradm_migration: None,
        manual_procedure: None,
    },
    StepDef {
        id: "elastic-reindex-keyword-mappings",
        kind: StepKind::Data,
        description: "Reindex Elastic indexes created without mappings, whose group field is \
                      not a keyword (megathor deployments before the indexes were left to the \
                      search-streamer); skipped when every index is already correct",
        quiesce: &[UpgradeComponent::SearchStreamer],
        thoradm_migration: Some("elastic-keyword-mappings"),
        manual_procedure: Some(REINDEX_PROCEDURE),
    },
    StepDef {
        id: "finalize",
        kind: StepKind::Config,
        description: "Record the new revision",
        quiesce: &[],
        thoradm_migration: None,
        manual_procedure: None,
    },
];

/// Every revision this build knows about, oldest first
pub const CATALOG: &[CatalogEntry] = &[
    CatalogEntry {
        id: "2026-10-v01",
        released_with: "1.8.1",
        description: "Thorium deployed by the minithor or megathor scripts before the Helm \
                      charts (the baseline every converted cluster starts from)",
        steps: &[],
    },
    CatalogEntry {
        id: "2026-10-v02",
        released_with: "1.8.1",
        description: "Thorium deployed by the Helm charts with the chart-managed operator",
        steps: HELM_STEPS,
    },
];

/// Get the newest revision this build knows about
///
/// # Panics
///
/// Panics if the catalog is empty, which the catalog tests rule out.
#[must_use]
pub fn latest() -> RevisionId {
    CATALOG
        .last()
        .expect("the catalog has at least one revision")
        .revision()
}

/// Get the catalog entry for a revision
///
/// # Arguments
///
/// * `id` - The revision to find
#[must_use]
pub fn entry(id: &RevisionId) -> Option<&'static CatalogEntry> {
    CATALOG.iter().find(|entry| entry.id == id.as_str())
}

/// Find a step anywhere in the catalog by its id
///
/// # Arguments
///
/// * `id` - The id of the step to find
#[must_use]
pub fn step(id: &str) -> Option<(&'static CatalogEntry, &'static StepDef)> {
    CATALOG
        .iter()
        .flat_map(|entry| entry.steps.iter().map(move |step| (entry, step)))
        .find(|(_, step)| step.id == id)
}

/// The state of a step in an upgrade plan
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum StepState {
    /// The step hasn't run yet
    Pending,
    /// The step is running
    Running,
    /// The step finished
    Done,
    /// The step had nothing to do
    Skipped,
    /// The step can't continue until something changes, such as an approval or manual work
    Blocked,
    /// The step failed
    Failed,
}

/// A step planned to bring a cluster to its target revision
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PlannedStep {
    /// The revision this step belongs to
    pub revision: String,
    /// The id of this step
    pub step: String,
    /// What kind of step this is
    pub kind: StepKind,
    /// What this step does
    pub description: String,
    /// The components scaled to zero while this step runs
    #[serde(default)]
    pub quiesce: Vec<UpgradeComponent>,
    /// The state of this step
    pub state: StepState,
    /// Details about the state of this step
    #[serde(default)]
    pub message: Option<String>,
}

/// Check that a revision is in the catalog and not newer than it
///
/// # Arguments
///
/// * `id` - The revision to check
fn known(id: &RevisionId) -> Result<(), UpgradeError> {
    // a revision past the end of the catalog came from a newer build
    let latest = latest();
    if *id > latest {
        return Err(UpgradeError::AheadOfCatalog {
            current: id.clone(),
            latest,
        });
    }
    // anything else must be a revision we know
    match entry(id) {
        Some(_) => Ok(()),
        None => Err(UpgradeError::UnknownRevision(id.to_string())),
    }
}

/// Plan the steps that bring a cluster from its revision to a target revision
///
/// Steps the state already records as done or skipped are left out, so a plan resumes where
/// an interrupted upgrade stopped.
///
/// # Arguments
///
/// * `state` - The cluster's recorded upgrade state
/// * `target` - The revision to upgrade to
pub fn plan(state: &UpgradeState, target: &RevisionId) -> Result<Vec<PlannedStep>, UpgradeError> {
    // a state in a newer layout may record things this build would misread or drop
    state.check_schema()?;
    // both revisions must be ones we know
    known(&state.revision)?;
    if *target > latest() {
        return Err(UpgradeError::UnknownRevision(target.to_string()));
    }
    known(target)?;
    // never move backwards
    if *target < state.revision {
        return Err(UpgradeError::Backwards {
            current: state.revision.clone(),
            target: target.clone(),
        });
    }
    // collect the unfinished steps of every revision after the current one up to the target
    let steps = CATALOG
        .iter()
        .filter(|entry| entry.id > state.revision.as_str() && entry.id <= target.as_str())
        .flat_map(|entry| entry.steps.iter().map(move |step| (entry, step)))
        .filter(|(entry, step)| !state.finished(entry.id, step.id))
        .map(|(entry, step)| PlannedStep {
            revision: entry.id.to_owned(),
            step: step.id.to_owned(),
            kind: step.kind,
            description: step.description.to_owned(),
            quiesce: step.quiesce.to_vec(),
            state: StepState::Pending,
            message: None,
        })
        .collect();
    Ok(steps)
}

/// Pick the revision a cluster should upgrade to
///
/// An explicit target always wins. Without one, `auto_target_dev` targets this build's latest
/// revision, and otherwise there is no target and the cluster waits for one.
///
/// # Arguments
///
/// * `target` - The target revision set on the cluster, if any
/// * `auto_target_dev` - Whether to target the latest revision when no target is set
pub fn resolve_target(
    target: Option<&str>,
    auto_target_dev: bool,
) -> Result<Option<RevisionId>, UpgradeError> {
    match target {
        // an explicit target must be a revision we know
        Some(raw) => {
            let target: RevisionId = raw.parse()?;
            if target > latest() {
                return Err(UpgradeError::UnknownRevision(target.to_string()));
            }
            known(&target)?;
            Ok(Some(target))
        }
        // dev clusters follow the latest revision
        None if auto_target_dev => Ok(Some(latest())),
        // anything else waits for an admin to pick a target
        None => Ok(None),
    }
}

/// An approval for a step that needs one
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UpgradeApproval {
    /// The id of the approved step
    pub step: String,
    /// Where the backup taken before this step is (a path, snapshot name, or ticket)
    pub backup: String,
}

/// A step recorded in a cluster's upgrade history
///
/// Finished steps are recorded as `Done` or `Skipped`. A step that blocks on an admin is also
/// recorded once as `Blocked`, which doesn't finish it, so a step later found to have nothing to
/// do can be told apart as fixed manually.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// When the step finished or blocked (RFC3339)
    #[serde(default)]
    pub at: Option<String>,
    /// What applied this step (the operator and its version, or a script)
    #[serde(default)]
    pub by: Option<String>,
}

/// The upgrade state recorded for a Thorium deployment
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeState {
    /// The version of this layout
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// The revision this deployment is at
    pub revision: RevisionId,
    /// The Thorium version that recorded the current revision (advisory)
    #[serde(default)]
    pub applied_by_version: Option<String>,
    /// The steps applied to this deployment, oldest first
    #[serde(default)]
    pub applied: Vec<AppliedStep>,
    /// Data schema markers by backend
    #[serde(default)]
    pub schema: BTreeMap<String, String>,
}

/// Serde helper for the layout version of state written before it was recorded
fn default_schema_version() -> u32 {
    STATE_SCHEMA_VERSION
}

/// Make a string safe to use as a Kubernetes label value
///
/// Characters outside `[A-Za-z0-9._-]` become `_`, the value is cut to 63 characters, and
/// leading or trailing characters that aren't alphanumeric are trimmed.
///
/// # Arguments
///
/// * `raw` - The string to convert
#[must_use]
pub fn label_value(raw: &str) -> String {
    // replace every character a label can't hold
    let replaced: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(MAX_LABEL_LEN)
        .collect();
    // labels must start and end with an alphanumeric character
    replaced
        .trim_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_owned()
}

impl UpgradeState {
    /// Create the state of a deployment at a revision
    ///
    /// # Arguments
    ///
    /// * `revision` - The revision the deployment is at
    /// * `version` - The Thorium version recording this state
    #[must_use]
    pub fn new(revision: RevisionId, version: &str) -> Self {
        UpgradeState {
            schema_version: STATE_SCHEMA_VERSION,
            revision,
            applied_by_version: Some(version.to_owned()),
            applied: Vec::new(),
            schema: BTreeMap::new(),
        }
    }

    /// Check that this build can read the layout this state was written in
    ///
    /// A state written by a newer build may hold fields this build would drop when saving it,
    /// so it must not be planned from or written back.
    pub fn check_schema(&self) -> Result<(), UpgradeError> {
        // only layouts up to this build's own are understood
        if self.schema_version > STATE_SCHEMA_VERSION {
            return Err(UpgradeError::SchemaAhead {
                found: self.schema_version,
                supported: STATE_SCHEMA_VERSION,
            });
        }
        Ok(())
    }

    /// Whether a step finished (done or skipped) in this deployment's history
    ///
    /// # Arguments
    ///
    /// * `revision` - The revision the step belongs to
    /// * `step` - The id of the step
    #[must_use]
    pub fn finished(&self, revision: &str, step: &str) -> bool {
        self.applied.iter().any(|applied| {
            applied.revision == revision
                && applied.step == step
                && matches!(applied.outcome, StepState::Done | StepState::Skipped)
        })
    }

    /// Record a step in this deployment's history, dropping the oldest entries past the cap
    ///
    /// # Arguments
    ///
    /// * `applied` - The step to record
    pub fn record(&mut self, applied: AppliedStep) {
        // add the step and drop the oldest entries if the history is too long
        self.applied.push(applied);
        if self.applied.len() > MAX_HISTORY {
            let excess = self.applied.len() - MAX_HISTORY;
            self.applied.drain(..excess);
        }
    }

    /// Move the recorded revision past every revision up to a target whose steps all finished
    ///
    /// Returns whether the revision moved.
    ///
    /// # Arguments
    ///
    /// * `target` - The newest revision to move to
    /// * `version` - The Thorium version recording the new revision
    pub fn advance(&mut self, target: &RevisionId, version: &str) -> bool {
        // walk the revisions after the current one in order, stopping at the first unfinished one
        let start = self.revision.clone();
        let mut moved = false;
        for entry in CATALOG
            .iter()
            .filter(|entry| entry.id > start.as_str() && entry.id <= target.as_str())
        {
            // stop at the first revision with an unfinished step
            if !entry
                .steps
                .iter()
                .all(|step| self.finished(entry.id, step.id))
            {
                break;
            }
            // every step of this revision finished so record it
            self.revision = entry.revision();
            self.applied_by_version = Some(version.to_owned());
            moved = true;
        }
        moved
    }

    /// Hash the parts of this state that describe the deployment
    ///
    /// Only the revision, the outcome of each applied step, and the schema markers are
    /// hashed, so timestamps and messages don't change it.
    #[must_use]
    pub fn hash(&self) -> String {
        // hash each part after its length so parts can't run into each other
        let mut hasher = Sha256::new();
        let mut add = |part: &str| {
            hasher.update((part.len() as u64).to_le_bytes());
            hasher.update(part.as_bytes());
        };
        add(self.revision.as_str());
        for applied in &self.applied {
            add(&applied.revision);
            add(&applied.step);
            add(&format!("{:?}", applied.outcome));
        }
        for (backend, marker) in &self.schema {
            add(backend);
            add(marker);
        }
        format!("{:x}", hasher.finalize())
    }

    /// Build the labels describing this state
    #[must_use]
    pub fn labels(&self) -> BTreeMap<String, String> {
        // label the revision, advisory version, and a shortened hash
        let mut labels = BTreeMap::new();
        labels.insert(REVISION_LABEL.to_owned(), self.revision.to_string());
        if let Some(version) = &self.applied_by_version {
            labels.insert(VERSION_LABEL.to_owned(), label_value(version));
        }
        labels.insert(
            HASH_LABEL.to_owned(),
            self.hash()[..HASH_LABEL_LEN].to_owned(),
        );
        labels
    }

    /// Describe a mismatch between the recorded Thorium version and the catalog's
    ///
    /// The version is advisory, so a mismatch is only worth a warning.
    #[must_use]
    pub fn version_mismatch(&self) -> Option<String> {
        // compare the recorded version with the one the catalog lists for this revision
        let recorded = self.applied_by_version.as_deref()?;
        let expected = entry(&self.revision)?.released_with;
        (recorded != expected).then(|| {
            format!(
                "Revision {} was recorded by Thorium {recorded}, but this build's catalog lists \
                 it as released with Thorium {expected}; revisions decide upgrades, so this is \
                 only advisory",
                self.revision
            )
        })
    }
}

/// The upgrade progress reported in a `ThoriumCluster`'s status
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UpgradeStatus {
    /// The revision the cluster is at
    pub current: Option<String>,
    /// The revision the cluster is upgrading to
    pub target: Option<String>,
    /// The newest revision the operator knows about
    pub latest: Option<String>,
    /// The steps left to reach the target (or the latest revision when there is no target)
    #[serde(default)]
    pub steps: Vec<PlannedStep>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a revision id that must be valid
    ///
    /// # Arguments
    ///
    /// * `raw` - The id to parse
    fn rev(raw: &str) -> RevisionId {
        raw.parse().expect("valid revision")
    }

    /// Revision ids must be zero padded `YYYY-MM-vNN` and sort in upgrade order
    #[test]
    fn revision_format_and_order() {
        // valid ids parse
        assert_eq!(rev("2026-10-v02").as_str(), "2026-10-v02");
        // malformed ids are rejected
        for bad in [
            "2026-10-v2",
            "2026-1-v02",
            "2026-13-v01",
            "2026-00-v01",
            "26-10-v01",
            "2026-10-V01",
            "2026-10-v001",
            " 2026-10-v01",
            "",
        ] {
            assert!(bad.parse::<RevisionId>().is_err(), "{bad} should fail");
        }
        // ids sort in upgrade order
        assert!(rev("2026-10-v02") > rev("2026-10-v01"));
        assert!(rev("2026-11-v01") > rev("2026-10-v99"));
        assert!(rev("2027-01-v01") > rev("2026-12-v99"));
        // ids round trip through serde as strings and invalid ones fail to deserialize
        let json = serde_json::to_string(&rev("2026-10-v01")).expect("serialize");
        assert_eq!(json, "\"2026-10-v01\"");
        assert!(serde_json::from_str::<RevisionId>("\"latest\"").is_err());
    }

    /// The CRD's revision pattern accepts what the parser accepts, apart from the month range
    #[test]
    fn pattern_matches_parser() {
        // compile the pattern the CRD validates target revisions with
        let pattern = regex::Regex::new(REVISION_PATTERN).expect("valid pattern");
        // well formed ids pass both
        for good in ["2026-10-v02", "2027-01-v99"] {
            assert!(pattern.is_match(good) && good.parse::<RevisionId>().is_ok());
        }
        // malformed ids fail both
        for bad in [
            "2026-10-v2",
            "2026-1-v02",
            "2026-10-V01",
            "latest",
            "2026-10-v001",
        ] {
            assert!(!pattern.is_match(bad) && bad.parse::<RevisionId>().is_err());
        }
        // only the parser checks the month is in range
        for month in ["2026-13-v01", "2026-00-v01"] {
            assert!(pattern.is_match(month) && month.parse::<RevisionId>().is_err());
        }
    }

    /// The catalog's ids are valid, strictly increasing, and its step ids unique
    #[test]
    fn catalog_is_consistent() {
        // every id is valid and newer than the one before it
        let ids = CATALOG
            .iter()
            .map(CatalogEntry::revision)
            .collect::<Vec<_>>();
        assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
        // every step id is unique across the catalog
        let mut steps = CATALOG
            .iter()
            .flat_map(|entry| entry.steps.iter().map(|step| step.id))
            .collect::<Vec<_>>();
        let count = steps.len();
        steps.sort_unstable();
        steps.dedup();
        assert_eq!(steps.len(), count, "duplicate step ids");
        // steps that need approval say how to do them by hand
        for (_, step) in CATALOG
            .iter()
            .flat_map(|e| e.steps.iter().map(move |s| (e, s)))
        {
            if step.kind.needs_approval() {
                assert!(
                    step.manual_procedure.is_some(),
                    "{} has no procedure",
                    step.id
                );
            }
        }
        assert_eq!(latest(), rev("2026-10-v02"));
    }

    /// Planning covers every unfinished step between the two revisions
    #[test]
    fn plans_between_revisions() {
        // a baseline cluster upgrading to the latest revision runs every Helm step
        let mut state = UpgradeState::new(rev("2026-10-v01"), "1.8.1");
        let steps = plan(&state, &rev("2026-10-v02")).expect("plan");
        let ids = steps.iter().map(|s| s.step.as_str()).collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec![
                "scylla-role",
                "elastic-identity",
                "elastic-reindex-keyword-mappings",
                "finalize"
            ]
        );
        assert!(steps.iter().all(|s| s.state == StepState::Pending));
        // finished steps are left out so an interrupted upgrade resumes
        for (step, outcome) in [
            ("scylla-role", StepState::Done),
            ("elastic-identity", StepState::Skipped),
        ] {
            state.record(AppliedStep {
                revision: "2026-10-v02".to_owned(),
                step: step.to_owned(),
                outcome,
                message: None,
                at: None,
                by: None,
            });
        }
        let steps = plan(&state, &rev("2026-10-v02")).expect("plan");
        let ids = steps.iter().map(|s| s.step.as_str()).collect::<Vec<_>>();
        assert_eq!(ids, ["elastic-reindex-keyword-mappings", "finalize"]);
        // a cluster at its target has nothing to do
        let current = UpgradeState::new(rev("2026-10-v02"), "1.8.1");
        assert_eq!(
            plan(&current, &rev("2026-10-v02")).expect("plan"),
            Vec::new()
        );
    }

    /// The revision only moves past revisions whose steps all finished
    #[test]
    fn advance_stops_at_unfinished_revisions() {
        // nothing finished means the revision stays put
        let mut state = UpgradeState::new(rev("2026-10-v01"), "1.8.1");
        assert!(!state.advance(&rev("2026-10-v02"), "1.9.0"));
        assert_eq!(state.revision, rev("2026-10-v01"));
        // finishing every step of the next revision moves to it
        for step in HELM_STEPS {
            state.record(AppliedStep {
                revision: "2026-10-v02".to_owned(),
                step: step.id.to_owned(),
                outcome: StepState::Done,
                message: None,
                at: None,
                by: None,
            });
        }
        assert!(state.advance(&rev("2026-10-v02"), "1.9.0"));
        assert_eq!(state.revision, rev("2026-10-v02"));
        assert_eq!(state.applied_by_version.as_deref(), Some("1.9.0"));
        // a target at the current revision moves nothing
        assert!(!state.advance(&rev("2026-10-v02"), "1.9.0"));
    }

    /// Planning rejects backwards, unknown, and too new revisions
    #[test]
    fn plan_errors() {
        // targets older than the cluster are refused
        let current = UpgradeState::new(rev("2026-10-v02"), "1.8.1");
        assert!(matches!(
            plan(&current, &rev("2026-10-v01")),
            Err(UpgradeError::Backwards { .. })
        ));
        // targets past the catalog are unknown
        assert!(matches!(
            plan(&current, &rev("2099-01-v01")),
            Err(UpgradeError::UnknownRevision(_))
        ));
        // a cluster past the catalog came from a newer build
        let ahead = UpgradeState::new(rev("2099-01-v01"), "9.9.9");
        assert!(matches!(
            plan(&ahead, &rev("2026-10-v02")),
            Err(UpgradeError::AheadOfCatalog { .. })
        ));
        // a well formed id inside the catalog's range that isn't in it is unknown
        let gap = UpgradeState::new(rev("2026-10-v00"), "1.8.1");
        assert!(matches!(
            plan(&gap, &rev("2026-10-v02")),
            Err(UpgradeError::UnknownRevision(_))
        ));
        // a state written in a newer layout is refused even at a known revision
        let mut newer = UpgradeState::new(rev("2026-10-v01"), "9.9.9");
        newer.schema_version = STATE_SCHEMA_VERSION + 1;
        assert_eq!(
            plan(&newer, &rev("2026-10-v02")),
            Err(UpgradeError::SchemaAhead {
                found: STATE_SCHEMA_VERSION + 1,
                supported: STATE_SCHEMA_VERSION
            })
        );
        // the current layout and older ones are read
        newer.schema_version = STATE_SCHEMA_VERSION;
        assert!(newer.check_schema().is_ok());
        newer.schema_version = 0;
        assert!(newer.check_schema().is_ok());
    }

    /// An explicit target wins, then auto targeting, then no target
    #[test]
    fn target_resolution() {
        // explicit targets win over auto targeting
        assert_eq!(
            resolve_target(Some("2026-10-v01"), true).expect("resolve"),
            Some(rev("2026-10-v01"))
        );
        // auto targeting picks the latest revision
        assert_eq!(resolve_target(None, true).expect("resolve"), Some(latest()));
        // otherwise there is no target
        assert_eq!(resolve_target(None, false).expect("resolve"), None);
        // malformed targets are invalid
        assert_eq!(
            resolve_target(Some("latest"), false),
            Err(UpgradeError::InvalidRevision("latest".to_owned()))
        );
        // well formed targets past the catalog or in a gap inside its range are unknown
        for unknown in ["2099-01-v01", "2026-10-v00"] {
            assert_eq!(
                resolve_target(Some(unknown), false),
                Err(UpgradeError::UnknownRevision(unknown.to_owned()))
            );
        }
    }

    /// The state hash ignores timestamps and messages but tracks outcomes
    #[test]
    fn state_hash_is_stable() {
        // build a state with the one step convert-to-helm.sh records, which isn't a catalog step
        let mut state = UpgradeState::new(rev("2026-10-v01"), "1.8.1");
        let applied = AppliedStep {
            revision: "2026-10-v01".to_owned(),
            step: "convert-to-helm".to_owned(),
            outcome: StepState::Done,
            message: Some("converted".to_owned()),
            at: Some("2026-10-08T00:00:00Z".to_owned()),
            by: Some("convert-to-helm.sh".to_owned()),
        };
        state.record(applied.clone());
        let base = state.hash();
        // timestamps and messages don't change the hash
        let mut other = state.clone();
        other.applied[0].at = Some("2027-01-01T00:00:00Z".to_owned());
        other.applied[0].message = None;
        assert_eq!(other.hash(), base);
        // outcomes, revisions, and schema markers do
        other.applied[0].outcome = StepState::Skipped;
        assert_ne!(other.hash(), base);
        let mut other = state.clone();
        other.revision = rev("2026-10-v02");
        assert_ne!(other.hash(), base);
        let mut other = state.clone();
        other.schema.insert("elastic".to_owned(), "1".to_owned());
        assert_ne!(other.hash(), base);
        // parts are length prefixed, so moving text between neighbouring fields changes it
        let mut joined = state.clone();
        joined.schema.insert("ab".to_owned(), "c".to_owned());
        let mut split = state.clone();
        split.schema.insert("a".to_owned(), "bc".to_owned());
        assert_ne!(joined.hash(), split.hash());
    }

    /// The history is capped by dropping the oldest entries
    #[test]
    fn history_is_capped() {
        // record more steps than the cap
        let mut state = UpgradeState::new(rev("2026-10-v01"), "1.8.1");
        for i in 0..(MAX_HISTORY + 5) {
            state.record(AppliedStep {
                revision: "2026-10-v01".to_owned(),
                step: format!("step-{i}"),
                outcome: StepState::Done,
                message: None,
                at: None,
                by: None,
            });
        }
        // only the newest entries are kept
        assert_eq!(state.applied.len(), MAX_HISTORY);
        assert_eq!(state.applied[0].step, "step-5");
    }

    /// Labels hold valid values even for versions with build metadata, and label values are
    /// cut, trimmed, and stripped of unsafe characters
    #[test]
    fn labels_are_valid() {
        // build a state recorded by a version with build metadata
        let state = UpgradeState::new(rev("2026-10-v02"), "1.9.0-rc.1+abc");
        let labels = state.labels();
        assert_eq!(labels[REVISION_LABEL], "2026-10-v02");
        assert_eq!(labels[VERSION_LABEL], "1.9.0-rc.1_abc");
        assert_eq!(labels[HASH_LABEL].len(), HASH_LABEL_LEN);
        // every value fits in a label
        for value in labels.values() {
            assert!(value.len() <= MAX_LABEL_LEN);
            assert!(
                value
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || ".-_".contains(c))
            );
        }
        // long or oddly ending values are cut and trimmed
        assert_eq!(label_value(&"a".repeat(80)).len(), MAX_LABEL_LEN);
        assert_eq!(label_value("-1.0+"), "1.0");
        // unsafe characters in the middle are replaced and a value of only them is emptied
        assert_eq!(label_value("a+b.c"), "a_b.c");
        assert_eq!(label_value("+++"), "");
    }

    /// The minimal state convert-to-helm.sh writes parses with defaults for missing fields
    #[test]
    fn script_seed_parses() {
        // the seed written by deploy/charts/scripts/convert-to-helm.sh
        let seed = r#"{"schema_version":1,"revision":"2026-10-v01","applied":[{"revision":"2026-10-v01","step":"convert-to-helm","outcome":"Done","at":"2026-10-08T00:00:00Z","by":"convert-to-helm.sh"}]}"#;
        let state: UpgradeState = serde_json::from_str(seed).expect("seed should parse");
        assert_eq!(state.revision, rev("2026-10-v01"));
        assert!(state.finished("2026-10-v01", "convert-to-helm"));
        assert!(state.schema.is_empty());
        // the script records no Thorium version, so there is nothing to warn about
        assert_eq!(state.version_mismatch(), None);
        // a bare revision parses too
        let bare: UpgradeState =
            serde_json::from_str(r#"{"revision":"2026-10-v01"}"#).expect("bare should parse");
        assert_eq!(bare.schema_version, STATE_SCHEMA_VERSION);
    }

    /// A recorded version that differs from the catalog's is reported
    #[test]
    fn version_mismatch_is_advisory() {
        // the catalog's own version matches
        let released = entry(&rev("2026-10-v02")).expect("entry").released_with;
        let state = UpgradeState::new(rev("2026-10-v02"), released);
        assert_eq!(state.version_mismatch(), None);
        // any other version is reported with both versions
        let state = UpgradeState::new(rev("2026-10-v02"), "0.0.1");
        let warning = state.version_mismatch().expect("mismatch");
        assert!(warning.contains("0.0.1") && warning.contains(released));
        // a revision outside the catalog has no version to compare with
        let unknown = UpgradeState::new(rev("2099-01-v01"), "0.0.1");
        assert_eq!(unknown.version_mismatch(), None);
    }

    /// Build an applied step of revision `2026-10-v02`
    ///
    /// # Arguments
    ///
    /// * `step` - The id of the step
    /// * `outcome` - How the step finished
    fn applied(step: &str, outcome: StepState) -> AppliedStep {
        AppliedStep {
            revision: "2026-10-v02".to_owned(),
            step: step.to_owned(),
            outcome,
            message: None,
            at: None,
            by: None,
        }
    }

    /// Only done and skipped steps count as finished, so failed or blocked ones run again
    #[test]
    fn failed_steps_are_replanned() {
        // record every step of the Helm revision as failed or blocked
        let mut state = UpgradeState::new(rev("2026-10-v01"), "1.8.1");
        for (index, step) in HELM_STEPS.iter().enumerate() {
            let outcome = if index % 2 == 0 {
                StepState::Failed
            } else {
                StepState::Blocked
            };
            state.record(applied(step.id, outcome));
        }
        // every step is still planned and the revision doesn't move
        assert_eq!(
            plan(&state, &rev("2026-10-v02")).expect("plan").len(),
            HELM_STEPS.len()
        );
        assert!(!state.advance(&rev("2026-10-v02"), "1.9.0"));
        // a step finished under another revision doesn't count for this one
        let mut other = UpgradeState::new(rev("2026-10-v01"), "1.8.1");
        other.record(AppliedStep {
            revision: "2026-10-v01".to_owned(),
            ..applied("scylla-role", StepState::Done)
        });
        assert!(!other.finished("2026-10-v02", "scylla-role"));
    }

    /// Steps and revisions are found by id and unknown ids find nothing
    #[test]
    fn step_and_entry_lookup() {
        // a step is found with the revision it belongs to
        let (entry, step) = super::step("elastic-reindex-keyword-mappings").expect("step");
        assert_eq!(entry.id, "2026-10-v02");
        assert_eq!(step.kind, StepKind::Data);
        assert_eq!(step.thoradm_migration, Some("elastic-keyword-mappings"));
        assert_eq!(step.quiesce, &[UpgradeComponent::SearchStreamer]);
        assert!(super::step("nope").is_none());
        // a revision is found by id
        assert_eq!(
            super::entry(&rev("2026-10-v01")).map(|entry| entry.steps.len()),
            Some(0)
        );
        assert!(super::entry(&rev("2026-10-v00")).is_none());
    }

    /// Only data and manual steps need an approval
    #[test]
    fn approval_by_kind() {
        // automatic kinds run unattended
        assert!(!StepKind::Config.needs_approval());
        assert!(!StepKind::Infra.needs_approval());
        // kinds that can lose data or need a person need an approval
        assert!(StepKind::Data.needs_approval());
        assert!(StepKind::Manual.needs_approval());
    }

    /// Every component names the Deployment the operator creates for it
    #[test]
    fn component_deployment_names() {
        // the names match the operator's component Deployments
        let names = [
            UpgradeComponent::Api,
            UpgradeComponent::Scaler,
            UpgradeComponent::BaremetalScaler,
            UpgradeComponent::EventHandler,
            UpgradeComponent::SearchStreamer,
        ]
        .map(UpgradeComponent::deployment_name);
        assert_eq!(
            names,
            [
                "api",
                "scaler",
                "baremetal-scaler",
                "event-handler",
                "search-streamer"
            ]
        );
    }

    /// Errors name the revisions involved and what to do
    #[test]
    fn error_messages() {
        // a malformed id shows the expected shape
        let invalid = UpgradeError::InvalidRevision("latest".to_owned()).to_string();
        assert!(invalid.contains("\"latest\"") && invalid.contains("2026-10-v02"));
        // an unknown id names the latest known revision
        let unknown = UpgradeError::UnknownRevision("2026-10-v00".to_owned()).to_string();
        assert!(unknown.contains("2026-10-v00") && unknown.contains(latest().as_str()));
        // a cluster ahead of the catalog is told downgrades aren't supported
        let ahead = UpgradeError::AheadOfCatalog {
            current: rev("2099-01-v01"),
            latest: latest(),
        }
        .to_string();
        assert!(ahead.contains("2099-01-v01") && ahead.contains("downgrades are not supported"));
        // a state in a newer layout names both versions and refuses the downgrade
        let schema = UpgradeError::SchemaAhead {
            found: 2,
            supported: 1,
        }
        .to_string();
        assert!(schema.contains("written by a newer Thorium operator"));
        assert!(
            schema.contains("schema version 2") && schema.contains("downgrades are not supported")
        );
        // a backwards target names both revisions
        let backwards = UpgradeError::Backwards {
            current: rev("2026-10-v02"),
            target: rev("2026-10-v01"),
        }
        .to_string();
        assert!(backwards.contains("2026-10-v01") && backwards.contains("2026-10-v02"));
    }

    /// Planned steps and upgrade progress serialize with plain names and fill defaults
    #[test]
    fn planned_step_serde_defaults() {
        // a planned step without its optional fields parses with defaults
        let step: PlannedStep = serde_json::from_value(serde_json::json!({
            "revision": "2026-10-v02",
            "step": "finalize",
            "kind": "Config",
            "description": "Record the new revision",
            "state": "Pending"
        }))
        .expect("planned step");
        assert_eq!(step.quiesce, Vec::new());
        assert_eq!(step.message, None);
        // states, kinds, and components serialize as their names
        let planned =
            plan(&UpgradeState::new(rev("2026-10-v01"), "1.8.1"), &latest()).expect("plan");
        let value = serde_json::to_value(&planned[2]).expect("serialize");
        assert_eq!(value["state"], "Pending");
        assert_eq!(value["kind"], "Data");
        assert_eq!(value["quiesce"], serde_json::json!(["SearchStreamer"]));
        // an empty progress parses with no steps
        let status: UpgradeStatus = serde_json::from_value(serde_json::json!({})).expect("status");
        assert_eq!(status, UpgradeStatus::default());
    }
}
