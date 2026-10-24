//! Tests of how a derived rebuild sits in a plan and in the history: several rebuilds become one,
//! and a `Reingesting` entry finishes its step without being done

use super::fixtures::{BASE, NEXT, RELEASE};
use super::plan_fixtures::ELASTIC_REBUILDS;
use super::{
    AppliedStep, Catalog, PlanItem, RESET_STATE_STEP, RevisionId, StepState, UpgradeState,
};
use crate::models::elastic::ElasticIndex;
use crate::models::schema::{ElasticIndexDef, RebuildTarget, RevisionSnapshot};
use crate::models::upgrades::SchemaBackend;

/// Parse a revision id that must be valid
///
/// # Arguments
///
/// * `raw` - The id to parse
fn rev(raw: &str) -> RevisionId {
    // parse the id, failing the test when it is malformed
    raw.parse().expect("valid revision")
}

/// Build a history entry
///
/// # Arguments
///
/// * `revision` - The revision of the step
/// * `step` - The step's id
/// * `outcome` - How it ended
fn entry(revision: &str, step: &str, outcome: StepState) -> AppliedStep {
    // only what the entry's identity needs
    AppliedStep {
        revision: revision.to_owned(),
        step: step.to_owned(),
        outcome,
        ..AppliedStep::default()
    }
}

/// Two rebuilds in one plan become one: the later one runs and the earlier is reported dropped, and
/// the one that runs drops what both would, with the mappings the target revision declares
#[test]
fn two_rebuilds_in_a_plan_become_one_with_the_targets_mappings() {
    // a deployment at the base and a plan to the second edge's revision
    let state = ELASTIC_REBUILDS.reached(&rev(BASE), "0.0.0");
    let plan = ELASTIC_REBUILDS
        .compose(&state, &rev(NEXT))
        .expect("compose");
    // the plan holds one rebuild, the later one
    let steps = plan
        .items
        .iter()
        .map(|item| match item {
            PlanItem::Step(step) => step.step.clone(),
            other => panic!("not a single step: {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(steps, ["elastic-rebuild-2"]);
    // the earlier one is dropped in favor of it
    let dropped = ELASTIC_REBUILDS
        .superseded(&state, &rev(NEXT))
        .expect("superseded")
        .into_iter()
        .map(|(step, by)| (step.step, by))
        .collect::<Vec<_>>();
    assert_eq!(
        dropped,
        [("elastic-rebuild-1".to_owned(), "elastic-rebuild-2")]
    );
    // both name the Elastic rebuild and pause only the search-streamer
    for edge in ELASTIC_REBUILDS.edges {
        for step in edge.steps {
            assert_eq!(step.thoradm_migration, Some("elastic-rebuild"));
            assert_eq!(step.pause.len(), 1);
        }
    }
    // the rebuild that runs checks against the target revision's mappings: the latest committed
    // snapshot holds exactly what this build declares for every index
    let snapshot = RevisionSnapshot::committed(super::latest().as_str())
        .expect("a committed snapshot")
        .expect("a readable snapshot");
    for index in ElasticIndex::ALL {
        let declared = ElasticIndexDef::of(&index);
        let committed = snapshot
            .objects
            .iter()
            .find(|object| object.backend == SchemaBackend::Elastic && object.name == declared.name)
            .expect("the snapshot holds the index");
        assert_eq!(committed.fingerprint, declared.fingerprint());
    }
    // merging the targets of the two rebuilds drops each index once
    let merged = RebuildTarget::of(["SampleResults"]).merge(&RebuildTarget::of(["SampleResults"]));
    assert_eq!(merged.indexes.len(), 1);
}

/// A `Reingesting` entry finishes its step for the plan, and a later `Done` entry for the same step
/// ends the reingest
#[test]
fn a_reingesting_step_is_finished_but_listed_until_it_is_done() {
    // the rebuild's Job finished: the step counts as finished and is listed as reingesting
    let catalog: &Catalog = &ELASTIC_REBUILDS;
    let mut state = catalog.reached(&rev(BASE), "0.0.0");
    state.record(entry(RELEASE, "elastic-rebuild-1", StepState::Running));
    assert!(!catalog.finished(&state, RELEASE, "elastic-rebuild-1"));
    assert_eq!(catalog.reingesting(&state).len(), 0);
    state.record(entry(RELEASE, "elastic-rebuild-1", StepState::Reingesting));
    assert!(catalog.finished(&state, RELEASE, "elastic-rebuild-1"));
    let listed = catalog.reingesting(&state);
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].step, "elastic-rebuild-1");
    // the plan to the next revision doesn't run it again
    let plan = catalog.compose(&state, &rev(RELEASE)).expect("compose");
    assert!(plan.items.is_empty(), "{plan:?}");
    // the reingest finishing ends the listing, and the step stays finished
    state.record(entry(RELEASE, "elastic-rebuild-1", StepState::Done));
    assert_eq!(catalog.reingesting(&state).len(), 0);
    assert!(catalog.finished(&state, RELEASE, "elastic-rebuild-1"));
}

/// A reset below the rebuild's revision undoes the reingest too, so the step runs again and isn't
/// listed
#[test]
fn a_reset_ends_the_reingest_listing() {
    // a rebuild that is reingesting, then a reset to the base
    let catalog: &Catalog = &ELASTIC_REBUILDS;
    let mut state: UpgradeState = catalog.reached(&rev(BASE), "0.0.0");
    state.record(entry(RELEASE, "elastic-rebuild-1", StepState::Reingesting));
    assert_eq!(catalog.reingesting(&state).len(), 1);
    state.record(entry(BASE, RESET_STATE_STEP, StepState::Done));
    assert!(!catalog.finished(&state, RELEASE, "elastic-rebuild-1"));
    assert_eq!(catalog.reingesting(&state).len(), 0);
}

/// Only a finished state finishes a step: `Reingesting` does, `Blocked`, `Failed`, and `Running` don't
#[test]
fn only_finishing_states_finish_a_step() {
    // done, skipped, and reingesting finish
    assert!(StepState::Done.finishes());
    assert!(StepState::Skipped.finishes());
    assert!(StepState::Reingesting.finishes());
    // everything else leaves the step to run
    for state in [
        StepState::Pending,
        StepState::Running,
        StepState::Blocked,
        StepState::Failed,
        StepState::Waiting,
        StepState::Unknown,
    ] {
        assert!(!state.finishes(), "{state:?}");
    }
}
