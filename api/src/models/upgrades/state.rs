//! The upgrade state recorded for a deployment and the steps in its history

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

use super::{
    CATALOG, HASH_LABEL, HASH_LABEL_LEN, JobFailure, MAX_HISTORY, MAX_LABEL_LEN, REVISION_LABEL,
    RevisionId, STATE_SCHEMA_VERSION, StepLoss, StepState, UpgradeError, VERSION_LABEL, entry,
};

/// A step recorded in a cluster's upgrade history
///
/// Finished steps are recorded as `Done` or `Skipped`. A step that blocks on an admin is also
/// recorded once as `Blocked`, which doesn't finish it, so a step later found to have nothing to
/// do can be told apart as fixed manually.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    /// The approval's backup reference
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
    /// On a lossy step's `Blocked` entry, the confirmation code computed when it blocked (from the
    /// history length at that moment), which approvals are compared with; on its outcome entry, the
    /// code the approval repeated, whose recording consumes it, so it never matches again
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<String>,
    /// The work the step found: for a thoradm migration, the class counts of its run's first pass;
    /// for an operator executor, what its check found right before it ran
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work: Option<String>,
    /// The Job that ran the step
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    /// The step ids of the fused pass this step ran in, in order
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chain: Vec<String>,
    /// The issue groups the run recorded and the codes its approval accepted
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issues: Option<IssueSummary>,
    /// What a lossy step lost, with its fields and object counts
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loss: Option<StepLoss>,
    /// The failed Job's name and pod, without log lines
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<JobFailure>,
    /// The replica count of each Deployment this step paused, by Deployment name, recorded on its
    /// `Running` entry and read when the new release's Deployments are applied
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub paused: BTreeMap<String, i32>,
}

/// What a migration run left as issues, recorded on its history entry
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IssueSummary {
    /// How many issue groups the run recorded
    pub groups: u32,
    /// How many objects those groups hold
    pub objects: u64,
    /// The reason codes the approval accepted, consumed by this run
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accepted: Vec<String>,
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

/// The first [`UpgradeState`] layout version, used for state written before it was recorded
const FIRST_SCHEMA_VERSION: u32 = 1;

/// Serde helper for the layout version of state written before it was recorded
fn default_schema_version() -> u32 {
    // state without a recorded layout version predates the field, so it is the first layout
    FIRST_SCHEMA_VERSION
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
        // a step is finished once any history entry for it is done or skipped
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
