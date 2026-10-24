//! Tests of the deployment record's pure parts: history ids, the finished-step set, the catalog's
//! identity, and which differences refuse a seed

use serde_json::json;
use std::collections::BTreeMap;

use super::record::{copyable, ends_reingest, picked, seed_differences};
use super::{
    BackendDrift, DriftReport, Fingerprint, FinishedStep, ObjectDrift, catalog_identity,
    finished_steps, history_id,
};
use crate::models::upgrades::fixtures::{
    BASE, DATA_BLANK_PROCEDURE, ONE_EDGE, PROCEDURE_STEP, RELEASE, SKIP,
};
use crate::models::upgrades::{
    AppliedStep, BackendSchemaStatus, DriftKind, RESET_STATE_STEP, RevisionId, SchemaBackend,
    StepState,
};

/// Build a history entry
///
/// # Arguments
///
/// * `revision` - The revision the step belongs to
/// * `step` - The step's id
/// * `outcome` - How the step ended
/// * `at` - When it ended, if known
fn entry(revision: &str, step: &str, outcome: StepState, at: Option<&str>) -> AppliedStep {
    // fill only the fields the id and the finished rule read
    AppliedStep {
        revision: revision.to_owned(),
        step: step.to_owned(),
        outcome,
        at: at.map(ToOwned::to_owned),
        ..AppliedStep::default()
    }
}

/// A history id is `<at>/<revision>/<step>/<outcome>` with `at` in RFC 3339 with nanoseconds
#[test]
fn a_history_id_is_the_time_revision_step_and_outcome() {
    // an entry recorded at a whole second gets nine fractional digits
    let whole = entry(
        "2026-10-v02",
        "schema-apply",
        StepState::Done,
        Some("2026-10-22T11:43:39Z"),
    );
    assert_eq!(
        history_id(&whole).expect("an id"),
        "2026-10-22T11:43:39.000000000Z/2026-10-v02/schema-apply/Done"
    );
    // a fraction is kept to the nanosecond
    let fraction = entry(
        "2026-10-v02",
        "schema-apply",
        StepState::Running,
        Some("2026-10-22T11:43:39.5Z"),
    );
    assert_eq!(
        history_id(&fraction).expect("an id"),
        "2026-10-22T11:43:39.500000000Z/2026-10-v02/schema-apply/Running"
    );
    // an offset is read as the same instant in UTC
    let offset = entry(
        "2026-10-v02",
        "schema-apply",
        StepState::Done,
        Some("2026-10-22T13:43:39+02:00"),
    );
    assert_eq!(
        history_id(&offset).expect("an id"),
        history_id(&whole).expect("an id")
    );
}

/// Ids sort oldest first whatever precision the entries were written with, so a history read in
/// key order is in time order
#[test]
fn history_ids_sort_in_time_order() {
    // three instants written with different precision, out of order
    let mut ids = [
        "2026-10-22T11:43:39.000000001Z",
        "2026-10-22T11:43:39Z",
        "2026-10-22T11:43:38.9Z",
    ]
    .map(|at| history_id(&entry("2026-10-v02", "a", StepState::Done, Some(at))).expect("an id"));
    ids.sort();
    // the earliest comes first
    assert!(
        ids[0].starts_with("2026-10-22T11:43:38.900000000Z"),
        "{ids:?}"
    );
    assert!(
        ids[1].starts_with("2026-10-22T11:43:39.000000000Z"),
        "{ids:?}"
    );
    assert!(
        ids[2].starts_with("2026-10-22T11:43:39.000000001Z"),
        "{ids:?}"
    );
}

/// An entry with no time, or one that isn't a time, has no id
#[test]
fn an_entry_without_a_time_has_no_id() {
    // no time at all, and text that isn't a time
    for at in [None, Some("yesterday"), Some("")] {
        let missing = entry("2026-10-v02", "a", StepState::Done, at);
        assert!(history_id(&missing).is_err(), "{at:?}");
    }
}

/// A step recorded `Done`, followed by 250 more entries, is in the finished set computed over the
/// whole history, although the newest 200 entries don't hold it
#[test]
fn a_done_step_beyond_the_window_is_finished() {
    // a finished step, then 250 notes of a blocked step
    let mut history = vec![entry(
        RELEASE,
        "release-step",
        StepState::Done,
        Some("2026-10-22T00:00:00Z"),
    )];
    history.extend((0..250).map(|index| {
        entry(
            RELEASE,
            "other-step",
            StepState::Blocked,
            Some(&format!(
                "2026-10-22T01:{:02}:{:02}Z",
                index / 60,
                index % 60
            )),
        )
    }));
    // the first entry is outside the newest 200
    assert!(
        !history[history.len() - 200..]
            .iter()
            .any(|applied| applied.step == "release-step")
    );
    // the whole history still counts the step as finished
    let finished = finished_steps(&history, &ONE_EDGE);
    assert_eq!(
        finished,
        [FinishedStep {
            revision: RELEASE.parse().expect("a revision"),
            step: "release-step".to_owned(),
        }]
    );
}

/// Only `Done` and `Skipped` entries finish a step; a `Blocked` note or a `Running` entry doesn't
#[test]
fn only_done_and_skipped_entries_finish_a_step() {
    // each outcome on its own
    for (outcome, finished) in [
        (StepState::Done, true),
        (StepState::Skipped, true),
        (StepState::Blocked, false),
        (StepState::Running, false),
        (StepState::Failed, false),
    ] {
        let history = [entry(
            RELEASE,
            "release-step",
            outcome,
            Some("2026-10-22T00:00:00Z"),
        )];
        assert_eq!(
            finished_steps(&history, &ONE_EDGE).len(),
            usize::from(finished),
            "{outcome:?}"
        );
    }
}

/// A reset to an older revision after a `Done` entry makes the step unfinished again, by the same
/// rule the planner uses
#[test]
fn a_reset_after_a_done_entry_unfinishes_the_step() {
    // the step is done, then the deployment is reset to the base
    let history = [
        entry(
            RELEASE,
            "release-step",
            StepState::Done,
            Some("2026-10-22T00:00:00Z"),
        ),
        entry(
            BASE,
            RESET_STATE_STEP,
            StepState::Done,
            Some("2026-10-22T01:00:00Z"),
        ),
    ];
    assert_eq!(finished_steps(&history, &ONE_EDGE), []);
}

/// A step the catalog doesn't hold is never in the finished set, so the set stays as small as the
/// catalog however long the history is
#[test]
fn steps_outside_the_catalog_are_not_finished() {
    // a finished entry for a step no edge holds
    let history = [entry(
        RELEASE,
        "not-in-the-catalog",
        StepState::Done,
        Some("2026-10-22T00:00:00Z"),
    )];
    assert_eq!(finished_steps(&history, &ONE_EDGE), []);
}

/// The identity names the catalog's newest revision and hashes its edges and steps: the same
/// catalog gives the same hash, and another gives another
#[test]
fn the_catalog_identity_names_its_latest_revision_and_hashes_its_steps() {
    // one edge and a catalog with a skip edge past it
    let one = catalog_identity(&ONE_EDGE);
    let skip = catalog_identity(&SKIP);
    assert_eq!(
        one.latest,
        RELEASE.parse::<RevisionId>().expect("a revision")
    );
    assert_eq!(skip.latest, ONE_EDGE.latest().max(SKIP.latest()));
    // the hash is a sha256 in hex, stable, and different for another catalog
    assert_eq!(one.hash.as_str().len(), 64, "{one:?}");
    assert!(one.hash.as_str().chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(one, catalog_identity(&ONE_EDGE));
    assert_ne!(one.hash, skip.hash);
}

/// Build a drift entry for an object
///
/// # Arguments
///
/// * `object` - The object, or `object.field`
/// * `kind` - How it differs
fn drift(object: &str, kind: DriftKind) -> ObjectDrift {
    // an unacknowledged difference with no field detail
    ObjectDrift {
        backend: SchemaBackend::Scylla,
        object: object.to_owned(),
        kind,
        fields: Vec::new(),
        acknowledged: false,
        lapsed: false,
        fingerprint: None,
    }
}

/// Build a drift report holding entries for Scylla
///
/// # Arguments
///
/// * `objects` - The differences
fn report(objects: Vec<ObjectDrift>) -> DriftReport {
    // the fingerprints and revision don't matter to which differences refuse a seed
    let print = Fingerprint::of(&json!("fixture"));
    DriftReport {
        revision: BASE.parse().expect("a revision"),
        expected: print.clone(),
        observed: print,
        backends: BTreeMap::from([(
            SchemaBackend::Scylla,
            BackendDrift {
                status: BackendSchemaStatus::Drift,
                objects,
            },
        )]),
    }
}

/// Objects and columns the apply would add don't refuse a seed, and unexpected extras don't
/// either
#[test]
fn what_apply_would_add_does_not_refuse_a_seed() {
    // a declared table missing, a declared column missing, and an extra object
    let differences = seed_differences(&report(vec![
        drift("samples", DriftKind::Missing),
        drift("results.entities", DriftKind::Missing),
        drift("associations.submissions_source", DriftKind::Missing),
        drift("something_extra", DriftKind::Unexpected),
        drift("something_extra.column", DriftKind::Unexpected),
    ]));
    assert!(differences.is_empty(), "{differences:?}");
}

/// A changed object, a missing object the registry doesn't declare, and a missing key column
/// refuse a seed, each named
#[test]
fn a_changed_or_unaddable_object_refuses_a_seed() {
    // a column with another type, an undeclared object, a missing key column, a changed view, and
    // a column missing from a view, which is never altered
    let differences = seed_differences(&report(vec![
        drift("nodes.health", DriftKind::Changed),
        drift("ghost", DriftKind::Missing),
        drift("samples_list.group", DriftKind::Missing),
        drift("entities_by_id", DriftKind::Changed),
        drift("samples.sha256", DriftKind::Missing),
    ]));
    assert_eq!(differences.len(), 5, "{differences:?}");
    for named in [
        "nodes.health",
        "ghost",
        "samples_list.group",
        "entities_by_id",
        "samples.sha256",
    ] {
        assert!(
            differences
                .iter()
                .any(|difference| difference.starts_with(named)),
            "{named} isn't named in {differences:?}"
        );
    }
}

/// The entries only `thoradm migrate reset-state` and `thoradm restore` write, which lower the
/// revision, are never copied from the state; an ordinary entry is
#[test]
fn r7b_reset_and_restore_entries_are_never_copied() {
    // each lowering step, and one ordinary step
    for step in [RESET_STATE_STEP, crate::models::upgrades::RESTORED_STEP] {
        let lowering = entry(BASE, step, StepState::Done, Some("2026-10-22T00:00:00Z"));
        assert!(!copyable(&ONE_EDGE, &lowering), "{step}");
    }
    let ordinary = entry(
        RELEASE,
        "release-step",
        StepState::Skipped,
        Some("2026-10-22T00:00:00Z"),
    );
    assert!(copyable(&ONE_EDGE, &ordinary));
}

/// A gated step's `Done` entry is written only by its own run, except the one that ends a derived
/// rebuild's reingest: its own run ends `Reingesting`, so the operator's later `Done` is copied
#[test]
fn only_the_done_that_ends_a_reingest_is_copied_for_a_gated_step() {
    // the rebuild's run ended `Reingesting`, and the operator recorded `Done` when the streamer finished
    let at = Some("2026-10-22T00:00:00Z");
    let reingesting = entry(RELEASE, "elastic-rebuild-1", StepState::Reingesting, at);
    let done = entry(RELEASE, "elastic-rebuild-1", StepState::Done, at);
    let history = [reingesting.clone(), done.clone()];
    // the `Done` after the `Reingesting` is copied; the same `Done` alone is not; neither is another
    // step's
    assert!(ends_reingest(&history, 1));
    assert!(!ends_reingest(&history, 0));
    assert!(!ends_reingest(&[done], 0));
    let other = entry(RELEASE, "release-step", StepState::Done, at);
    assert!(!ends_reingest(&[reingesting, other], 1));
}

/// The operator's `Done` of a gated step it ran in a Job is copied once the history holds the
/// step's `Running` entry, so the revision can be recorded, and only while the record holds no
/// `Done` of the step; a `Done` with no `Running` before it is never copied
#[test]
fn the_operators_done_of_a_run_it_started_is_copied_once() {
    // the operator started the gated step, then recorded its `Done`
    let at = Some("2026-10-22T00:00:00Z");
    let later = Some("2026-10-22T00:05:00Z");
    let running = entry(RELEASE, PROCEDURE_STEP, StepState::Running, at);
    let done = entry(RELEASE, PROCEDURE_STEP, StepState::Done, later);
    let history = [running.clone(), done.clone()];
    // the record holds the `Running` the Job wrote and no `Done`: the `Done` is copied
    let copied = picked(
        &DATA_BLANK_PROCEDURE,
        &history,
        std::slice::from_ref(&running),
    );
    assert!(copied.contains(&done), "{copied:?}");
    // a `Done` already in the record, written by a run by hand, is the one `Done` of the step
    let by_hand = entry(
        RELEASE,
        PROCEDURE_STEP,
        StepState::Done,
        Some("2026-10-22T00:04:00Z"),
    );
    let recorded = [running.clone(), by_hand];
    let copied = picked(&DATA_BLANK_PROCEDURE, &history, &recorded);
    assert!(!copied.contains(&done), "{copied:?}");
    // a `Done` with no `Running` before it is not the end of a run
    let copied = picked(&DATA_BLANK_PROCEDURE, std::slice::from_ref(&done), &[]);
    assert!(copied.is_empty(), "{copied:?}");
    // the `Running` of another step doesn't start this one's run
    let other = entry(RELEASE, "release-step", StepState::Running, at);
    let copied = picked(&DATA_BLANK_PROCEDURE, &[other, done], &[]);
    assert!(
        !copied.iter().any(|copy| copy.outcome == StepState::Done),
        "{copied:?}"
    );
}

/// A step run again after an earlier run finished (a reverse undid it, then the upgrade ran it
/// again) has its new `Done` copied even though the record still holds the earlier run's `Done`,
/// so the revision can be recorded; the earlier run's own `Done` is not copied a second time
#[test]
fn the_operators_done_of_a_rerun_is_copied_though_the_record_holds_an_older_done() {
    // the operator ran the step twice, hours apart
    let first_running = entry(
        RELEASE,
        PROCEDURE_STEP,
        StepState::Running,
        Some("2026-10-22T00:00:00Z"),
    );
    let first_done = entry(
        RELEASE,
        PROCEDURE_STEP,
        StepState::Done,
        Some("2026-10-22T00:01:00Z"),
    );
    let second_running = entry(
        RELEASE,
        PROCEDURE_STEP,
        StepState::Running,
        Some("2026-10-22T05:00:00Z"),
    );
    let second_done = entry(
        RELEASE,
        PROCEDURE_STEP,
        StepState::Done,
        Some("2026-10-22T05:01:00Z"),
    );
    let history = [
        first_running.clone(),
        first_done.clone(),
        second_running.clone(),
        second_done.clone(),
    ];
    // the record holds the first run's entries only
    let recorded = [first_running, first_done.clone()];
    // the second run's `Done` is copied and the first run's is not copied again
    let copied = picked(&DATA_BLANK_PROCEDURE, &history, &recorded);
    assert!(copied.contains(&second_done), "{copied:?}");
    assert!(!copied.contains(&first_done), "{copied:?}");
}
