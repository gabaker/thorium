//! Revision-based upgrades of a Thorium deployment
//!
//! A revision (`YYYY-MM-vNN`) names a point in Thorium's deployment history that needs a known
//! set of steps to reach from the revision before it. The catalog of revisions and steps lives
//! here so the operator (which plans and runs upgrades) and thoradm (which runs data
//! migrations) work from the same data. Decisions are made only from revisions; the Thorium
//! version a revision was released with is advisory.

mod catalog;
mod plan;
mod registry;
mod reversibility;
mod revisions;
mod state;
mod status;
mod steps;
mod transforms;

pub use catalog::{
    CATALOG, Catalog, CatalogEntry, FLOORS, REINDEX_PROCEDURE, SupportFloor, entry, latest, step,
};
pub use plan::{
    ChainLink, PlanDirection, PlanItem, PlannedPass, UpgradePlan, plan, resolve_target,
};
pub use registry::{Enumeration, LayoutDef, SchemaBackend};
pub use reversibility::{
    EnumMapping, Fill, FillValue, PrivilegeOrder, Reversibility, VariantGrants,
};
pub use revisions::{REVISION_PATTERN, RevisionId, UpgradeError};
pub use state::{AppliedStep, IssueSummary, UpgradeState, label_value};
pub use status::{
    BackendSchemaStatus, BlockReason, ConfirmRefusal, DriftEntry, DriftKind, DriftSummary,
    FieldLoss, JobFailure, LossReport, PlannedStep, StepLoss, StepState, UpgradeApproval,
    UpgradeCondition, UpgradeConditionType, UpgradeStatus,
};
pub use steps::{
    DEFAULT_GROUP_MAX, Family, MigrationPhase, SchemaTarget, StepDef, StepKind, UpgradeComponent,
    Work,
};
pub use transforms::{
    FieldPath, IssueCategory, IssueReason, PathSegment, ReasonBuilder, RecordClass, TransformIssue,
};
#[cfg(feature = "scylla-utils")]
pub use transforms::{Record, TransformDef};

#[cfg(test)]
use catalog::HELM_STEPS;

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

/// The longest value a Kubernetes label may hold
const MAX_LABEL_LEN: usize = 63;

/// How many hex characters of the state hash are kept in [`HASH_LABEL`]
const HASH_LABEL_LEN: usize = 32;

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a revision id that must be valid
    ///
    /// # Arguments
    ///
    /// * `raw` - The id to parse
    fn rev(raw: &str) -> RevisionId {
        // parse the id, failing the test when it is malformed
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
                ..AppliedStep::default()
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
                ..AppliedStep::default()
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
            ..AppliedStep::default()
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
                ..AppliedStep::default()
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
        assert_eq!(bare.schema_version, 1);
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
        // build the entry without any details
        AppliedStep {
            revision: "2026-10-v02".to_owned(),
            step: step.to_owned(),
            outcome,
            message: None,
            at: None,
            by: None,
            ..AppliedStep::default()
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
        assert_eq!(step.pause, &[UpgradeComponent::SearchStreamer]);
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
        assert_eq!(step.pause, Vec::new());
        assert_eq!(step.message, None);
        // states, kinds, and components serialize as their names
        let planned =
            plan(&UpgradeState::new(rev("2026-10-v01"), "1.8.1"), &latest()).expect("plan");
        let value = serde_json::to_value(&planned[2]).expect("serialize");
        assert_eq!(value["state"], "Pending");
        assert_eq!(value["kind"], "Data");
        assert_eq!(value["pause"], serde_json::json!(["SearchStreamer"]));
        // an empty progress parses with no steps
        let status: UpgradeStatus = serde_json::from_value(serde_json::json!({})).expect("status");
        assert_eq!(status, UpgradeStatus::default());
    }
}
