//! Tests of downgrade planning: reverse paths, irreversible steps, fused `down` chains, merged
//! reverse rebuilds, lossy reverses with their fills and loss reports, and the forward transforms'
//! after-a-reverse contract

use super::downgrade_fixtures::{
    CONTACT_SENTINEL, CONTACTS_LOSS, CONTACTS_LOSSY, CONTACTS_V2, DOWNGRADE, DUAL_WRITE,
    IRREVERSIBLE, LOSSY, LOSSY_ARTIFACTS, ONE_WAY, contact_user,
};
use super::fixtures::{self, BASE, NEXT, RELEASE};
use super::plan_fixtures::{self, USERS_V2, USERS_V3, UserVariant};
use super::{
    AppliedStep, Catalog, CatalogEntry, ChainLink, ChainOutcome, FieldLoss, Fill, FillValue,
    FusedChain, IssueCategory, IssueReason, LossReport, MigrationPhase, PlanDirection, PlanItem,
    PlannedPass, PlannedStep, Record, RecordClass, Reversibility, RevisionId, StepDef, StepKind,
    StepLoss, StepState, TransformDef, TransformIssue, UpgradeError, UpgradePlan, UpgradeState,
};

/// Parse a revision id that must be valid
///
/// # Arguments
///
/// * `raw` - The id to parse
fn rev(raw: &str) -> RevisionId {
    // parse the id, failing the test when it is malformed
    raw.parse().expect("valid revision")
}

/// Compose the plan from a deployment that reached one revision to another
///
/// # Arguments
///
/// * `catalog` - The catalog to plan over
/// * `from` - The revision the deployment reached
/// * `to` - The revision to reach
fn compose(catalog: &Catalog, from: &str, to: &str) -> Result<UpgradePlan, UpgradeError> {
    // build the state of a deployment at the starting revision and compose its plan
    let state = catalog.reached(&rev(from), "0.0.0");
    catalog.compose(&state, &rev(to))
}

/// Describe a plan's items as text: `converge <target> [steps]`, `passes` with each pass as
/// `<family> [links]`, or `step <id>`
///
/// # Arguments
///
/// * `plan` - The plan to describe
fn shape(plan: &UpgradePlan) -> Vec<String> {
    // describe each item on its own line
    let ids = |steps: &[PlannedStep]| {
        steps
            .iter()
            .map(|step| step.step.as_str())
            .collect::<Vec<_>>()
            .join(",")
    };
    plan.items
        .iter()
        .map(|item| match item {
            PlanItem::Converge { target, steps } => format!("converge {target} [{}]", ids(steps)),
            PlanItem::Passes(passes) => {
                let passes = passes
                    .iter()
                    .map(|pass| format!("{} [{}]", pass.family, pass.chain_ids().join(",")))
                    .collect::<Vec<_>>()
                    .join(" | ");
                format!("passes {passes}")
            }
            PlanItem::Step(step) => format!("step {}", step.step),
        })
        .collect()
}

/// Get every pass of a plan, in order
///
/// # Arguments
///
/// * `plan` - The plan to read
fn passes(plan: &UpgradePlan) -> Vec<&PlannedPass> {
    // flatten the items of passes
    plan.items
        .iter()
        .filter_map(|item| match item {
            PlanItem::Passes(passes) => Some(passes),
            _ => None,
        })
        .flatten()
        .collect()
}

/// Get a field of a Redis record
///
/// # Arguments
///
/// * `record` - The record to read
/// * `field` - The field's name
fn field<'a>(record: &'a Record, field: &str) -> Option<&'a str> {
    // only Redis records hold fields
    match record {
        Record::Redis { fields, .. } => fields.get(field).map(String::as_str),
        _ => None,
    }
}

/// Run every pass of a plan over users in memory, as a migration Job would, returning each
/// user's issue step, if it had one
///
/// # Arguments
///
/// * `catalog` - The catalog the plan was composed over
/// * `plan` - The plan whose passes to run
/// * `store` - The users, rewritten in place
fn sweep(catalog: &Catalog, plan: &UpgradePlan, store: &mut [Record]) -> Vec<Option<String>> {
    // each pass reads every user once and writes the ones its chain converts
    let mut issues = vec![None; store.len()];
    for pass in passes(plan) {
        let chain = catalog.chain(pass).expect("the pass resolves");
        for (index, record) in store.iter_mut().enumerate() {
            match chain.apply(record) {
                ChainOutcome::Current => (),
                ChainOutcome::Converted(converted) => *record = converted,
                ChainOutcome::Issue { step, .. } => issues[index] = Some(step),
            }
        }
    }
    issues
}

/// For a target older than the current revision, the planner finds the reverse path one
/// revision back and several revisions back
#[test]
fn reverse_paths_one_and_several_revisions_back() {
    // one revision back runs the reverse transform as a pass, then the reverse rebuild
    let plan = compose(&DOWNGRADE, NEXT, RELEASE).expect("one back");
    assert_eq!(plan.direction, PlanDirection::Downgrade);
    assert_eq!(plan.path, [rev(NEXT), rev(RELEASE)]);
    assert_eq!(
        shape(&plan),
        [
            "passes redis:users [users-v3-reverse]",
            "step rebuild-b-reverse"
        ]
    );
    // several revisions back follows both reverse edges, fusing the reverse transforms
    let plan = compose(&DOWNGRADE, NEXT, BASE).expect("two back");
    assert_eq!(plan.direction, PlanDirection::Downgrade);
    assert_eq!(plan.path, [rev(NEXT), rev(RELEASE), rev(BASE)]);
    assert_eq!(
        shape(&plan),
        [
            "passes redis:users [users-v3-reverse,users-v2-reverse]",
            "step rebuild-a-reverse"
        ]
    );
    // the chain reads the newest layout and writes the oldest, every step marked as a reverse
    let pass = passes(&plan)[0];
    assert_eq!(
        pass.chain,
        [
            ChainLink {
                revision: rev(RELEASE),
                step: "users-v3-reverse".to_owned(),
                from: 3,
                to: 2,
            },
            ChainLink {
                revision: rev(BASE),
                step: "users-v2-reverse".to_owned(),
                from: 2,
                to: 1,
            },
        ]
    );
    assert!(pass.steps.iter().all(|step| step.reverse));
    assert!(!pass.after_reverse);
    // the flat plan finds the same reverse path
    let state = DOWNGRADE.reached(&rev(NEXT), "0.0.0");
    let steps = DOWNGRADE.plan(&state, &rev(BASE)).expect("plan");
    assert_eq!(steps.len(), 4);
    assert!(steps.iter().all(|step| step.reverse));
    // an upgrade over the same catalog still goes forward
    let up = compose(&DOWNGRADE, BASE, NEXT).expect("up");
    assert_eq!(up.direction, PlanDirection::Upgrade);
    assert_eq!(up.loss, None);
}

/// A multi-revision downgrade is refused before anything runs when any edge on the path lacks a
/// reverse edge or any step on it is irreversible
#[test]
fn downgrades_without_reverse_edges_or_across_irreversible_steps_are_refused() {
    // an edge without a reverse edge leaves no path back, one or several revisions
    for target in [RELEASE, BASE] {
        assert_eq!(
            compose(&ONE_WAY, NEXT, target),
            Err(UpgradeError::NoPath {
                from: rev(NEXT),
                to: rev(target),
            })
        );
    }
    // the edge with a reverse edge still goes back
    assert!(compose(&ONE_WAY, RELEASE, BASE).is_ok());
    // a finished irreversible step refuses the whole path, one revision back or several
    let refused = UpgradeError::Irreversible {
        revision: rev(NEXT),
        step: "files-v2".to_owned(),
    };
    for target in [RELEASE, BASE] {
        assert_eq!(compose(&IRREVERSIBLE, NEXT, target), Err(refused.clone()));
        let state = IRREVERSIBLE.reached(&rev(NEXT), "0.0.0");
        assert_eq!(
            IRREVERSIBLE.plan(&state, &rev(target)),
            Err(refused.clone())
        );
        assert_eq!(
            IRREVERSIBLE.superseded(&state, &rev(target)),
            Err(refused.clone())
        );
    }
    // an irreversible step the deployment never finished has nothing to undo
    let mut partial = IRREVERSIBLE.reached(&rev(RELEASE), "0.0.0");
    partial.revision = rev(NEXT);
    let plan = IRREVERSIBLE
        .compose(&partial, &rev(RELEASE))
        .expect("partial");
    assert_eq!(shape(&plan), Vec::<String>::new());
    // the irreversible catalog is consistent, since the step says why it can't be undone
    assert_eq!(IRREVERSIBLE.check(), Ok(()));
    // an irreversible step on a later edge of the path refuses it too
    let early = Catalog {
        edges: vec![
            IRREVERSIBLE.edges[0],
            CatalogEntry {
                steps: [
                    IRREVERSIBLE.edges[1].steps,
                    &IRREVERSIBLE.edges[3].steps[1..],
                ]
                .concat()
                .leak(),
                ..IRREVERSIBLE.edges[1]
            },
            IRREVERSIBLE.edges[2],
            DOWNGRADE.edges[3],
            DOWNGRADE.edges[4],
        ]
        .leak(),
        floors: &[],
    };
    assert_eq!(
        compose(&early, NEXT, BASE),
        Err(UpgradeError::Irreversible {
            revision: rev(RELEASE),
            step: "files-v2".to_owned(),
        })
    );
    // while the edge above it still goes back
    assert!(compose(&early, NEXT, RELEASE).is_ok());
}

/// Add a skip edge from [`BASE`] to [`NEXT`] and its reverse edge to a catalog's edges
///
/// # Arguments
///
/// * `edges` - The catalog's edges
fn with_skip(edges: &[CatalogEntry]) -> Catalog {
    // a skip step running the layout 2 to 3 transform, and the step reversing it
    let skip = StepDef {
        id: "users-skip",
        thoradm_migration: Some("users-skip"),
        ..DOWNGRADE.edges[3].steps[0]
    };
    let skip_reverse = StepDef {
        id: "users-skip-reverse",
        reverses: Some("users-skip"),
        thoradm_migration: Some("users-skip-reverse"),
        ..DOWNGRADE.edges[4].steps[0]
    };
    // the skip edge and its reverse beside the given edges
    let mut edges = edges.to_vec();
    edges.push(CatalogEntry {
        from: Some(BASE),
        steps: vec![skip].leak(),
        ..DOWNGRADE.edges[3]
    });
    edges.push(CatalogEntry {
        from: Some(NEXT),
        id: BASE,
        steps: vec![skip_reverse].leak(),
        ..DOWNGRADE.edges[4]
    });
    Catalog {
        edges: edges.leak(),
        floors: &[],
    }
}

/// Build the state of a deployment at [`RELEASE`] whose upgrade to [`NEXT`] stopped partway,
/// after recording entries of steps of the edge to [`NEXT`]
///
/// # Arguments
///
/// * `entries` - The steps of the edge to [`NEXT`] and their outcomes, oldest first
fn stopped_partway(entries: &[(&str, super::StepState)]) -> super::UpgradeState {
    // the deployment finished the edge to the release and recorded it
    let mut state = IRREVERSIBLE.reached(&rev(RELEASE), "0.0.0");
    // then ran some steps of the next edge without finishing it
    for (step, outcome) in entries {
        state.record(super::AppliedStep {
            revision: NEXT.to_owned(),
            step: (*step).to_owned(),
            outcome: *outcome,
            ..super::AppliedStep::default()
        });
    }
    state
}

/// A downgrade from a deployment that stopped partway up an edge refuses the steps of that edge it
/// ran, finished or only started, since the path down from the recorded revision never undoes
/// them; one that only blocked, or that a reset voided, leaves the path planned
#[test]
fn downgrades_from_a_half_finished_upgrade_refuse_the_steps_it_ran_above_the_revision() {
    // the irreversible step of the next edge refuses the path down whether it finished or started
    let refused = UpgradeError::Irreversible {
        revision: rev(NEXT),
        step: "files-v2".to_owned(),
    };
    for outcome in [
        super::StepState::Done,
        super::StepState::Skipped,
        super::StepState::Running,
    ] {
        let state = stopped_partway(&[("files-v2", outcome)]);
        assert_eq!(
            IRREVERSIBLE.plan(&state, &rev(BASE)),
            Err(refused.clone()),
            "{outcome:?}"
        );
        assert_eq!(
            IRREVERSIBLE.compose(&state, &rev(BASE)),
            Err(refused.clone()),
            "{outcome:?}"
        );
        assert_eq!(
            IRREVERSIBLE.superseded(&state, &rev(BASE)),
            Err(refused.clone()),
            "{outcome:?}"
        );
    }
    // a run that started and then failed may have written part of its data too
    let failed = stopped_partway(&[
        ("files-v2", super::StepState::Running),
        ("files-v2", super::StepState::Failed),
    ]);
    assert_eq!(IRREVERSIBLE.plan(&failed, &rev(BASE)), Err(refused.clone()));
    // the refusal names the step, its revision, and the restore
    let message = refused.to_string();
    assert!(
        message.contains("files-v2") && message.contains(NEXT),
        "{message}"
    );
    assert!(message.contains("restore from backup"), "{message}");
    // a step with a transform that no step of the path down reverses refuses it as unreversed
    let backfilled = stopped_partway(&[("users-v3", super::StepState::Done)]);
    assert_eq!(
        IRREVERSIBLE.plan(&backfilled, &rev(BASE)),
        Err(UpgradeError::Unreversed {
            revision: rev(NEXT),
            step: "users-v3".to_owned(),
        })
    );
    // a step that only blocked never ran, so the path down is planned as before
    let blocked = stopped_partway(&[("files-v2", super::StepState::Blocked)]);
    let planned = IRREVERSIBLE
        .plan(&blocked, &rev(BASE))
        .expect("nothing ran above the revision");
    let untouched = IRREVERSIBLE
        .plan(&IRREVERSIBLE.reached(&rev(RELEASE), "0.0.0"), &rev(BASE))
        .expect("a clean deployment goes back");
    assert_eq!(planned, untouched);
    // a reset to the recorded revision voids the work above it
    let mut reset = stopped_partway(&[("files-v2", super::StepState::Done)]);
    reset.record(super::AppliedStep {
        revision: RELEASE.to_owned(),
        step: super::RESET_STATE_STEP.to_owned(),
        outcome: super::StepState::Done,
        ..super::AppliedStep::default()
    });
    assert!(IRREVERSIBLE.plan(&reset, &rev(BASE)).is_ok());
    // going up through the edge plans its steps, which the forward runs check as before
    let resumed = stopped_partway(&[("files-v2", super::StepState::Running)]);
    assert!(IRREVERSIBLE.plan(&resumed, &rev(NEXT)).is_ok());
}

/// A downgrade whose edges don't undo the steps the deployment finished on its way up, as when
/// it came up through a chain and the path down takes a skip edge, is refused before anything
/// runs, and a finished irreversible step on the chain refuses it as irreversible
#[test]
fn downgrades_across_skip_edges_refuse_steps_their_edges_dont_undo() {
    // a deployment that came up through the chain finished the chain's steps, not the skip step
    let skip = with_skip(DOWNGRADE.edges);
    let chain = DOWNGRADE.reached(&rev(NEXT), "0.0.0");
    let refused = UpgradeError::Unreversed {
        revision: rev(RELEASE),
        step: "users-v2".to_owned(),
    };
    // the shortest path down takes the reverse skip edge, which undoes none of them
    assert_eq!(
        skip.path(&rev(NEXT), &rev(BASE)).map(|path| path.len()),
        Ok(1)
    );
    assert_eq!(skip.compose(&chain, &rev(BASE)), Err(refused.clone()));
    assert_eq!(skip.plan(&chain, &rev(BASE)), Err(refused.clone()));
    assert_eq!(skip.superseded(&chain, &rev(BASE)), Err(refused.clone()));
    // the refusal tells the admin to go down the way the cluster came up
    let message = refused.to_string();
    assert!(
        message.contains("users-v2") && message.contains(RELEASE),
        "{message}"
    );
    assert!(message.contains("downgrade along the edges"), "{message}");
    // going down the chain one edge at a time still works
    let plan = skip.compose(&chain, &rev(RELEASE)).expect("one back");
    assert_eq!(
        shape(&plan),
        [
            "passes redis:users [users-v3-reverse]",
            "step rebuild-b-reverse"
        ]
    );
    // a deployment that came up through the skip edge goes back down it
    let skipped = skip.reached(&rev(NEXT), "0.0.0");
    let plan = skip.compose(&skipped, &rev(BASE)).expect("skip back");
    assert_eq!(shape(&plan), ["passes redis:users [users-skip-reverse]"]);
    // but not down the chain, whose edges don't undo the skip step
    assert_eq!(
        skip.compose(&skipped, &rev(RELEASE)),
        Err(UpgradeError::Unreversed {
            revision: rev(NEXT),
            step: "users-skip".to_owned(),
        })
    );
    // a finished irreversible step on the chain refuses the skip path as irreversible
    let irreversible = with_skip(&IRREVERSIBLE.edges[..4]);
    let chain = IRREVERSIBLE.reached(&rev(NEXT), "0.0.0");
    assert_eq!(
        irreversible.compose(&chain, &rev(BASE)),
        Err(UpgradeError::Irreversible {
            revision: rev(NEXT),
            step: "files-v2".to_owned(),
        })
    );
}

/// A path that goes up a skip edge and back down a reverse edge refuses the skip edge's steps the
/// edge down doesn't undo, rather than planning nothing for the way down
#[test]
fn paths_up_a_skip_edge_and_back_down_refuse_steps_the_way_down_leaves() {
    // a chain of four release edges, the last with a reverse edge, and a skip edge past the
    // target from the baseline
    let revisions = [
        "2026-01-v01",
        "2026-01-v02",
        "2026-01-v03",
        "2026-01-v04",
        "2026-01-v05",
    ];
    let forward = |id: &'static str| StepDef {
        id,
        thoradm_migration: Some(id),
        ..DOWNGRADE.edges[1].steps[0]
    };
    let edge = |from: Option<&'static str>, id: &'static str, steps: Vec<StepDef>| CatalogEntry {
        from,
        id,
        steps: steps.leak(),
        ..DOWNGRADE.edges[0]
    };
    let catalog = Catalog {
        edges: vec![
            edge(None, revisions[0], Vec::new()),
            edge(Some(revisions[0]), revisions[1], vec![forward("chain-2")]),
            edge(Some(revisions[1]), revisions[2], vec![forward("chain-3")]),
            edge(Some(revisions[2]), revisions[3], vec![forward("chain-4")]),
            edge(Some(revisions[3]), revisions[4], vec![forward("chain-5")]),
            edge(
                Some(revisions[4]),
                revisions[3],
                vec![StepDef {
                    id: "chain-5-reverse",
                    reverses: Some("chain-5"),
                    thoradm_migration: Some("chain-5-reverse"),
                    ..DOWNGRADE.edges[2].steps[0]
                }],
            ),
            edge(Some(revisions[0]), revisions[4], vec![forward("skip-5")]),
        ]
        .leak(),
        floors: &[],
    };
    // the shortest path to the fourth revision goes up the skip edge and down the reverse edge
    let path = catalog
        .path(&rev(revisions[0]), &rev(revisions[3]))
        .expect("path");
    assert_eq!(path.len(), 2);
    // the reverse edge undoes the chain's step, not the skip step, so the path is refused
    let state = catalog.reached(&rev(revisions[0]), "0.0.0");
    assert_eq!(
        catalog.compose(&state, &rev(revisions[3])),
        Err(UpgradeError::Unreversed {
            revision: rev(revisions[4]),
            step: "skip-5".to_owned(),
        })
    );
}

/// Reverse steps run in the reverse order of the forward steps they undo, whichever order their
/// edge declares them in, so two transforms of one family revert through one chain
#[test]
fn reverse_steps_run_in_the_reverse_order_of_their_forward_steps() {
    // both users transforms on one release edge, reversed by an edge listing them forwards
    let forwards = Catalog {
        edges: vec![
            DOWNGRADE.edges[0],
            CatalogEntry {
                steps: vec![DOWNGRADE.edges[1].steps[0], DOWNGRADE.edges[3].steps[0]].leak(),
                ..DOWNGRADE.edges[1]
            },
            CatalogEntry {
                steps: vec![DOWNGRADE.edges[2].steps[0], DOWNGRADE.edges[4].steps[0]].leak(),
                ..DOWNGRADE.edges[2]
            },
        ]
        .leak(),
        floors: &[],
    };
    assert_eq!(forwards.check(), Ok(()));
    // the reverses fuse into one chain from layout 3 to layout 1, newest first
    let plan = compose(&forwards, RELEASE, BASE).expect("down");
    assert_eq!(
        shape(&plan),
        ["passes redis:users [users-v3-reverse,users-v2-reverse]"]
    );
    // the flat plan lists them in the same order
    let state = forwards.reached(&rev(RELEASE), "0.0.0");
    let steps = forwards.plan(&state, &rev(BASE)).expect("plan");
    let ids = steps
        .iter()
        .map(|step| step.step.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["users-v3-reverse", "users-v2-reverse"]);
    // and every layout 3 user reverts to layout 1 without an issue
    for dark in [true, false] {
        let mut store = [plan_fixtures::user(
            0,
            UserVariant::Layout3,
            dark,
            Some(true),
        )];
        assert_eq!(sweep(&forwards, &plan, &mut store), [None]);
        assert_eq!(
            store[0],
            plan_fixtures::user(0, UserVariant::Layout1, dark, Some(true))
        );
    }
}

/// Revert a users record from layout 2 to layout 1 unless it holds `ai = true`, so a user can
/// convert at the first link of a down chain and fail at the second
///
/// # Arguments
///
/// * `record` - The user to revert
fn strict_users_v2_down(record: Record) -> Result<Record, TransformIssue> {
    // refuse the flag, and revert anything else as the fixture transform does
    if field(&record, "ai") == Some("true") {
        return Err(TransformIssue {
            reason: IssueReason::new(IssueCategory::InvalidValue, "ai"),
            layouts_tried: vec![2],
        });
    }
    let down = USERS_V2.down.expect("the fixture transform has a down");
    down(record)
}

/// The layout 1 to 2 users transform with [`strict_users_v2_down`] as its `down`
static STRICT_USERS_V2: TransformDef = TransformDef {
    down: Some(strict_users_v2_down),
    ..USERS_V2
};

/// Revert a user through reverse edges one at a time, as separate passes with a write between
/// them, without the fused chain: each link classifies the record with its forward transform,
/// reverts it when it is in the link's new layout, skips it when it is older, and stops with
/// the link's step when the record is unrecognized or doesn't revert
///
/// # Arguments
///
/// * `links` - Each reverse step and the forward transform it undoes, newest first
/// * `record` - The user as stored
fn stepped_down(links: &[(&str, &TransformDef)], record: &Record) -> Result<Record, String> {
    // run each link in turn over the record the previous one wrote
    let mut current = record.clone();
    for (step, transform) in links {
        match (transform.classify)(&current) {
            RecordClass::Current => {
                current = transform
                    .revert(current, None)
                    .map_err(|_| (*step).to_owned())?;
            }
            RecordClass::Migratable { .. } => (),
            RecordClass::Unrecognized { .. } => return Err((*step).to_owned()),
        }
    }
    Ok(current)
}

/// A fused `down` chain over several revisions gives the same result as running each reverse
/// edge with writes in between, for many generated users of every layout and broken variant
#[test]
fn fused_down_chains_equal_reverse_edges_with_writes_between() {
    // users of every layout and every broken variant
    let users = UserVariant::ALL
        .into_iter()
        .flat_map(|variant| plan_fixtures::users(variant, 40))
        .collect::<Vec<_>>();
    // the downgrade catalog, and one whose second link refuses users with `ai = true`
    let strict = Catalog {
        edges: vec![
            DOWNGRADE.edges[0],
            CatalogEntry {
                steps: vec![
                    StepDef {
                        transform: Some(&STRICT_USERS_V2),
                        ..DOWNGRADE.edges[1].steps[0]
                    },
                    DOWNGRADE.edges[1].steps[1],
                ]
                .leak(),
                ..DOWNGRADE.edges[1]
            },
            DOWNGRADE.edges[2],
            DOWNGRADE.edges[3],
            DOWNGRADE.edges[4],
        ]
        .leak(),
        floors: &[],
    };
    for catalog in [&DOWNGRADE, &strict] {
        // reverse both edges at once with the fused chain
        let mut fused = users.clone();
        let plan = compose(catalog, NEXT, BASE).expect("fused");
        let issues = sweep(catalog, &plan, &mut fused);
        // the reference reverts with each forward transform of the catalog, newest first
        let transform = |id: &str| {
            catalog
                .step(id)
                .and_then(|(_, step)| step.transform)
                .expect("a transform")
        };
        let links = [
            ("users-v3-reverse", transform("users-v3")),
            ("users-v2-reverse", transform("users-v2")),
        ];
        // every user ends as the reference leaves it, with an issue at the same link
        let mut converted = 0;
        for (index, user) in users.iter().enumerate() {
            match stepped_down(&links, user) {
                Ok(expected) => {
                    assert_eq!(issues[index], None, "{user:?}");
                    assert_eq!(fused[index], expected, "{user:?}");
                    converted += 1;
                }
                Err(step) => {
                    assert_eq!(issues[index].as_deref(), Some(step.as_str()), "{user:?}");
                    assert_eq!(fused[index], *user, "{user:?}");
                }
            }
        }
        // the sample holds users that convert and users that are issues
        assert!(converted > 0 && converted < users.len());
    }
    // a user that converts at the first link and fails at the second is blamed on the second
    let failing = plan_fixtures::user(0, UserVariant::Layout3, true, Some(true));
    let plan = compose(&strict, NEXT, BASE).expect("strict");
    assert_eq!(
        sweep(&strict, &plan, &mut [failing]),
        [Some("users-v2-reverse".to_owned())]
    );
    // every layout 3 user reverts to the layout 1 user with its theme's flag and its ai flag
    let plan = compose(&DOWNGRADE, NEXT, BASE).expect("fused");
    for dark in [true, false] {
        for ai in [None, Some(true), Some(false)] {
            let mut store = [plan_fixtures::user(0, UserVariant::Layout3, dark, ai)];
            assert_eq!(sweep(&DOWNGRADE, &plan, &mut store), [None]);
            assert_eq!(
                store[0],
                plan_fixtures::user(0, UserVariant::Layout1, dark, ai),
                "dark {dark} ai {ai:?}"
            );
        }
    }
}

/// Reverse rebuilds on a downgrade path merge into one rebuild with the target revision's
/// mappings
#[test]
fn reverse_rebuilds_merge_into_the_target_revisions_rebuild() {
    // two revisions back keeps only the rebuild back to the target's mappings, after the passes
    let state = DOWNGRADE.reached(&rev(NEXT), "0.0.0");
    let plan = DOWNGRADE.compose(&state, &rev(BASE)).expect("compose");
    let rebuilds = shape(&plan)
        .into_iter()
        .filter(|line| line.contains("rebuild"))
        .collect::<Vec<_>>();
    assert_eq!(rebuilds, ["step rebuild-a-reverse"]);
    assert_eq!(
        shape(&plan).last().map(String::as_str),
        Some("step rebuild-a-reverse")
    );
    // the dropped rebuild names the kept one, so it can be recorded as skipped
    let superseded = DOWNGRADE
        .superseded(&state, &rev(BASE))
        .expect("superseded");
    let superseded = superseded
        .iter()
        .map(|(step, by)| (step.step.as_str(), *by))
        .collect::<Vec<_>>();
    assert_eq!(superseded, [("rebuild-b-reverse", "rebuild-a-reverse")]);
    // one revision back has a single rebuild, superseding nothing
    let one = DOWNGRADE.compose(&state, &rev(RELEASE)).expect("compose");
    assert!(shape(&one).contains(&"step rebuild-b-reverse".to_owned()));
    assert_eq!(DOWNGRADE.superseded(&state, &rev(RELEASE)), Ok(Vec::new()));
}

/// A path across an irreversible step is refused with a message naming the step, its revision,
/// and "restore from backup"
#[test]
fn irreversible_refusals_name_the_step_revision_and_restore() {
    // the refusal names the irreversible step and the revision it belongs to
    let error = compose(&IRREVERSIBLE, NEXT, BASE).expect_err("refused");
    let message = error.to_string();
    assert!(message.contains("files-v2"), "{message}");
    assert!(message.contains(NEXT), "{message}");
    assert!(message.contains("restore from backup"), "{message}");
}

/// An older operator that finds an unknown newer revision refuses, and its message names the
/// `thoradm migrate revert --to <N-1>` command
#[test]
fn older_operators_refuse_newer_revisions_naming_the_revert() {
    // an older catalog that ends at the release before the cluster's revision
    let older = fixtures::ONE_EDGE;
    let error = older.check_current(&rev(NEXT)).expect_err("newer revision");
    assert_eq!(
        error,
        UpgradeError::AheadOfCatalog {
            current: rev(NEXT),
            latest: rev(RELEASE),
        }
    );
    // the message names the revert to the older operator's latest revision
    let message = error.to_string();
    assert!(message.contains(NEXT), "{message}");
    assert!(
        message.contains(&format!("thoradm migrate revert --to {RELEASE}")),
        "{message}"
    );
    // the message falls back to a backup restore when the revert can't undo a step
    assert!(
        message.contains("restore from a backup taken before"),
        "{message}"
    );
    // planning from the newer revision is refused the same way
    let state = super::UpgradeState::new(rev(NEXT), "0.0.0");
    assert!(matches!(
        older.compose(&state, &rev(RELEASE)),
        Err(UpgradeError::AheadOfCatalog { .. })
    ));
}

/// `up` then a lossy reverse then `up` again yields sentinels in the filled fields, not
/// fabricated values
#[test]
fn lossy_round_trips_yield_sentinels_not_fabricated_values() {
    // a user whose contacts the reverse can't derive a single contact from
    let empty = contact_user(0, "contacts", "[]");
    let reverted = CONTACTS_V2
        .revert(empty.clone(), Some(&CONTACTS_LOSSY))
        .expect("revert");
    assert_eq!(field(&reverted, "contact"), Some(CONTACT_SENTINEL));
    // upgrading again reads the sentinel as no contact, not as a real address
    let again = (CONTACTS_V2.up)(reverted).expect("up again");
    assert_eq!(again, empty);
    // the same holds starting from the old layout: up, the lossy reverse of a user that lost
    // its contacts in the new layout, and up again
    let upgraded = (CONTACTS_V2.up)(contact_user(1, "contact", "ann@example.com")).expect("up");
    assert_eq!(field(&upgraded, "contacts"), Some("[\"ann@example.com\"]"));
    let emptied = contact_user(1, "contacts", "[]");
    let round = (CONTACTS_V2.up)(
        CONTACTS_V2
            .revert(emptied.clone(), Some(&CONTACTS_LOSSY))
            .expect("revert"),
    )
    .expect("up");
    assert_eq!(round, emptied);
    // a contact the reverse derives is kept rather than filled
    let kept = CONTACTS_V2
        .revert(
            contact_user(2, "contacts", "[\"bob@example.com\",\"cy@example.com\"]"),
            Some(&CONTACTS_LOSSY),
        )
        .expect("revert");
    assert_eq!(field(&kept, "contact"), Some("bob@example.com"));
    // a constant fill instead would come back as a fabricated contact
    let constant = Reversibility::LossyRevert {
        fills: &[Fill {
            field: "contact",
            value: FillValue::Constant("nobody@example.com"),
        }],
        lost_fields: &["contact"],
        enum_mappings: &[],
        privilege_orders: &[],
        description: CONTACTS_LOSS,
    };
    let fabricated = (CONTACTS_V2.up)(
        CONTACTS_V2
            .revert(empty.clone(), Some(&constant))
            .expect("revert"),
    )
    .expect("up");
    assert_eq!(
        field(&fabricated, "contacts"),
        Some("[\"nobody@example.com\"]")
    );
    // the registered round trips of the lossy catalog pass the artifact check
    assert_eq!(LOSSY.check(), Ok(()));
    assert_eq!(LOSSY.check_artifacts(&LOSSY_ARTIFACTS), Ok(()));
}

/// A lossy reverse is planned as a pass whose chain writes the fills, and the plan and the step
/// name what it loses
#[test]
fn lossy_reverses_fill_in_their_pass_and_report_their_loss() {
    // the reverse edge runs the lossy reverse as a pass
    let plan = compose(&LOSSY, RELEASE, BASE).expect("compose");
    assert_eq!(
        shape(&plan),
        ["passes redis:contacts [contacts-v2-reverse]"]
    );
    // the step and the plan name the lost field, leaving counts to the step's check
    let loss = StepLoss {
        revision: BASE.to_owned(),
        step: "contacts-v2-reverse".to_owned(),
        description: CONTACTS_LOSS.to_owned(),
        fields: vec![FieldLoss {
            field: "contact".to_owned(),
            objects: 0,
            backed_up: false,
        }],
    };
    assert_eq!(passes(&plan)[0].steps[0].loss, Some(loss.clone()));
    assert_eq!(
        plan.loss,
        Some(LossReport {
            steps: vec![loss.clone()]
        })
    );
    // the flat plan names it too
    let state = LOSSY.reached(&rev(RELEASE), "0.0.0");
    assert_eq!(
        LOSSY.plan(&state, &rev(BASE)).expect("plan")[0].loss,
        Some(loss)
    );
    // the resolved chain reverts and fills in memory
    let chain = LOSSY.chain(passes(&plan)[0]).expect("resolves");
    assert_eq!(chain.direction, PlanDirection::Downgrade);
    let ChainOutcome::Converted(reverted) = chain.apply(&contact_user(0, "contacts", "[]")) else {
        panic!("the user reverts");
    };
    assert_eq!(field(&reverted, "contact"), Some(CONTACT_SENTINEL));
    // a user still in the old layout is left alone, and a broken list is an issue
    assert_eq!(
        chain.apply(&contact_user(1, "contact", "ann@example.com")),
        ChainOutcome::Current
    );
    assert!(matches!(
        chain.apply(&contact_user(2, "contacts", "{")),
        ChainOutcome::Issue { step, .. } if step == "contacts-v2-reverse"
    ));
}

/// A step of an edge leading down names what it loses when it reverses a lossy step, and nothing
/// when it reverses another kind, isn't on a reverse edge, or isn't in the catalog
#[test]
fn a_reverse_step_names_what_it_loses() {
    // the lossy reverse names its field, its description, and the revision it reverts to
    let loss = LOSSY.planned_loss("contacts-v2-reverse").expect("a loss");
    assert_eq!(loss.revision, BASE);
    assert_eq!(loss.step, "contacts-v2-reverse");
    assert_eq!(loss.description, CONTACTS_LOSS);
    assert_eq!(
        loss.fields,
        vec![FieldLoss {
            field: "contact".to_owned(),
            objects: 0,
            backed_up: false,
        }]
    );
    // the forward step, a reverse of a lossless step, and an unknown step lose nothing
    assert_eq!(LOSSY.planned_loss("contacts-v2"), None);
    assert_eq!(DOWNGRADE.planned_loss("users-v2-reverse"), None);
    assert_eq!(LOSSY.planned_loss("nothing"), None);
}

/// The reverse of a `DualWrite` step runs no pass and only records the revision
#[test]
fn dual_write_reverses_plan_no_pass() {
    // the reverse step is a single step, not a pass
    let plan = compose(&DUAL_WRITE, RELEASE, BASE).expect("compose");
    assert_eq!(shape(&plan), ["step users-v2-reverse"]);
    assert_eq!(plan.loss, None);
}

/// A contract step with a transform runs after the barriers that follow it, as every contract
/// step does
#[test]
fn contract_transforms_run_last() {
    // a contract transform on the first edge and a manual step on the second
    /// A data step in the contract phase that carries a transform
    static CONTRACT: StepDef = StepDef {
        id: "clear-dark",
        kind: StepKind::Data,
        description: "A fixture step",
        manual_procedure: Some("A fixture procedure"),
        phase: Some(MigrationPhase::Contract),
        transform: Some(&USERS_V2),
        ..StepDef::BASE
    };
    /// A manual step with no transform
    static MANUAL: StepDef = StepDef {
        id: "manual",
        kind: StepKind::Manual,
        description: "A fixture step",
        manual_procedure: Some("A fixture procedure"),
        ..StepDef::BASE
    };
    let catalog = Catalog {
        edges: vec![
            fixtures::ONE_EDGE.edges[0],
            super::CatalogEntry {
                from: Some(BASE),
                id: RELEASE,
                steps: std::slice::from_ref(&CONTRACT),
                ..fixtures::ONE_EDGE.edges[1]
            },
            super::CatalogEntry {
                from: Some(RELEASE),
                id: NEXT,
                steps: std::slice::from_ref(&MANUAL),
                ..fixtures::ONE_EDGE.edges[1]
            },
        ]
        .leak(),
        floors: &[],
    };
    // the manual step runs first and the contract transform's pass last
    let plan = compose(&catalog, BASE, NEXT).expect("compose");
    assert_eq!(
        shape(&plan),
        ["step manual", "passes redis:users [clear-dark]"]
    );
}

/// A forward transform's `up` derives the new form from the old one whenever the old form is
/// present, so the after-a-reverse mode replaces a stale new form left by a reverse, for every
/// fixture transform with a `down`
#[test]
fn after_a_reverse_up_recomputes_stale_new_forms() {
    // a user with the old form changed after a reverse, beside the stale new form, for each
    // transform with the field holding the recomputed new form and its expected value
    let stale = |pairs: &[(&str, &str)]| Record::Redis {
        key: "users:stale".to_owned(),
        fields: pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
    };
    let cases: [(&'static TransformDef, Record, &str, &str); 3] = [
        (
            &fixtures::THEME,
            stale(&[("dark", "false"), ("theme", "dark")]),
            "theme",
            "light",
        ),
        (
            &USERS_V2,
            stale(&[("dark", "false"), ("theme", "dark")]),
            "theme",
            "light",
        ),
        (
            &USERS_V3,
            stale(&[("theme", "light"), ("settings", "{\"theme\":\"dark\"}")]),
            "settings",
            "{\"theme\":\"light\"}",
        ),
    ];
    for (transform, user, name, expected) in cases {
        assert!(transform.down.is_some());
        // the after-a-reverse chain recomputes the new form from the old one
        let chain = FusedChain {
            links: vec![("step", transform)],
            after_reverse: true,
            direction: PlanDirection::Upgrade,
            reversibility: Vec::new(),
        };
        let ChainOutcome::Converted(converted) = chain.apply(&user) else {
            panic!("{user:?} is recomputed");
        };
        assert_eq!(field(&converted, name), Some(expected), "{converted:?}");
    }
}

/// The newest revision of the production catalog
const NEWEST: &str = "2026-10-v04";

/// The revision before the newest one, which the newest revision's edge back leads to
const PREVIOUS: &str = "2026-10-v03";

/// The first Helm revision, two revisions below the newest one
const FIRST_HELM: &str = "2026-10-v02";

/// Build the state of a deployment at a revision that finished only some of its steps
///
/// # Arguments
///
/// * `revision` - The revision the deployment is at
/// * `finished` - The ids of the steps of that revision the deployment finished
fn state_at(revision: &str, finished: &[&str]) -> UpgradeState {
    // start from a deployment at the revision and record each finished step
    let mut state = UpgradeState::new(rev(revision), "0.0.0");
    for step in finished {
        state.record(AppliedStep {
            revision: revision.to_owned(),
            step: (*step).to_owned(),
            outcome: StepState::Done,
            ..AppliedStep::default()
        });
    }
    state
}

/// Build the state of a deployment at the newest revision that finished only some of its steps
///
/// # Arguments
///
/// * `finished` - The ids of the steps of the newest revision the deployment finished
fn newest_state(finished: &[&str]) -> UpgradeState {
    // the steps belong to the newest revision
    state_at(NEWEST, finished)
}

/// A downgrade of the production catalog from the newest revision is refused for each tool
/// conversion step the deployment finished, naming that step and the revision, whether it goes
/// back one revision or two
#[test]
fn the_production_downgrade_refuses_each_tool_conversion_step() {
    for step in [
        "images-to-tools",
        "pipelines-to-tool-stages",
        "provenance-to-legacy-tools",
    ] {
        // a deployment that finished the step alongside the drain
        let state = newest_state(&["drain-reactions", step]);
        for target in [PREVIOUS, FIRST_HELM] {
            let error = Catalog::PRODUCTION
                .compose(&state, &rev(target))
                .expect_err("the downgrade is refused");
            assert_eq!(
                error,
                UpgradeError::Irreversible {
                    revision: rev(NEWEST),
                    step: step.to_owned(),
                }
            );
            // the message names the step and says to restore from backup
            let message = error.to_string();
            assert!(message.contains(step), "{message}");
            assert!(message.contains(NEWEST), "{message}");
            assert!(message.contains("restore from backup"), "{message}");
            // planning refuses it the same way
            assert_eq!(Catalog::PRODUCTION.plan(&state, &rev(target)), Err(error));
        }
    }
    // a deployment that finished every step is refused, at the first conversion step in order
    let state = Catalog::PRODUCTION.reached(&rev(NEWEST), "0.0.0");
    let error = Catalog::PRODUCTION
        .compose(&state, &rev(PREVIOUS))
        .expect_err("refused");
    assert!(error.to_string().contains("images-to-tools"), "{error}");
}

/// A downgrade across only the users conversion is not refused: the previous revision's reverse
/// step runs, and no step of the newest revision blocks it
#[test]
fn the_users_theme_conversion_alone_still_downgrades() {
    // a deployment at the revision that applies the schema first, with its steps finished
    let state = state_at(PREVIOUS, &["schema-apply", "users-settings-theme"]);
    let plan = Catalog::PRODUCTION
        .compose(&state, &rev(FIRST_HELM))
        .expect("the users conversion alone downgrades");
    // the reverse of the users conversion is the only step in the plan
    assert_eq!(
        shape(&plan),
        ["passes redis:users [users-settings-theme-reverse]"]
    );
    // a deployment at the newest revision that finished the earlier revisions and only drained
    // goes down two revisions the same way
    let mut drained = Catalog::PRODUCTION.reached(&rev(PREVIOUS), "0.0.0");
    drained.revision = rev(NEWEST);
    drained.record(AppliedStep {
        revision: NEWEST.to_owned(),
        step: "drain-reactions".to_owned(),
        outcome: StepState::Done,
        ..AppliedStep::default()
    });
    let plan = Catalog::PRODUCTION
        .compose(&drained, &rev(FIRST_HELM))
        .expect("a drain alone downgrades");
    assert_eq!(
        shape(&plan),
        ["passes redis:users [users-settings-theme-reverse]"]
    );
}

/// A deployment whose upgrade to the newest revision stopped partway, still recorded at the
/// previous revision, can't go back below it once a tool conversion step ran, finished or only
/// started, as the admin guide says; one that only drained still goes back
#[test]
fn a_half_finished_tool_conversion_refuses_a_downgrade_below_the_recorded_revision() {
    // the previous revision finished, then the drain and one conversion step of the newest
    let stopped = |step: &str, outcome: StepState| {
        let mut state = Catalog::PRODUCTION.reached(&rev(PREVIOUS), "0.0.0");
        for (id, outcome) in [("drain-reactions", StepState::Done), (step, outcome)] {
            state.record(AppliedStep {
                revision: NEWEST.to_owned(),
                step: id.to_owned(),
                outcome,
                ..AppliedStep::default()
            });
        }
        state
    };
    for step in [
        "images-to-tools",
        "pipelines-to-tool-stages",
        "provenance-to-legacy-tools",
    ] {
        for outcome in [StepState::Done, StepState::Skipped, StepState::Running] {
            // the path down from the recorded revision would leave the step's data converted
            let state = stopped(step, outcome);
            let refused = UpgradeError::Irreversible {
                revision: rev(NEWEST),
                step: step.to_owned(),
            };
            assert_eq!(
                Catalog::PRODUCTION.compose(&state, &rev(FIRST_HELM)),
                Err(refused.clone()),
                "{step} {outcome:?}"
            );
            assert_eq!(
                Catalog::PRODUCTION.plan(&state, &rev(FIRST_HELM)),
                Err(refused),
                "{step} {outcome:?}"
            );
        }
    }
    // a deployment that only drained goes back as one that never started the newest revision
    let drained = stopped("drain-reactions", StepState::Done);
    let clean = Catalog::PRODUCTION.reached(&rev(PREVIOUS), "0.0.0");
    assert_eq!(
        Catalog::PRODUCTION.plan(&drained, &rev(FIRST_HELM)),
        Catalog::PRODUCTION.plan(&clean, &rev(FIRST_HELM))
    );
    assert!(Catalog::PRODUCTION.plan(&clean, &rev(FIRST_HELM)).is_ok());
}
