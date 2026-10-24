//! Tests of the revision graph: path finding, pre-release and sibling edges, revert edges, the
//! finished-step rule, and the catalog checks

use super::fixtures::{self, BASE, CONVERT, FLOOR, NEXT, RELEASE, SIBLING, STACK};
use super::{
    AppliedStep, CATALOG, Catalog, CatalogEntry, RESET_STATE_STEP, RESTORED_STEP, RevisionId,
    StepDef, StepKind, StepState, SupportFloor, UpgradeError, UpgradeState,
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

/// Get the step ids of a plan
///
/// # Arguments
///
/// * `catalog` - The catalog to plan over
/// * `state` - The deployment's recorded state
/// * `target` - The revision to reach
fn planned_ids(catalog: &Catalog, state: &UpgradeState, target: &str) -> Vec<String> {
    // plan and keep only the step ids
    catalog
        .plan(state, &rev(target))
        .expect("plan")
        .into_iter()
        .map(|planned| planned.step)
        .collect()
}

/// Record a step as done in a state's history
///
/// # Arguments
///
/// * `state` - The state to record in
/// * `revision` - The revision the step belongs to
/// * `step` - The id of the step
fn done(state: &mut UpgradeState, revision: &str, step: &str) {
    // append a done entry without any details
    state.record(AppliedStep {
        revision: revision.to_owned(),
        step: step.to_owned(),
        outcome: StepState::Done,
        ..AppliedStep::default()
    });
}

/// Run every step of a plan by recording it done, then advance the state to the target
///
/// # Arguments
///
/// * `catalog` - The catalog to plan over
/// * `state` - The state to move
/// * `target` - The revision to reach
fn run_to(catalog: &Catalog, state: &mut UpgradeState, target: &str) -> Vec<String> {
    // record each planned step done, in plan order
    let planned = catalog.plan(state, &rev(target)).expect("plan");
    for step in &planned {
        done(state, &step.revision, &step.step);
    }
    // every edge on the path finished, so the revision reaches the target
    catalog.advance(state, &rev(target), "0.0.0");
    assert_eq!(state.revision, rev(target));
    planned.into_iter().map(|planned| planned.step).collect()
}

/// A catalog with one release edge plans that edge's steps between its two revisions
#[test]
fn one_release_edge_is_a_one_edge_path() {
    // the path from the baseline to the release is the release edge alone
    let catalog = fixtures::ONE_EDGE;
    let path = catalog.path(&rev(BASE), &rev(RELEASE)).expect("path");
    assert_eq!(
        path.iter()
            .map(|edge| (edge.from, edge.id))
            .collect::<Vec<_>>(),
        [(Some(BASE), RELEASE)]
    );
    // the plan holds that edge's steps, as forward steps of a release
    let state = UpgradeState::new(rev(BASE), "0.0.0");
    let planned = catalog.plan(&state, &rev(RELEASE)).expect("plan");
    assert_eq!(planned.len(), 1);
    assert_eq!(planned[0].step, "release-step");
    assert_eq!(planned[0].revision, RELEASE);
    assert!(!planned[0].prerelease && !planned[0].reverse);
    // a path to the revision a deployment is at is empty
    assert!(
        catalog
            .path(&rev(RELEASE), &rev(RELEASE))
            .expect("path")
            .is_empty()
    );
}

/// A skip edge is taken over the chain of release edges it skips
#[test]
fn skip_edge_beats_the_chain() {
    // both the chain and the skip edge reach the next release, and the skip edge wins
    let catalog = fixtures::SKIP;
    let path = catalog.path(&rev(BASE), &rev(NEXT)).expect("path");
    assert_eq!(
        path.iter()
            .map(|edge| (edge.from, edge.id))
            .collect::<Vec<_>>(),
        [(Some(BASE), NEXT)]
    );
    let state = UpgradeState::new(rev(BASE), "0.0.0");
    assert_eq!(planned_ids(&catalog, &state, NEXT), ["skip-step"]);
    // a deployment already at the middle release takes the release edge
    let middle = UpgradeState::new(rev(RELEASE), "0.0.0");
    assert_eq!(planned_ids(&catalog, &middle, NEXT), ["next-step"]);
}

/// A target with no path from the recorded revision fails naming both, and nothing moves
#[test]
fn no_path_names_both_revisions() {
    // the skip catalog has no edge back down from the newest release
    let catalog = fixtures::SKIP;
    let mut state = UpgradeState::new(rev(NEXT), "0.0.0");
    let error = catalog.plan(&state, &rev(BASE)).expect_err("no path");
    assert_eq!(
        error,
        UpgradeError::NoPath {
            from: rev(NEXT),
            to: rev(BASE)
        }
    );
    let message = error.to_string();
    assert!(message.contains(NEXT) && message.contains(BASE));
    // the revision doesn't move and no step was planned to run
    assert!(!catalog.advance(&mut state, &rev(BASE), "0.0.0"));
    assert_eq!(state.revision, rev(NEXT));
    // the production catalog has no edge back either, so an older target has no path
    let production = UpgradeState::new(rev("2026-10-v02"), "0.0.0");
    assert!(matches!(
        Catalog::PRODUCTION.plan(&production, &rev("2026-10-v01")),
        Err(UpgradeError::NoPath { .. })
    ));
}

/// Pre-release ids parse and sort between their base and the next release, and a stack of
/// pre-release edges is followed up and back down
#[test]
fn prerelease_ids_parse_sort_and_stack() {
    // suffixed ids parse and know their base
    let pre = rev("2026-10-v02-pre.users-theme");
    assert!(pre.is_prerelease());
    assert_eq!(pre.base(), rev("2026-10-v02"));
    assert!(!rev("2026-10-v02").is_prerelease());
    assert_eq!(rev("2026-10-v02").base(), rev("2026-10-v02"));
    // malformed suffixes are rejected
    for bad in [
        "2026-10-v02-pre.",
        "2026-10-v02-pre.Users",
        "2026-10-v02-pre.users--theme",
        "2026-10-v02-pre.users-",
        "2026-10-v02-pre.-users",
        "2026-10-v02-pre.users_theme",
        "2026-10-v02-rc.1",
        "2026-10-v02pre.x",
        "2026-13-v02-pre.x",
    ] {
        assert!(bad.parse::<RevisionId>().is_err(), "{bad} should fail");
    }
    // an id too long for the state's revision label is rejected
    let long = format!("2026-10-v02-pre.{}", "a".repeat(48));
    assert!(long.parse::<RevisionId>().is_err());
    assert!(long[..63].parse::<RevisionId>().is_ok());
    // a pre-release sorts after its base and before the next release
    assert!(rev("2026-10-v02") < pre && pre < rev("2026-10-v03"));
    assert!(pre < rev("2026-11-v01"));
    // pre-release ids round trip through serde
    let json = serde_json::to_string(&pre).expect("serialize");
    assert_eq!(
        serde_json::from_str::<RevisionId>(&json).expect("parse"),
        pre
    );
    // the stack is followed up from its base, one pre-release edge at a time
    let catalog = fixtures::SERIES;
    let mut state = UpgradeState::new(rev(RELEASE), "0.0.0");
    let up = catalog.plan(&state, &rev(STACK[2])).expect("plan");
    assert_eq!(
        up.iter().map(|step| step.step.as_str()).collect::<Vec<_>>(),
        ["stack-1-step", "stack-2-step", "stack-3-step"]
    );
    assert!(up.iter().all(|step| step.prerelease && !step.reverse));
    run_to(&catalog, &mut state, STACK[2]);
    // and back down through each revert edge, undoing the newest pre-release first
    let down = catalog.plan(&state, &rev(RELEASE)).expect("plan");
    assert_eq!(
        down.iter()
            .map(|step| step.step.as_str())
            .collect::<Vec<_>>(),
        ["stack-3-revert", "stack-2-revert", "stack-1-revert"]
    );
    assert!(down.iter().all(|step| step.prerelease && step.reverse));
    assert_eq!(down[0].revision, STACK[1]);
    run_to(&catalog, &mut state, RELEASE);
    // going up again runs every forward step again, although their old entries remain
    assert_eq!(
        run_to(&catalog, &mut state, STACK[1]),
        ["stack-1-step", "stack-2-step"]
    );
    // a deployment that reached a pre-release through the catalog plans its reverts
    let reached = catalog.reached(&rev(STACK[1]), "0.0.0");
    assert_eq!(
        planned_ids(&catalog, &reached, RELEASE),
        ["stack-2-revert", "stack-1-revert"]
    );
    // and going down again runs the reverts again
    assert_eq!(
        run_to(&catalog, &mut state, RELEASE),
        ["stack-2-revert", "stack-1-revert"]
    );
}

/// Pre-release edges are planned only for dev clusters following their chain or for explicit
/// targets, and auto targeting never picks a sibling
#[test]
fn prerelease_edges_only_for_dev_or_explicit_targets() {
    // a cluster without a target and without auto targeting plans nothing
    let catalog = fixtures::SERIES;
    let release = rev(RELEASE);
    assert_eq!(catalog.resolve_target(None, false, &release), Ok(None));
    // a dev cluster follows the primary chain to the top of the stack, never to the sibling
    assert_eq!(
        catalog.resolve_target(None, true, &release),
        Ok(Some(rev(STACK[4])))
    );
    assert!(rev(SIBLING) > rev(STACK[4]));
    assert_eq!(fixtures::SERIES.latest(), rev(STACK[4]));
    assert_eq!(
        catalog.resolve_target(None, true, &rev(STACK[3])),
        Ok(Some(rev(STACK[4])))
    );
    // a dev cluster on the sibling isn't moved off it by auto targeting
    assert_eq!(
        catalog.resolve_target(None, true, &rev(SIBLING)),
        Ok(Some(rev(SIBLING)))
    );
    // the sibling is reached only by an explicit target, through the fourth pre-release
    assert_eq!(
        catalog.resolve_target(Some(SIBLING), false, &release),
        Ok(Some(rev(SIBLING)))
    );
    let state = UpgradeState::new(release.clone(), "0.0.0");
    assert_eq!(
        planned_ids(&catalog, &state, SIBLING),
        [
            "stack-1-step",
            "stack-2-step",
            "stack-3-step",
            "stack-4-step",
            "stack-5b-step"
        ]
    );
    // an explicit pre-release target is planned on a cluster without auto targeting
    assert_eq!(
        catalog.resolve_target(Some(STACK[0]), false, &release),
        Ok(Some(rev(STACK[0])))
    );
    // an explicit target the catalog doesn't know is refused
    assert_eq!(
        catalog.resolve_target(Some("2026-10-v02-pre.gone"), true, &release),
        Err(UpgradeError::UnknownRevision(
            "2026-10-v02-pre.gone".to_owned()
        ))
    );
    // the production catalog has no sibling edges, so its dev clusters target its latest
    let production = Catalog::PRODUCTION;
    let latest = production.latest();
    assert!(production.edges.iter().all(|edge| !edge.sibling));
    assert_eq!(
        production.resolve_target(None, true, &rev(BASE)),
        Ok(Some(latest.clone()))
    );
    assert_eq!(super::resolve_target(None, true), Ok(Some(latest)));
    assert_eq!(super::resolve_target(None, false), Ok(None));
}

/// A catalog without a revert edge for a pre-release edge, or with a revert step that undoes a
/// step of another edge, fails the catalog check
#[test]
fn catalog_check_requires_revert_edges_and_downs() {
    // the production and fixture catalogs pass
    for catalog in [
        Catalog::PRODUCTION,
        fixtures::ONE_EDGE,
        fixtures::SKIP,
        fixtures::SERIES,
    ] {
        assert_eq!(catalog.check(), Ok(()));
    }
    // a stacked pre-release edge without its revert edge fails, naming the edge
    let without_revert: Vec<CatalogEntry> = fixtures::SERIES
        .edges
        .iter()
        .filter(|edge| !(edge.from == Some(STACK[2]) && edge.id == STACK[1]))
        .copied()
        .collect();
    let catalog = Catalog {
        edges: without_revert.leak(),
        floors: &[],
    };
    let problems = catalog.check().expect_err("missing revert edge");
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains(STACK[2]) && problem.contains("no revert edge")),
        "{problems:?}"
    );
    // a revert step naming a step of another edge fails
    let stray = StepDef {
        id: "stray-revert",
        kind: StepKind::Config,
        description: "A revert of the wrong step",
        reverses: Some("stack-3-step"),
        ..StepDef::BASE
    };
    let edges: Vec<CatalogEntry> = fixtures::SERIES
        .edges
        .iter()
        .map(|edge| {
            if edge.from == Some(STACK[0]) && edge.id == RELEASE {
                CatalogEntry {
                    steps: vec![stray].leak(),
                    ..*edge
                }
            } else {
                *edge
            }
        })
        .collect();
    let problems = Catalog {
        edges: edges.leak(),
        floors: &[],
    }
    .check()
    .expect_err("stray revert");
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("stray-revert")),
        "{problems:?}"
    );
}

/// A manual step without a procedure that isn't blank fails the catalog check with a problem
/// naming the step, and so does any other step needing an approval whose procedure is blank
#[test]
fn catalog_check_requires_manual_procedures() {
    // the production catalog and a manual step that says what to do pass
    assert_eq!(Catalog::PRODUCTION.check(), Ok(()));
    assert_eq!(fixtures::MANUAL_PROCEDURE.check(), Ok(()));
    // a manual step with no procedure, an empty one, or only whitespace fails, naming the step
    for catalog in [
        fixtures::MANUAL_NO_PROCEDURE,
        fixtures::MANUAL_EMPTY_PROCEDURE,
        fixtures::MANUAL_BLANK_PROCEDURE,
    ] {
        let problems = catalog
            .check()
            .expect_err("manual step without a procedure");
        assert!(
            problems.iter().any(|problem| {
                problem.contains(fixtures::PROCEDURE_STEP)
                    && problem.contains("manual step")
                    && problem.contains("manual procedure")
            }),
            "{problems:?}"
        );
    }
    // a data step whose procedure is only whitespace fails, naming the step
    let problems = fixtures::DATA_BLANK_PROCEDURE
        .check()
        .expect_err("data step with a blank procedure");
    assert!(
        problems.iter().any(|problem| {
            problem.contains(fixtures::PROCEDURE_STEP) && problem.contains("manual procedure")
        }),
        "{problems:?}"
    );
}

/// Build a fixture edge with one forward step of the same name as the revision it reaches
///
/// # Arguments
///
/// * `from` - The revision the edge starts from
/// * `id` - The revision the edge reaches
fn diamond_edge(from: Option<&'static str>, id: &'static str) -> CatalogEntry {
    // a release edge whose one step is named after the revision it reaches
    let steps: &'static [StepDef] = match from {
        Some(_) => vec![StepDef {
            id,
            kind: StepKind::Config,
            description: "A fixture step",
            ..StepDef::BASE
        }]
        .leak(),
        None => &[],
    };
    CatalogEntry {
        from,
        id,
        released_with: "0.0.0",
        description: "A fixture edge",
        schema: None,
        prerelease: false,
        sibling: false,
        steps,
    }
}

/// Between paths with the same number of edges the one whose first edge comes earlier in the
/// catalog wins, and a longer path never beats a shorter one declared later
#[test]
fn equal_length_paths_take_the_earlier_catalog_edge() {
    // a diamond from the baseline to the next release through either middle release
    let (top, left, right, bottom) = ("2026-01-v01", "2026-02-v01", "2026-03-v01", "2026-04-v01");
    let diamond = |order: &[(&'static str, &'static str)]| Catalog {
        edges: std::iter::once(diamond_edge(None, top))
            .chain(order.iter().map(|(from, id)| diamond_edge(Some(*from), id)))
            .collect::<Vec<_>>()
            .leak(),
        floors: &[],
    };
    let ends = |catalog: &Catalog| {
        catalog
            .path(&rev(top), &rev(bottom))
            .expect("path")
            .iter()
            .map(|edge| (edge.from, edge.id))
            .collect::<Vec<_>>()
    };
    // the left branch is declared first, so it wins
    let left_first = diamond(&[(top, left), (top, right), (left, bottom), (right, bottom)]);
    assert_eq!(ends(&left_first), [(Some(top), left), (Some(left), bottom)]);
    // declaring the right branch first flips the choice
    let right_first = diamond(&[(top, right), (top, left), (left, bottom), (right, bottom)]);
    assert_eq!(
        ends(&right_first),
        [(Some(top), right), (Some(right), bottom)]
    );
    // the order of the first edges decides, not the order of the edges into the target
    let mixed = diamond(&[(right, bottom), (top, left), (left, bottom), (top, right)]);
    assert_eq!(ends(&mixed), [(Some(top), left), (Some(left), bottom)]);
    // a three-edge chain declared first loses to a two-edge shortcut declared after it
    let shortcut = diamond(&[
        (top, left),
        (left, right),
        (right, bottom),
        (top, "2026-03-v02"),
        ("2026-03-v02", bottom),
    ]);
    assert_eq!(
        ends(&shortcut),
        [(Some(top), "2026-03-v02"), (Some("2026-03-v02"), bottom)]
    );
}

/// A reset onto the sibling of the stack's tip lowers the tip's step by the graph, although the
/// sibling's id sorts after the tip's
#[test]
fn reset_to_the_sibling_lowers_the_stack_tip() {
    // a deployment at the tip of the stack is reset onto the sibling
    let catalog = fixtures::SERIES;
    let mut tip = catalog.reached(&rev(STACK[4]), "0.0.0");
    done(&mut tip, SIBLING, RESET_STATE_STEP);
    // the tip's step is no longer finished, while the steps both share stay finished
    assert!(rev(SIBLING) > rev(STACK[4]));
    assert!(!catalog.finished(&tip, STACK[4], "stack-5-step"));
    assert!(catalog.finished(&tip, STACK[3], "stack-4-step"));
    // a reset the other way lowers the sibling's step
    let mut sibling = catalog.reached(&rev(SIBLING), "0.0.0");
    done(&mut sibling, STACK[4], RESET_STATE_STEP);
    assert!(!catalog.finished(&sibling, SIBLING, "stack-5b-step"));
}

/// A step of a downward edge that reverses nothing is planned again once a forward step raises
/// the deployment back above it
#[test]
fn downward_record_steps_are_replanned_after_a_re_raise() {
    // a pre-release whose revert edge also records the lowered revision
    let (base, pre) = ("2026-01-v01", "2026-01-v01-pre.x");
    let step = |id: &'static str, reverses: Option<&'static str>| StepDef {
        id,
        kind: StepKind::Config,
        description: "A fixture step",
        reverses,
        ..StepDef::BASE
    };
    let entry = |from: Option<&'static str>, id: &'static str, steps: Vec<StepDef>| CatalogEntry {
        from,
        id,
        released_with: "0.0.0",
        description: "A fixture edge",
        schema: None,
        prerelease: from.is_some(),
        sibling: false,
        steps: steps.leak(),
    };
    let catalog = Catalog {
        edges: vec![
            entry(None, base, Vec::new()),
            entry(Some(base), pre, vec![step("x-up", None)]),
            entry(
                Some(pre),
                base,
                vec![step("x-down", Some("x-up")), step("x-record", None)],
            ),
        ]
        .leak(),
        floors: &[],
    };
    assert_eq!(catalog.check(), Ok(()));
    // go up and back down, running both steps of the revert edge
    let mut state = UpgradeState::new(rev(base), "0.0.0");
    run_to(&catalog, &mut state, pre);
    assert_eq!(run_to(&catalog, &mut state, base), ["x-down", "x-record"]);
    assert!(!catalog.finished(&state, pre, "x-up"));
    // going up again raises the deployment above the record step, so both are planned again
    assert_eq!(run_to(&catalog, &mut state, pre), ["x-up"]);
    assert!(!catalog.finished(&state, base, "x-record"));
    assert_eq!(planned_ids(&catalog, &state, base), ["x-down", "x-record"]);
}

/// A revert step lowers only the forward step it reverses, leaving the other steps of the same
/// edge finished
#[test]
fn a_revert_step_lowers_only_what_it_reverses() {
    // a deployment at the paired pre-release reverts only its second step
    let catalog = fixtures::PAIRED;
    assert_eq!(catalog.check(), Ok(()));
    let mut state = UpgradeState::new(rev(RELEASE), "0.0.0");
    run_to(&catalog, &mut state, fixtures::PAIR);
    done(&mut state, RELEASE, "pair-second-revert");
    // the reverted step is unfinished and the first step of the same edge is still finished
    assert!(!catalog.finished(&state, fixtures::PAIR, "pair-second-step"));
    assert!(catalog.finished(&state, fixtures::PAIR, "pair-first-step"));
    // so finishing the revert plans only the first step's revert
    assert_eq!(
        planned_ids(&catalog, &state, RELEASE),
        ["pair-first-revert"]
    );
}

/// Replace the steps of the first pre-release edge of the branch catalog and check the result
///
/// # Arguments
///
/// * `step` - The one step the edge gets
#[cfg(feature = "scylla-utils")]
fn check_with_first_step(step: StepDef) -> Result<(), Vec<String>> {
    // swap the forward edge's steps, keeping every other edge
    let steps: &'static [StepDef] = vec![step].leak();
    let edges: Vec<CatalogEntry> = fixtures::SERIES
        .edges
        .iter()
        .map(|edge| {
            if edge.id == STACK[0] && !edge.is_downward() {
                CatalogEntry { steps, ..*edge }
            } else {
                *edge
            }
        })
        .collect();
    Catalog {
        edges: edges.leak(),
        floors: &[],
    }
    .check()
}

/// A pre-release transform without a `down` fails the catalog check unless its step is declared
/// irreversible with a justification
#[cfg(feature = "scylla-utils")]
#[test]
fn catalog_check_requires_transform_downs() {
    // a transform without a down fails
    /// The first pre-release's transform with its `down` removed
    static NO_DOWN: super::TransformDef = super::TransformDef {
        down: None,
        ..fixtures::THEME
    };
    let no_down = StepDef {
        transform: Some(&NO_DOWN),
        ..fixtures::SERIES.edges[2].steps[0]
    };
    let problems = check_with_first_step(no_down).expect_err("transform without down");
    assert!(
        problems.iter().any(|problem| problem.contains("no `down`")),
        "{problems:?}"
    );
    // declaring the step irreversible with a justification is allowed
    let irreversible = StepDef {
        reversibility: Some(super::Reversibility::Irreversible {
            justification: "the old form can't hold the new data",
        }),
        ..no_down
    };
    assert_eq!(check_with_first_step(irreversible), Ok(()));
    // an empty justification doesn't count
    let unjustified = StepDef {
        reversibility: Some(super::Reversibility::Irreversible { justification: " " }),
        ..no_down
    };
    assert!(check_with_first_step(unjustified).is_err());
}

/// A main image that finds a pre-release revision it doesn't know refuses, naming the base and
/// the revert command
#[test]
fn unknown_prerelease_names_base_and_revert_command() {
    // the production catalog doesn't know another branch's pre-release
    let state = UpgradeState::new(rev("2026-10-v02-pre.other-branch"), "0.0.0");
    let error = super::plan(&state, &rev("2026-10-v02")).expect_err("unknown pre-release");
    assert_eq!(
        error,
        UpgradeError::UnknownPrerelease {
            current: rev("2026-10-v02-pre.other-branch"),
            base: rev("2026-10-v02"),
        }
    );
    let message = error.to_string();
    assert!(message.contains("2026-10-v02-pre.other-branch"));
    assert!(message.contains("thoradm migrate revert --to 2026-10-v02"));
    // a pre-release on a base newer than the catalog is still a pre-release to revert
    let ahead = UpgradeState::new(rev("2099-01-v01-pre.x"), "0.0.0");
    assert!(matches!(
        super::plan(&ahead, &rev("2026-10-v02")),
        Err(UpgradeError::UnknownPrerelease { .. })
    ));
    // a branch catalog refuses another branch's pre-release on the same base
    let other = UpgradeState::new(rev("2026-10-v02-pre.gone"), "0.0.0");
    assert!(matches!(
        fixtures::SERIES.plan(&other, &rev(RELEASE)),
        Err(UpgradeError::UnknownPrerelease { .. })
    ));
}

/// Every pre-release transform round trips, and re-upgrading after a revert derives the new
/// field from the old one even when a stale new field is present
#[cfg(feature = "scylla-utils")]
#[test]
fn prerelease_transforms_round_trip() {
    // every pre-release step with a transform in the branch catalog round trips on its fixtures
    let catalog = fixtures::SERIES;
    let transforms = catalog
        .steps()
        .filter(|(edge, _)| edge.prerelease && !edge.is_downward())
        .filter_map(|(_, step)| step.transform)
        .collect::<Vec<_>>();
    assert!(!transforms.is_empty());
    for transform in &transforms {
        for fixture in fixtures::theme_fixtures() {
            assert_eq!(transform.check_round_trip(&fixture), Ok(()));
        }
    }
    // a user upgraded, reverted, and then changed by the base code keeps its stale theme
    let theme = fixtures::THEME;
    let user = fixtures::theme_fixtures().remove(0);
    let upgraded = (theme.up)(user).expect("up");
    let mut reverted = theme.down.expect("down")(upgraded).expect("down");
    let super::Record::Redis { fields, .. } = &mut reverted else {
        panic!("a user is a Redis record");
    };
    fields.insert("dark".to_owned(), "false".to_owned());
    // classifying alone would take the stale theme as current
    assert_eq!((theme.classify)(&reverted), super::RecordClass::Current);
    // upgrading it again derives the theme from the changed flag
    let again = (theme.up)(reverted).expect("up after a revert");
    let super::Record::Redis { fields, .. } = again else {
        panic!("a user is a Redis record");
    };
    assert_eq!(fields["theme"], "light");
    // a transform with no down can't round trip
    let no_down = super::TransformDef {
        down: None,
        ..theme
    };
    assert!(
        no_down
            .check_round_trip(&fixtures::theme_fixtures()[0])
            .is_err()
    );
}

/// A step counts as finished only when its entry is newer than the latest entry that lowered
/// the revision below the step's revision
#[test]
fn finished_steps_respect_lowering_entries() {
    // a release's steps all done, then a reset to the baseline
    let catalog = fixtures::ONE_EDGE;
    let mut state = UpgradeState::new(rev(BASE), "0.0.0");
    done(&mut state, RELEASE, "release-step");
    assert!(catalog.finished(&state, RELEASE, "release-step"));
    done(&mut state, BASE, RESET_STATE_STEP);
    // the reset makes the release's step unfinished again, so it is planned and nothing advances
    assert!(!catalog.finished(&state, RELEASE, "release-step"));
    assert_eq!(planned_ids(&catalog, &state, RELEASE), ["release-step"]);
    assert!(!catalog.advance(&mut state, &rev(RELEASE), "0.0.0"));
    // running the step again finishes it
    done(&mut state, RELEASE, "release-step");
    assert!(catalog.finished(&state, RELEASE, "release-step"));
    // a restore to the release itself doesn't lower anything at or below it
    done(&mut state, RELEASE, RESTORED_STEP);
    assert!(catalog.finished(&state, RELEASE, "release-step"));
    // a restore to the baseline does
    done(&mut state, BASE, RESTORED_STEP);
    assert!(!catalog.finished(&state, RELEASE, "release-step"));
    // entries that aren't done or skipped lower nothing
    let mut failed = UpgradeState::new(rev(BASE), "0.0.0");
    done(&mut failed, RELEASE, "release-step");
    failed.record(AppliedStep {
        revision: BASE.to_owned(),
        step: RESET_STATE_STEP.to_owned(),
        outcome: StepState::Failed,
        ..AppliedStep::default()
    });
    assert!(catalog.finished(&failed, RELEASE, "release-step"));
    // reverting one pre-release step leaves the steps below it finished
    let series = fixtures::SERIES;
    let mut stacked = UpgradeState::new(rev(RELEASE), "0.0.0");
    run_to(&series, &mut stacked, STACK[1]);
    done(&mut stacked, STACK[0], "stack-2-revert");
    assert!(!series.finished(&stacked, STACK[1], "stack-2-step"));
    assert!(series.finished(&stacked, STACK[0], "stack-1-step"));
    // the production state's own check applies the same rule
    let mut production = UpgradeState::new(rev("2026-10-v01"), "0.0.0");
    done(&mut production, "2026-10-v02", "finalize");
    assert!(production.finished("2026-10-v02", "finalize"));
    done(&mut production, "2026-10-v01", RESET_STATE_STEP);
    assert!(!production.finished("2026-10-v02", "finalize"));
}

/// A revision below the catalog's floor is refused naming the minimum and the release to install
/// first, after the unknown pre-release check
#[test]
fn below_minimum_names_the_intermediate_release() {
    // a revision below both floors names the intermediate of the lowest floor it is below
    let catalog = fixtures::FLOORED;
    let mut oldest = UpgradeState::new(rev("2025-06-v01"), "0.0.0");
    let error = catalog
        .plan(&oldest, &rev("2028-01-v01"))
        .expect_err("below the floor");
    assert_eq!(
        error,
        UpgradeError::BelowMinimum {
            current: rev("2025-06-v01"),
            minimum: rev(FLOOR),
            intermediate: "1.5.0".to_owned(),
        }
    );
    // the message names the revision, the minimum, the release to install, and the table
    let message = error.to_string();
    for part in [
        "2025-06-v01",
        FLOOR,
        "install Thorium 1.5.0 first",
        "table of valid upgrade paths",
    ] {
        assert!(message.contains(part), "{message} should name {part}");
    }
    // nothing runs and the revision doesn't move
    assert!(!catalog.advance(&mut oldest, &rev("2028-01-v01"), "0.0.0"));
    assert_eq!(oldest.revision, rev("2025-06-v01"));
    // a revision between the floors names the newer floor's intermediate
    let between = UpgradeState::new(rev("2026-05-v01"), "0.0.0");
    assert!(matches!(
        catalog.plan(&between, &rev("2028-01-v01")),
        Err(UpgradeError::BelowMinimum { intermediate, .. }) if intermediate == "1.9.0"
    ));
    // a pre-release whose base is below the floor gets the revert instruction instead
    let branch = UpgradeState::new(rev("2025-06-v01-pre.theme"), "0.0.0");
    assert!(matches!(
        catalog.plan(&branch, &rev("2028-01-v01")),
        Err(UpgradeError::UnknownPrerelease { .. })
    ));
    // the floor itself and everything after it plan as usual
    assert_eq!(catalog.check_floor(&rev(FLOOR)), Ok(()));
    let floor = UpgradeState::new(rev(FLOOR), "0.0.0");
    assert_eq!(
        planned_ids(&catalog, &floor, "2028-01-v01"),
        ["april-step", "july-step", "october-step", "january-step"]
    );
    // a revision above the floor that the catalog doesn't know is still unknown
    let gap = UpgradeState::new(rev("2027-02-v01"), "0.0.0");
    assert!(matches!(
        catalog.plan(&gap, &rev("2028-01-v01")),
        Err(UpgradeError::UnknownRevision(_))
    ));
}

/// The pre-Helm conversion script carries the catalog's floor and refuses to record a baseline
/// below it
#[cfg(unix)]
#[test]
fn conversion_script_refuses_baselines_below_the_floor() {
    // read the script and the floor values it ships with
    let script_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../deploy/charts/scripts/convert-to-helm.sh"
    );
    let script = std::fs::read_to_string(script_path).expect("read convert-to-helm.sh");
    let value = |name: &str| {
        script
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")))
            .unwrap_or_else(|| panic!("{name} is set in the script"))
            .trim_matches('"')
            .to_owned()
    };
    // the script's floor is the production catalog's
    let floor = Catalog::PRODUCTION.floors.last();
    assert_eq!(
        value("MINIMUM_REVISION"),
        floor.map_or("", |floor| floor.minimum)
    );
    assert_eq!(
        value("INTERMEDIATE_RELEASE"),
        floor.map_or("", |floor| floor.intermediate)
    );
    // the baseline it records is a revision the catalog plans from
    let baseline = rev(&value("BASELINE_REVISION"));
    assert!(
        CATALOG
            .iter()
            .any(|edge| edge.from.is_none() && edge.revision() == baseline)
    );
    assert_eq!(Catalog::PRODUCTION.check_floor(&baseline), Ok(()));
    // a copy of the script whose floor is above its baseline refuses before touching anything
    let dir = std::env::temp_dir().join(format!("thorium-convert-floor-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create a scratch dir");
    let copy = dir.join("convert-to-helm.sh");
    let raised = script
        .replace("MINIMUM_REVISION=\"\"", "MINIMUM_REVISION=\"2027-01-v01\"")
        .replace(
            "INTERMEDIATE_RELEASE=\"\"",
            "INTERMEDIATE_RELEASE=\"1.9.0\"",
        );
    std::fs::write(&copy, raised).expect("write the copy");
    // put kubectl and helm stubs first on the PATH that fail loudly and leave a marker if called
    let stubs = dir.join("bin");
    std::fs::create_dir_all(&stubs).expect("create the stub dir");
    let marker = dir.join("cluster-tool-called");
    for tool in ["kubectl", "helm"] {
        let stub = stubs.join(tool);
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\necho \"{tool} $*\" >> '{}'\necho '{tool} must not run' >&2\nexit 99\n",
                marker.display()
            ),
        )
        .expect("write a stub");
        std::fs::set_permissions(
            &stub,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .expect("make a stub executable");
    }
    let output = std::process::Command::new("bash")
        .arg(&copy)
        .arg("--dry-run")
        .env("PATH", format!("{}:/usr/bin:/bin", stubs.display()))
        .output()
        .expect("run the copy");
    let called = marker.exists();
    std::fs::remove_dir_all(&dir).expect("remove the scratch dir");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    // the refusal comes before any cluster tool runs
    assert!(!called, "kubectl or helm ran before the refusal: {stderr}");
    for part in [baseline.as_str(), "2027-01-v01", "1.9.0"] {
        assert!(stderr.contains(part), "{stderr} should name {part}");
    }
}

/// Build a catalog of release edges, oldest first, from a baseline at the first revision
///
/// # Arguments
///
/// * `revisions` - The revisions, oldest first
/// * `raised_by` - The Thorium version the last revision was released with
/// * `floors` - The catalog's floors
fn releases(
    revisions: &[&'static str],
    raised_by: &'static str,
    floors: &'static [SupportFloor],
) -> Catalog {
    // a baseline at the first revision, then one release edge to each later revision
    let mut edges = vec![CatalogEntry {
        from: None,
        id: revisions[0],
        released_with: "0.0.0",
        description: "A fixture baseline",
        schema: None,
        prerelease: false,
        sibling: false,
        steps: &[],
    }];
    for pair in revisions.windows(2) {
        edges.push(CatalogEntry {
            from: Some(pair[0]),
            id: pair[1],
            ..edges[0]
        });
    }
    // the newest revision is the release that raised the last floor
    if let Some(last) = edges.last_mut() {
        last.released_with = raised_by;
    }
    Catalog {
        edges: edges.leak(),
        floors,
    }
}

/// A floor with only three revisions above it over nine months
const RAISED: &[SupportFloor] = &[SupportFloor {
    minimum: "2027-04-v01",
    intermediate: "1.9.0",
    since_version: "2.0.0",
}];

/// A floor with five revisions above it over only five months
const BUSY_FLOOR: &[SupportFloor] = &[SupportFloor {
    minimum: "2027-07-v01",
    intermediate: "1.9.0",
    since_version: "2.0.0",
}];

/// A floor with one revision above it over seventeen months
const OLD_FLOOR: &[SupportFloor] = &[SupportFloor {
    minimum: "2026-01-v01",
    intermediate: "1.9.0",
    since_version: "2.0.0",
}];

/// A floor with four revisions above it over only eleven months
const ELEVEN_MONTHS: &[SupportFloor] = &[SupportFloor {
    minimum: "2027-02-v01",
    intermediate: "1.9.0",
    since_version: "2.0.0",
}];

/// Floors listed newest first, whose last minimum is no baseline
const UNORDERED: &[SupportFloor] = &[
    SupportFloor {
        minimum: "2027-01-v01",
        intermediate: "1.9.0",
        since_version: "2.0.0",
    },
    SupportFloor {
        minimum: "2026-01-v01",
        intermediate: "1.5.0",
        since_version: "1.6.0",
    },
];

/// A floor must leave the previous four revisions and the last twelve months of revisions above
/// it, and the floors must be ordered with the last one at a baseline
#[test]
fn floors_keep_the_support_window() {
    // the floored fixture leaves four revisions over twelve months above its floor
    assert_eq!(fixtures::FLOORED.check(), Ok(()));
    // a floor with only three revisions above it over nine months fails both rules
    let short = releases(
        &["2027-04-v01", "2027-07-v01", "2027-10-v01", "2028-01-v01"],
        "2.0.0",
        RAISED,
    );
    let problems = short.check().expect_err("too few revisions");
    assert!(
        problems.iter().any(|problem| problem.contains("four")),
        "{problems:?}"
    );
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("twelve months")),
        "{problems:?}"
    );
    // many revisions in a few months still leave the twelve months uncovered
    let busy = releases(
        &[
            "2027-07-v01",
            "2027-07-v02",
            "2027-08-v01",
            "2027-09-v01",
            "2027-10-v01",
            "2027-12-v01",
        ],
        "2.0.0",
        BUSY_FLOOR,
    );
    let problems = busy.check().expect_err("window too short");
    assert!(
        problems.iter().all(|problem| !problem.contains("four")),
        "{problems:?}"
    );
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("twelve months")),
        "{problems:?}"
    );
    // four revisions over eleven months fail only the twelve months
    let eleven = releases(
        &[
            "2027-02-v01",
            "2027-04-v01",
            "2027-07-v01",
            "2027-10-v01",
            "2028-01-v01",
        ],
        "2.0.0",
        ELEVEN_MONTHS,
    );
    let problems = eleven.check().expect_err("window one month short");
    assert!(
        problems.iter().all(|problem| !problem.contains("four")),
        "{problems:?}"
    );
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("twelve months")),
        "{problems:?}"
    );
    // a long gap with too few revisions fails only the count
    let sparse = releases(&["2026-01-v01", "2027-06-v01"], "2.0.0", OLD_FLOOR);
    let problems = sparse.check().expect_err("too few revisions");
    assert!(
        problems.iter().any(|problem| problem.contains("four")),
        "{problems:?}"
    );
    assert!(
        problems
            .iter()
            .all(|problem| !problem.contains("twelve months")),
        "{problems:?}"
    );
    // floors out of order, or a last floor that isn't a baseline, fail
    let unordered = Catalog {
        floors: UNORDERED,
        ..fixtures::FLOORED
    };
    let problems = unordered.check().expect_err("unordered floors");
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("oldest first")),
        "{problems:?}"
    );
    assert!(
        problems.iter().any(|problem| problem.contains("baseline")),
        "{problems:?}"
    );
}

/// An edge from a pre-release to the release after its base is a pre-release edge that passes the
/// catalog check, and a plan from the pre-release runs its steps
#[test]
fn a_prerelease_may_convert_into_the_next_release() {
    // the catalog check accepts the converting edge and its revert edge
    let catalog = fixtures::CONVERTING;
    assert_eq!(catalog.check(), Ok(()));
    // the converting edge is a pre-release edge going up and its revert edge one going down
    let forward = catalog.edge(CONVERT, NEXT).expect("converting edge");
    assert!(forward.prerelease && !forward.is_downward());
    let back = catalog.edge(NEXT, CONVERT).expect("revert edge");
    assert!(back.prerelease && back.is_downward());
    // a cluster on the pre-release plans the branch's step to the release, and its revert
    let state = UpgradeState::new(rev(CONVERT), "0.0.0");
    assert_eq!(planned_ids(&catalog, &state, NEXT), ["convert-step"]);
    let released = catalog.reached(&rev(NEXT), "0.0.0");
    assert_eq!(
        planned_ids(&catalog, &released, CONVERT),
        ["convert-revert"]
    );
}

/// An edge from a pre-release to a pre-release of another base still fails the catalog check
#[test]
fn a_prerelease_may_not_cross_to_another_bases_prerelease() {
    // the pre-release rule names the base the edge must stay on
    let problems = fixtures::CROSSING.check().expect_err("crossing edge");
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("must start from")),
        "{problems:?}"
    );
}
