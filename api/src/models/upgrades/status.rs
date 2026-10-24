//! The upgrade progress, approvals, conditions, drift, and losses shown in a `ThoriumCluster`

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use std::borrow::Cow;
use std::collections::BTreeMap;

use super::{MigrationPhase, SchemaBackend, StepKind, UpgradeComponent};

/// The state of a step in an upgrade plan
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum StepState {
    /// The step hasn't run yet
    #[default]
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
    /// A derived rebuild whose Job finished and whose reingest hasn't: the plan and the rollout go
    /// on, and the post-rollout check moves it to `Done` once the reingest finished and the probe
    /// sees the expected mappings
    Reingesting,
    /// The step is held without an approval: a backend it needs isn't up yet, or its check asked to
    /// be run again later, and the operator runs the check again when the wait lapses; nothing is
    /// recorded in the history
    Waiting,
    /// A value this build doesn't know, written by a newer operator
    #[serde(other)]
    Unknown,
}

impl StepState {
    /// Whether an entry in this state finishes its step: the step is done, had nothing to do, or
    /// is a derived rebuild whose reingest goes on after the plan (`Reingesting`)
    #[must_use]
    pub fn finishes(self) -> bool {
        // a rebuild that is reingesting is finished as far as the plan goes
        matches!(self, Self::Done | Self::Skipped | Self::Reingesting)
    }
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
    /// The components the operator pauses (scales to zero) before this step runs and restores
    /// after the new release's Deployments are applied
    #[serde(default)]
    pub pause: Vec<UpgradeComponent>,
    /// The state of this step
    pub state: StepState,
    /// Details about the state of this step
    #[serde(default)]
    pub message: Option<String>,
    /// Where this step sits in expand, migrate, contract
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<MigrationPhase>,
    /// Whether this step belongs to a pre-release edge
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prerelease: bool,
    /// Whether this step is on a downgrade path
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reverse: bool,
    /// The family of the fused pass this step runs in, when it runs in one
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass: Option<String>,
    /// What the step is waiting for, when it is `Blocked`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<BlockReason>,
    /// What this step will lose, estimated by its check before approval
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loss: Option<StepLoss>,
}

/// An approval for a step that needs one
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UpgradeApproval {
    /// The id of the approved step
    pub step: String,
    /// Where the backup taken before this step is (a path, snapshot name, or ticket)
    pub backup: String,
    /// Accept the loss a lossy downgrade step declares
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub accept_loss: bool,
    /// The confirmation code the blocked lossy step shows in the status, repeated to approve that
    /// one occurrence of the step; a code already recorded in the history never matches again
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<String>,
    /// Reason codes whose issue groups don't fail this step's run, taken from the check report or
    /// a failed run's status; adding or changing them on a failed step re-approves it, and they
    /// are consumed when the step runs
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accept_issues: Vec<String>,
    /// Let a lossy step run although a field it loses isn't covered by today's backups, as its
    /// loss report shows
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub accept_backup_gaps: bool,
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
    /// Each backend's schema status against the recorded revision
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub schema: BTreeMap<SchemaBackend, BackendSchemaStatus>,
    /// Drift counts and the first 20 unacknowledged objects; `thoradm schema diff` has the rest
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drift: Option<DriftSummary>,
    /// Conditions an admin should see, such as `SchemaDrift` and `MigrationIssues`
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<UpgradeCondition>,
    /// Approvals naming steps that aren't in the plan or are already done
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stale_approvals: Vec<String>,
    /// What a lossy downgrade will lose across its path, estimated before approval
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loss: Option<LossReport>,
    /// The last failed Job of the current step, for reading its logs
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<JobFailure>,
}

/// A condition on the upgrade, shown in `status.upgrade.conditions`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UpgradeCondition {
    /// What the condition is about
    #[serde(rename = "type")]
    pub kind: UpgradeConditionType,
    /// `"True"` while the condition holds, `"False"` once it cleared
    pub status: String,
    /// What an admin should do, such as the `thoradm` command to run
    pub message: String,
}

/// The conditions the operator reports on an upgrade
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum UpgradeConditionType {
    /// The live schema differs from the recorded revision (counts per kind)
    SchemaDrift,
    /// A migration finished with issues within its step's `group_max`, or accepted (the group and
    /// object counts, the five largest groups' codes, and `thoradm migrate issues`)
    MigrationIssues,
    /// The schema-apply Job reported a fingerprint different from the operator's
    SchemaSkew,
    /// The image's thoradm knows a different catalog (latest revision or hash) than the operator,
    /// so no data-changing step is planned until they agree
    CatalogSkew,
    /// `dangerous_downgrade_lossy` is set on a cluster that isn't dev, so it is ignored and lossy
    /// steps still need their own confirmation
    LossySettingRefused,
    /// A derived rebuild's reingest hasn't finished, so search results are incomplete (the sources
    /// still reingesting)
    SearchIncomplete,
    /// The cluster runs a mutable image tag (such as `latest`), so a new image pushed under the
    /// same tag doesn't change the schema-apply trigger key or run the Job
    MutableImageTag,
    /// A value this build doesn't know, written by a newer operator
    #[serde(other)]
    Unknown,
}

/// Drift as shown in `status.upgrade.drift`, capped so the status stays small
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DriftSummary {
    /// Unacknowledged `missing` objects and fields
    pub missing: u32,
    /// Unacknowledged `changed` objects
    pub changed: u32,
    /// Unacknowledged `unexpected` objects and fields
    pub unexpected: u32,
    /// Objects covered by an acknowledgement (the spec's or the catalog's)
    pub acknowledged: u32,
    /// Of those, the acknowledgements without a fingerprint, which never lapse
    #[serde(default)]
    pub unfingerprinted: u32,
    /// The first 20 unacknowledged objects, in `thoradm schema diff` order
    #[serde(default)]
    pub objects: Vec<DriftEntry>,
    /// How many unacknowledged objects aren't listed
    #[serde(default)]
    pub more: u32,
    /// The unlisted unacknowledged objects that block the steps targeting them, so a step is
    /// blocked however many objects drift; the operator keeps them out of the status
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<DriftEntry>,
    /// Whether more blocking objects were left out of `blockers` than it carries; every step with
    /// targets is then blocked, since the object it targets may be among them
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub blockers_truncated: bool,
    /// Each probed backend's status, which counts every unacknowledged object, listed or not
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub backends: BTreeMap<SchemaBackend, BackendSchemaStatus>,
}

/// One drifted object in the status
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DriftEntry {
    /// The backend the object belongs to
    pub backend: SchemaBackend,
    /// The object as `thoradm schema diff` names it
    pub object: String,
    /// How it differs
    pub kind: DriftKind,
    /// The observed fingerprint, to copy into an acknowledgement (absent for `missing`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// Whether an acknowledgement for it lapsed because the object changed
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lapsed: bool,
}

/// The kind of difference between the expected and the observed schema
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DriftKind {
    /// An expected object or field isn't there
    Missing,
    /// The object exists but differs (a column type, a key, a mapping type, an analyzer)
    Changed,
    /// An object or field the recorded revision doesn't know
    Unexpected,
    /// A value this build doesn't know, written by a newer operator
    #[serde(other)]
    Unknown,
}

/// A backend's schema status as shown in `status.upgrade.schema`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum BackendSchemaStatus {
    /// Every object matches the recorded revision (or only acknowledged objects differ)
    Ok,
    /// At least one unacknowledged object differs
    Drift,
    /// A value this build doesn't know, written by a newer operator
    #[serde(other)]
    Unknown,
}

/// A failed Job, copied into the status and the applied history
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct JobFailure {
    /// The Job's name
    pub job: String,
    /// The pod whose logs were read, if one existed
    pub pod: Option<String>,
    /// At most the last 20 log lines of that pod, in the status only; empty in history entries,
    /// so log lines never reach `schema_migrations` or backups
    #[serde(default)]
    pub log_tail: Vec<String>,
}

/// What a blocked step is waiting for
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum BlockReason {
    /// An approval naming a backup
    Approval,
    /// An approval with a backup, `accept_loss`, and this occurrence's confirmation code
    LossAcceptance {
        /// The confirmation code the approval must repeat as `confirm`
        confirm: String,
    },
    /// A lossy approval was given, but its code doesn't confirm this occurrence
    ConfirmationRefused {
        /// The current confirmation code, which a new approval must repeat
        confirm: String,
        /// Why the given code was refused
        refusal: ConfirmRefusal,
    },
    /// Objects the step targets have drifted
    Drift {
        /// The drifted objects
        objects: Vec<String>,
    },
    /// A value this build doesn't know, written by a newer operator; read through a custom
    /// `Deserialize`, since the other variants carry data
    Unknown,
}

/// The variants of [`BlockReason`] this build reads, in the same externally tagged form
#[derive(Deserialize)]
enum KnownBlockReason {
    /// An approval naming a backup
    Approval,
    /// An approval with a backup, `accept_loss`, and this occurrence's confirmation code
    LossAcceptance {
        /// The confirmation code the approval must repeat as `confirm`
        confirm: String,
    },
    /// A lossy approval was given, but its code doesn't confirm this occurrence
    ConfirmationRefused {
        /// The current confirmation code, which a new approval must repeat
        confirm: String,
        /// Why the given code was refused
        refusal: ConfirmRefusal,
    },
    /// Objects the step targets have drifted
    Drift {
        /// The drifted objects
        objects: Vec<String>,
    },
    /// A value a newer operator wrote that an earlier read already mapped to `Unknown`
    Unknown,
}

/// The names of the [`BlockReason`] variants this build knows
const KNOWN_BLOCK_REASONS: [&str; 5] = [
    "Approval",
    "LossAcceptance",
    "ConfirmationRefused",
    "Drift",
    "Unknown",
];

impl<'de> Deserialize<'de> for BlockReason {
    /// Read a block reason, mapping a variant this build doesn't know to `Unknown`
    ///
    /// # Arguments
    ///
    /// * `deserializer` - The deserializer to read from
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // read the value first so a variant this build doesn't know can be told apart
        let value = serde_json::Value::deserialize(deserializer)?;
        // a unit variant is a bare string and a data variant is an object with one key
        let name = match &value {
            serde_json::Value::String(name) => Some(name.as_str()),
            serde_json::Value::Object(map) if map.len() == 1 => {
                map.keys().next().map(String::as_str)
            }
            _ => None,
        };
        // a well formed variant this build doesn't know reads as unknown
        if name.is_some_and(|name| !KNOWN_BLOCK_REASONS.contains(&name)) {
            return Ok(BlockReason::Unknown);
        }
        // anything else must be a known variant in its usual form
        let known = KnownBlockReason::deserialize(value).map_err(D::Error::custom)?;
        // copy the known variant across
        Ok(match known {
            KnownBlockReason::Approval => BlockReason::Approval,
            KnownBlockReason::LossAcceptance { confirm } => BlockReason::LossAcceptance { confirm },
            KnownBlockReason::ConfirmationRefused { confirm, refusal } => {
                BlockReason::ConfirmationRefused { confirm, refusal }
            }
            KnownBlockReason::Drift { objects } => BlockReason::Drift { objects },
            KnownBlockReason::Unknown => BlockReason::Unknown,
        })
    }
}

impl JsonSchema for BlockReason {
    /// The name of this type's schema
    fn schema_name() -> Cow<'static, str> {
        // name the schema after the type
        Cow::Borrowed("BlockReason")
    }

    /// Describe a block reason as a value of any shape
    ///
    /// A unit variant is a string and a data variant is an object, and a Kubernetes structural
    /// schema can't give one field both types, so the field keeps whatever the operator writes.
    ///
    /// # Arguments
    ///
    /// * `_generator` - The generator building the schema
    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        // accept both forms and leave the shape to the custom deserializer
        json_schema!({
            "description": "What a blocked step is waiting for: Approval, LossAcceptance, \
                ConfirmationRefused, or Drift",
            "x-kubernetes-preserve-unknown-fields": true
        })
    }
}

/// Why a lossy approval's confirmation code was refused
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ConfirmRefusal {
    /// The approval has `accept_loss` but no `confirm`
    Missing,
    /// The code isn't the current one: it was shown before a step completed, or it is wrong
    Mismatch,
    /// The code is recorded as consumed: an earlier run of the step already used it
    Consumed,
    /// A field the step loses isn't covered by backups, and the approval lacks
    /// `accept_backup_gaps`
    BackupGap,
    /// A value this build doesn't know, written by a newer operator
    #[serde(other)]
    Unknown,
}

/// What a lossy downgrade will lose, across every lossy step on its path
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LossReport {
    /// Each lossy step's loss, in the order the steps run
    pub steps: Vec<StepLoss>,
}

/// What one lossy step will lose, or lost
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StepLoss {
    /// The revision the step belongs to
    pub revision: String,
    /// The step id
    pub step: String,
    /// The step's human-readable loss description
    pub description: String,
    /// The fields filled and how many objects each affects
    pub fields: Vec<FieldLoss>,
}

/// One field a lossy step fills
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FieldLoss {
    /// The column or field
    pub field: String,
    /// How many objects hold a value that will be filled
    pub objects: u64,
    /// Whether today's `thoradm backup` covers this field; a lossy step losing an uncovered field
    /// needs `accept_backup_gaps` in its approval
    #[serde(default)]
    pub backed_up: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Block reasons round trip, and a variant this build doesn't know reads as unknown
    #[test]
    fn block_reasons_read_unknown_variants() {
        // every known variant round trips through its externally tagged form
        for reason in [
            BlockReason::Approval,
            BlockReason::LossAcceptance {
                confirm: "ABCD1234".to_owned(),
            },
            BlockReason::ConfirmationRefused {
                confirm: "ABCD1234".to_owned(),
                refusal: ConfirmRefusal::Consumed,
            },
            BlockReason::Drift {
                objects: vec!["samples_list".to_owned()],
            },
            BlockReason::Unknown,
        ] {
            let value = serde_json::to_value(&reason).expect("serialize");
            assert_eq!(
                serde_json::from_value::<BlockReason>(value).expect("deserialize"),
                reason
            );
        }
        // unknown unit and data variants read as unknown
        for unknown in [
            serde_json::json!("FromTheFuture"),
            serde_json::json!({"FromTheFuture": {"x": 1}}),
        ] {
            assert_eq!(
                serde_json::from_value::<BlockReason>(unknown).expect("unknown"),
                BlockReason::Unknown
            );
        }
        // a known variant in the wrong shape is still an error
        assert!(serde_json::from_value::<BlockReason>(serde_json::json!({"Drift": 3})).is_err());
        assert!(serde_json::from_value::<BlockReason>(serde_json::json!(3)).is_err());
        // a refusal this build doesn't know reads as unknown too
        assert_eq!(
            serde_json::from_value::<ConfirmRefusal>(serde_json::json!("Later")).expect("refusal"),
            ConfirmRefusal::Unknown
        );
    }
}
