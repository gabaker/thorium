//! The upgrade state recorded for a deployment and the steps in its history

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

use super::plan::has_transform;
use super::{
    Catalog, CatalogEntry, HASH_LABEL, HASH_LABEL_LEN, JobFailure, MAX_HISTORY, MAX_LABEL_LEN,
    REVISION_LABEL, RevisionId, STATE_SCHEMA_VERSION, StepDef, StepLoss, StepState, UpgradeError,
    VERSION_LABEL, entry,
};

/// The step of the history entry `thoradm migrate reset-state` writes; its revision is the one
/// the deployment was reset to, so every step above it counts as unfinished again
pub const RESET_STATE_STEP: &str = "reset-state";

/// The step of the history entry `thoradm restore` appends; its revision is the backup's, so
/// every step above it counts as unfinished again
pub const RESTORED_STEP: &str = "restored";

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
    /// The fingerprints last applied and observed, as `{backend: fingerprint}` (the deployment
    /// fingerprint under [`DEPLOYMENT_MARKER`]), and under [`APPLY_KEY_MARKER`] the schema-apply
    /// trigger key the last successful apply ran for, under [`HELD_FAILURE_MARKER`] the failed
    /// schema-apply attempt holding the rollout, under [`RETRY_VALUE_MARKER`] the last retry
    /// annotation value a schema-apply attempt used, and under [`RECORD_MARKER`] that the state
    /// mirrors the record in `schema_migrations`
    #[serde(default)]
    pub schema: BTreeMap<String, String>,
}

/// The key in [`UpgradeState::schema`] holding the schema-apply trigger key the last successful
/// apply ran for: the image reference, the revision, and the hashes of the data-shaping and table
/// TTL settings
pub const APPLY_KEY_MARKER: &str = "apply_key";

/// The key in [`UpgradeState::schema`] holding the deployment fingerprint the last successful
/// schema-apply Job reported
pub const DEPLOYMENT_MARKER: &str = "deployment";

/// The key in [`UpgradeState::schema`] holding the failed schema-apply attempt that holds the
/// rollout, as JSON with its trigger key, attempt counter, Job name, and failure reason, so it
/// keeps holding after its Job is deleted
pub const HELD_FAILURE_MARKER: &str = "held_failure";

/// The key in [`UpgradeState::schema`] holding the last value of the cluster's retry-schema-apply
/// annotation that a finished schema-apply attempt used, so that value never forces another
pub const RETRY_VALUE_MARKER: &str = "retry_value";

/// The key in [`UpgradeState::schema`] marking a state as a mirror of the record in
/// `schema_migrations`: set whenever the operator rebuilds the state from a `thoradm schema status`
/// result, so a state without it is one no record is known for (written before the record existed,
/// or recreated by hand)
pub const RECORD_MARKER: &str = "record";

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

    /// Whether a step of this build's catalog finished in this deployment's history
    ///
    /// See [`Catalog::finished`] for when a step counts as finished.
    ///
    /// # Arguments
    ///
    /// * `revision` - The revision the step belongs to
    /// * `step` - The id of the step
    #[must_use]
    pub fn finished(&self, revision: &str, step: &str) -> bool {
        // judge the step against the catalog this build deploys
        Catalog::PRODUCTION.finished(self, revision, step)
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
        // walk the path through the catalog this build deploys
        Catalog::PRODUCTION.advance(self, target, version)
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

impl Catalog {
    /// Whether a step finished in a deployment's history
    ///
    /// A forward step is finished only when its `Done` or `Skipped` entry is newer than the
    /// latest entry that lowered the revision below the step's revision: a `reset-state` or
    /// `restored` entry, or a finished step of a downward edge that reverses no particular step
    /// (the edge's record), each lowering to its own revision. A finished step that reverses a
    /// forward step lowers only that step. So a step reverted, reset, or restored since its `Done`
    /// runs again. A step of a downward edge is finished only when its entry is newer than the
    /// latest finished forward step that raised the deployment above the edge's revision.
    ///
    /// # Arguments
    ///
    /// * `state` - The deployment's recorded upgrade state
    /// * `revision` - The revision the step belongs to
    /// * `step` - The id of the step
    #[must_use]
    pub fn finished(&self, state: &UpgradeState, revision: &str, step: &str) -> bool {
        // find the newest entry finishing this step
        let Some(done) = state.applied.iter().rposition(|applied| {
            applied.revision == revision && applied.step == step && is_finished(applied)
        }) else {
            return false;
        };
        // a step whose revision doesn't parse can't be lowered past
        let Ok(own) = revision.parse::<RevisionId>() else {
            return true;
        };
        // a step of a downward edge counts until something raises the deployment past it again
        let downward = self.step(step).is_some_and(|(edge, _)| edge.is_downward());
        let undone = state.applied[done + 1..].iter().any(|later| {
            if downward {
                self.raises_above(later, &own)
            } else {
                self.lowers_below(later, &own, step)
            }
        });
        !undone
    }

    /// The steps whose latest finishing entry is `Reingesting`: a derived rebuild whose Job
    /// finished and whose reingest hasn't been seen to finish
    ///
    /// A step a reset, restore, or revert has undone since isn't listed.
    ///
    /// # Arguments
    ///
    /// * `state` - The deployment's recorded upgrade state
    #[must_use]
    pub fn reingesting<'a>(&self, state: &'a UpgradeState) -> Vec<&'a AppliedStep> {
        // keep the latest finishing entry of each step, in the order they were recorded
        let mut latest: Vec<&AppliedStep> = Vec::new();
        for entry in &state.applied {
            if is_finished(entry) {
                latest.retain(|kept| !(kept.revision == entry.revision && kept.step == entry.step));
                latest.push(entry);
            }
        }
        // those that are still reingesting, unless something undid the step since
        latest
            .into_iter()
            .filter(|entry| {
                entry.outcome == StepState::Reingesting
                    && self.finished(state, &entry.revision, &entry.step)
            })
            .collect()
    }

    /// Whether a history entry lowered the deployment below a forward step's revision, or undid
    /// that step on its own
    ///
    /// # Arguments
    ///
    /// * `applied` - The history entry
    /// * `own` - The revision of the forward step
    /// * `step` - The id of the forward step
    fn lowers_below(&self, applied: &AppliedStep, own: &RevisionId, step: &str) -> bool {
        // only finished entries move the revision
        if !is_finished(applied) {
            return false;
        }
        // work out which revision the entry lowered the deployment to, if it lowered it
        let catalog_step = self.step(&applied.step);
        let lowered = match catalog_step {
            // a reset or restore lowers to the revision it records
            _ if applied.step == RESET_STATE_STEP || applied.step == RESTORED_STEP => true,
            // a step reversing one forward step undoes only that step
            Some((edge, def)) if edge.is_downward() && def.reverses.is_some() => {
                return def.reverses == Some(step);
            }
            // the record of a downward edge lowers to the edge's revision
            Some((edge, _)) => edge.is_downward(),
            // anything else this catalog doesn't know moves nothing
            None => false,
        };
        // a lowering entry undoes the step unless the deployment stayed at or above its revision
        lowered
            && applied
                .revision
                .parse::<RevisionId>()
                .is_ok_and(|to| !self.at_or_below(own, &to))
    }

    /// Whether a history entry is a finished forward step that raised the deployment above a
    /// downward step's revision
    ///
    /// # Arguments
    ///
    /// * `applied` - The history entry
    /// * `own` - The revision of the downward step
    fn raises_above(&self, applied: &AppliedStep, own: &RevisionId) -> bool {
        // only finished forward steps of this catalog raise the revision
        is_finished(applied)
            && self
                .step(&applied.step)
                .is_some_and(|(edge, _)| !edge.is_downward())
            && applied
                .revision
                .parse::<RevisionId>()
                .is_ok_and(|to| to != *own && self.at_or_below(own, &to))
    }

    /// Move a deployment's recorded revision along the path to a target, past every edge whose
    /// steps all finished
    ///
    /// Returns whether the revision moved.
    ///
    /// # Arguments
    ///
    /// * `state` - The deployment's recorded upgrade state
    /// * `target` - The revision to move towards
    /// * `version` - The Thorium version recording the new revision
    pub fn advance(&self, state: &mut UpgradeState, target: &RevisionId, version: &str) -> bool {
        // without a path there is nothing to move along
        let Ok(path) = self.path(&state.revision, target) else {
            return false;
        };
        // move past each edge in order, stopping at the first with work left
        let mut moved = false;
        for edge in path {
            // stop at the first edge with a step still to run
            if !self.edge_steps(state, edge).is_empty() {
                break;
            }
            // every step of this edge finished so record the revision it reaches
            state.revision = edge.revision();
            state.applied_by_version = Some(version.to_owned());
            moved = true;
        }
        moved
    }

    /// Get the steps of an edge a deployment still has to run
    ///
    /// On a forward edge these are its unfinished steps. On a downward edge a step reversing a
    /// forward step runs only while that forward step counts as finished, and any other step runs
    /// while it is unfinished itself.
    ///
    /// On a downward edge the steps reversing a transform run in the reverse order of the forward
    /// steps they undo, in the places the edge lists such steps, so transforms of one family
    /// revert newest first whichever order the edge declares them in. Every other step, such as a
    /// rebuild run from the reverted data, stays where the edge lists it.
    ///
    /// # Arguments
    ///
    /// * `state` - The deployment's recorded upgrade state
    /// * `edge` - The edge to check
    #[must_use]
    pub fn edge_steps(
        &self,
        state: &UpgradeState,
        edge: &'static CatalogEntry,
    ) -> Vec<&'static StepDef> {
        // keep every step that still has work to do
        let mut steps = edge
            .steps
            .iter()
            .filter(|step| match step.reverses {
                // a revert step is needed only while the step it reverses counts as finished
                Some(forward) if edge.is_downward() => {
                    self.step(forward).is_some_and(|(forward_edge, _)| {
                        self.finished(state, forward_edge.id, forward)
                    })
                }
                // any other step is needed until it finished
                _ => !self.finished(state, edge.id, step.id),
            })
            .collect::<Vec<_>>();
        // a forward edge runs its steps in the order it lists them
        if !edge.is_downward() {
            return steps;
        }
        // find the places of the reverses of transforms and the forward steps they undo
        let (places, mut reverses): (Vec<usize>, Vec<_>) = steps
            .iter()
            .enumerate()
            .filter_map(|(place, step)| {
                self.transform_position(step)
                    .map(|position| (place, (position, *step)))
            })
            .unzip();
        // put them back in those places, the reverse of the newest forward step first
        reverses.sort_by_key(|(position, _)| std::cmp::Reverse(*position));
        for (place, (_, step)) in places.into_iter().zip(reverses) {
            steps[place] = step;
        }
        steps
    }

    /// Find where the forward transform a reverse step undoes sits in the catalog: its edge's
    /// index and its index on that edge, which order the forward steps from oldest to newest
    ///
    /// Returns `None` for a step reversing no forward step, or one reversing a step without a
    /// transform.
    ///
    /// # Arguments
    ///
    /// * `step` - The reverse step
    fn transform_position(&self, step: &StepDef) -> Option<(usize, usize)> {
        // look the reversed step up on every forward edge
        let forward = step.reverses?;
        self.edges
            .iter()
            .enumerate()
            .filter(|(_, edge)| !edge.is_downward())
            .find_map(|(index, edge)| {
                edge.steps
                    .iter()
                    .position(|candidate| candidate.id == forward && has_transform(candidate))
                    .map(|place| (index, place))
            })
    }
}

/// Whether a history entry finished its step (`Done`, `Skipped`, or `Reingesting`)
///
/// # Arguments
///
/// * `applied` - The history entry
fn is_finished(applied: &AppliedStep) -> bool {
    // done, skipped, and reingesting entries finish a step
    applied.outcome.finishes()
}
