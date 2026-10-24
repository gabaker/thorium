//! Tests of plan composition: converge steps, fused passes, barriers, supersession, the checks
//! made right before a step runs, and fused chains run over records in memory

use std::cell::Cell;

use super::fixtures::{self, BASE, NEXT, RELEASE, STACK};
use super::plan_fixtures::{
    self, CHECKED, CONTRACT_BETWEEN, CYCLE, EXAMPLE, EXPAND_KINDS, FUSED, FUSED_SELF_READS,
    INDEPENDENT, MANUAL_BETWEEN, PHASES, READS, READS_AFTER, REBUILD_BETWEEN, REVERSIBLE,
    SUPERSEDES_CHAIN, SUPERSEDES_DIAMOND, USERS, USERS_V2, USERS_V3, UserVariant,
};
use super::{
    AppliedStep, CATALOG, Catalog, ChainLink, ChainOutcome, Family, FusedChain, IssueCategory,
    IssueReason, NextWork, PlanDirection, PlanItem, PlannedPass, PlannedStep, RESET_STATE_STEP,
    Record, RecordClass, RevisionId, StepState, TransformDef, UpgradePlan, UpgradeState, Work,
};
use crate::models::schema::PassCheckpoint;

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
fn compose(catalog: &Catalog, from: &str, to: &str) -> UpgradePlan {
    // build the state of a deployment at the starting revision and compose its plan
    let state = catalog.reached(&rev(from), "0.0.0");
    catalog.compose(&state, &rev(to)).expect("compose")
}

/// Describe a plan's items as text, so a whole plan compares in one assertion
///
/// A converge is `converge <target> [steps]`, an item of passes is `passes` followed by each pass
/// as `<family> [links]`, and a single step is `step <id>`.
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

/// Whether a record belongs to a family
///
/// # Arguments
///
/// * `record` - The record
/// * `family` - The family
fn in_family(record: &Record, family: Family) -> bool {
    // Redis records are users and S3 records are files in these fixtures
    matches!(
        (record, family),
        (Record::Redis { .. }, Family::Redis(_)) | (Record::S3 { .. }, Family::S3Prefix { .. })
    )
}

/// What running a plan's passes over an in-memory store did to each object
#[derive(Debug, Default)]
struct Sweep {
    /// How many times each object was read, by its index in the store
    reads: Vec<u32>,
    /// How many times each object was written, by its index in the store
    writes: Vec<u32>,
    /// Each issue: the object's index, the failing step, and the reason
    issues: Vec<(usize, String, IssueReason)>,
}

/// Run every pass of a plan over an in-memory store, as a migration Job would: each pass reads
/// every object of its family once and writes the ones its chain converts
///
/// # Arguments
///
/// * `catalog` - The catalog the plan was composed over
/// * `plan` - The plan whose passes to run
/// * `store` - The objects, rewritten in place
fn sweep(catalog: &Catalog, plan: &UpgradePlan, store: &mut [Record]) -> Sweep {
    // count every object's reads and writes
    let mut sweep = Sweep {
        reads: vec![0; store.len()],
        writes: vec![0; store.len()],
        issues: Vec::new(),
    };
    for pass in passes(plan) {
        let chain = catalog.chain(pass).expect("the pass resolves");
        for (index, record) in store.iter_mut().enumerate() {
            // only objects of the pass's family are read
            if !in_family(record, pass.family) {
                continue;
            }
            sweep.reads[index] += 1;
            match chain.apply(record) {
                ChainOutcome::Current => (),
                ChainOutcome::Converted(converted) => {
                    *record = converted;
                    sweep.writes[index] += 1;
                }
                ChainOutcome::Issue { step, issue } => {
                    sweep.issues.push((index, step, issue.reason));
                }
            }
        }
    }
    sweep
}

/// Build a chain of single transforms the way a fused pass resolves them
///
/// # Arguments
///
/// * `links` - Each link's step id and transform
/// * `after_reverse` - Whether the chain runs in its after-a-reverse mode
fn chain(links: &[(&'static str, &'static TransformDef)], after_reverse: bool) -> FusedChain {
    // copy the links as given
    FusedChain {
        links: links.to_vec(),
        after_reverse,
        direction: PlanDirection::Upgrade,
        reversibility: Vec::new(),
    }
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

/// Two transforms on the same family with chained layouts on consecutive edges fuse into one
/// pass that reads and writes each object once
#[test]
fn chained_transforms_fuse_into_one_pass() {
    // the two users transforms become one pass whose chain runs layout 1 to 3
    let plan = compose(&FUSED, BASE, NEXT);
    assert_eq!(shape(&plan), ["passes redis:users [users-v2,users-v3]"]);
    let pass = passes(&plan)[0];
    assert_eq!(
        pass.chain,
        [
            ChainLink {
                revision: rev(RELEASE),
                step: "users-v2".to_owned(),
                from: 1,
                to: 2
            },
            ChainLink {
                revision: rev(NEXT),
                step: "users-v3".to_owned(),
                from: 2,
                to: 3
            },
        ]
    );
    // the status names the pass on each of its steps
    assert!(
        pass.steps
            .iter()
            .all(|step| step.pass.as_deref() == Some("redis:users"))
    );
    assert_eq!(plan.direction, PlanDirection::Upgrade);
    assert_eq!(plan.path, [rev(BASE), rev(RELEASE), rev(NEXT)]);
    // running it reads every user once and writes each user that wasn't current once
    let mut store = plan_fixtures::users(UserVariant::Layout1, 20);
    store.extend(plan_fixtures::users(UserVariant::Layout2, 20));
    store.extend(plan_fixtures::users(UserVariant::Layout3, 20));
    let sweep = sweep(&FUSED, &plan, &mut store);
    assert!(sweep.reads.iter().all(|reads| *reads == 1));
    assert!(sweep.writes[..40].iter().all(|writes| *writes == 1));
    assert!(sweep.writes[40..].iter().all(|writes| *writes == 0));
    assert_eq!(sweep.issues, Vec::new());
    // every user ends in layout 3
    assert!(
        store
            .iter()
            .all(|user| (USERS_V3.classify)(user) == RecordClass::Current
                && field(user, "theme").is_none())
    );
}

/// Transforms on different families with no `reads` dependency are planned as independent
/// passes
#[test]
fn independent_families_run_as_concurrent_passes() {
    // both passes sit in one item, so one Job runs them concurrently
    let plan = compose(&INDEPENDENT, BASE, NEXT);
    assert_eq!(
        shape(&plan),
        ["passes redis:users [users-v2] | s3:files/ [files-v2]"]
    );
    // each pass touches only its own family
    let mut store = plan_fixtures::users(UserVariant::Layout1, 5);
    store.extend(plan_fixtures::files(5));
    let sweep = sweep(&INDEPENDENT, &plan, &mut store);
    assert!(sweep.reads.iter().all(|reads| *reads == 1));
    assert_eq!(sweep.issues, Vec::new());
}

/// A transform that `reads` a family another transform writes is ordered after that pass, and a
/// `reads` cycle fails the catalog test
#[test]
fn readers_run_after_writers_and_cycles_fail_the_check() {
    // files read users, so their pass waits for the users pass before them; the later users
    // transform waits for the files pass, which read users in their earlier layout
    let plan = compose(&READS, BASE, NEXT);
    assert_eq!(
        shape(&plan),
        [
            "passes redis:users [users-v2]",
            "passes s3:files/ [files-v2]",
            "passes redis:users [users-v3]",
        ]
    );
    // a plan from the release has only the files reader and the later users transform
    assert_eq!(
        shape(&compose(&READS, RELEASE, NEXT)),
        [
            "passes s3:files/ [files-v2]",
            "passes redis:users [users-v3]"
        ]
    );
    // the catalog check accepts readers but refuses a cycle of reads
    assert_eq!(READS.check(), Ok(()));
    let problems = CYCLE.check().expect_err("a reads cycle");
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("reads cycle")),
        "{problems:?}"
    );
    // every other fixture catalog and the production catalog have no cycle
    for catalog in [
        FUSED,
        INDEPENDENT,
        CONTRACT_BETWEEN,
        MANUAL_BETWEEN,
        REBUILD_BETWEEN,
        EXAMPLE,
        PHASES,
        CHECKED,
        Catalog::PRODUCTION,
    ] {
        assert_eq!(catalog.check(), Ok(()));
    }
}

/// A contract step, manual step, or rebuild between two transforms on the same family stops
/// them from fusing
#[test]
fn barriers_stop_fusion() {
    // a contract step runs last, after both users passes
    assert_eq!(
        shape(&compose(&CONTRACT_BETWEEN, BASE, NEXT)),
        [
            "passes redis:users [users-v2]",
            "passes redis:users [users-v3]",
            "step drop-dark",
        ]
    );
    // a manual step and a rebuild run where they sit, between the passes
    assert_eq!(
        shape(&compose(&MANUAL_BETWEEN, BASE, NEXT)),
        [
            "passes redis:users [users-v2]",
            "step by-hand",
            "passes redis:users [users-v3]",
        ]
    );
    assert_eq!(
        shape(&compose(&REBUILD_BETWEEN, BASE, NEXT)),
        [
            "passes redis:users [users-v2]",
            "step rebuild-results",
            "passes redis:users [users-v3]",
        ]
    );
}

/// When a fused chain fails at link k for an object, the object's issue group names transform k
/// with its reason code, and the object is left at its original layout
#[test]
fn chain_failures_name_the_failing_link() {
    // a layout 1 user with a bad ai flag converts at the first link and fails at the second
    let chain = chain(&[("users-v2", &USERS_V2), ("users-v3", &USERS_V3)], false);
    let user = plan_fixtures::user(0, UserVariant::BadAi, true, None);
    let original = user.clone();
    assert_eq!(
        chain.apply(&user),
        ChainOutcome::Issue {
            step: "users-v3".to_owned(),
            issue: super::TransformIssue {
                reason: IssueReason::new(IssueCategory::InvalidValue, "ai"),
                layouts_tried: vec![2],
            },
        }
    );
    assert_eq!(user, original);
    // a user the first link can't read fails at the first link
    let user = plan_fixtures::user(1, UserVariant::BadDark, true, None);
    let ChainOutcome::Issue { step, issue } = chain.apply(&user) else {
        panic!("a bad dark flag is an issue");
    };
    assert_eq!(step, "users-v2");
    assert_eq!(issue.reason.to_string(), "invalid_value:dark");
    // running the pass writes neither, so both stay in layout 1, and the issues name the links
    let plan = compose(&FUSED, BASE, NEXT);
    let mut store = vec![
        plan_fixtures::user(0, UserVariant::BadAi, true, None),
        plan_fixtures::user(1, UserVariant::BadDark, true, None),
        plan_fixtures::user(2, UserVariant::Layout1, true, None),
    ];
    let before = store.clone();
    let sweep = sweep(&FUSED, &plan, &mut store);
    assert_eq!(store[..2], before[..2]);
    assert_eq!(sweep.writes, [0, 0, 1]);
    let named: Vec<(usize, &str, String)> = sweep
        .issues
        .iter()
        .map(|(index, step, reason)| (*index, step.as_str(), reason.to_string()))
        .collect();
    assert_eq!(
        named,
        [
            (0, "users-v3", "invalid_value:ai".to_owned()),
            (1, "users-v2", "invalid_value:dark".to_owned()),
        ]
    );
}

/// A checkpoint recorded for a different chain makes the planner restart that pass from the
/// beginning
#[test]
fn checkpoints_of_another_chain_restart_the_pass() {
    // the fused pass resumes only from a checkpoint of its own chain
    let fused = compose(&FUSED, BASE, NEXT);
    let pass = passes(&fused)[0];
    let own = PassCheckpoint {
        chain: pass.chain_ids(),
        slices: 4,
        ..PassCheckpoint::default()
    };
    assert_eq!(pass.resume_from(Some(&own)), Some(&own));
    // a checkpoint left by a plan that ran only the first edge's transform restarts the pass
    let first = compose(&FUSED, BASE, RELEASE);
    let earlier = PassCheckpoint {
        chain: passes(&first)[0].chain_ids(),
        slices: 4,
        ..PassCheckpoint::default()
    };
    assert_eq!(earlier.chain, ["users-v2"]);
    assert_eq!(pass.resume_from(Some(&earlier)), None);
    // so does a chain in another order, or no checkpoint at all
    let reordered = PassCheckpoint {
        chain: vec!["users-v3".to_owned(), "users-v2".to_owned()],
        ..PassCheckpoint::default()
    };
    assert_eq!(pass.resume_from(Some(&reordered)), None);
    assert_eq!(pass.resume_from(None), None);
}

/// A step listed in a later step's `supersedes` is dropped from the plan
#[test]
fn superseded_steps_are_dropped() {
    // the worked example: one converge, the fused users pass beside files, and one rebuild
    let state = EXAMPLE.reached(&rev(BASE), "0.0.0");
    let plan = EXAMPLE.compose(&state, &rev(NEXT)).expect("compose");
    assert_eq!(
        shape(&plan),
        [
            "converge 2026-10-v03 [add-samples-column,add-table]",
            "passes redis:users [users-v2,users-v3] | s3:files/ [files-v2]",
            "step rebuild-results-2",
        ]
    );
    // the dropped rebuild is reported with the step that supersedes it
    let dropped: Vec<(String, &str)> = EXAMPLE
        .superseded(&state, &rev(NEXT))
        .expect("superseded")
        .into_iter()
        .map(|(step, by)| (step.step, by))
        .collect();
    assert_eq!(
        dropped,
        [("rebuild-results-1".to_owned(), "rebuild-results-2")]
    );
    // a plan to the first release alone keeps its rebuild, which nothing later supersedes
    assert_eq!(
        shape(&compose(&EXAMPLE, BASE, RELEASE)),
        [
            "converge 2026-10-v02 [add-samples-column]",
            "passes redis:users [users-v2]",
            "step rebuild-results-1",
        ]
    );
    assert_eq!(
        EXAMPLE
            .superseded(&state, &rev(RELEASE))
            .expect("superseded"),
        Vec::new()
    );
}

/// A superseded step names the step that finally supersedes it and stays in the plan, even when
/// the step listing it is itself superseded
#[test]
fn superseded_steps_name_the_kept_step_superseding_them() {
    // list each dropped step of a whole-path plan with the step it names
    let dropped = |catalog: &Catalog| -> Vec<(String, &'static str)> {
        let state = catalog.reached(&rev(BASE), "0.0.0");
        catalog
            .superseded(&state, &rev(NEXT))
            .expect("superseded")
            .into_iter()
            .map(|(step, by)| (step.step, by))
            .collect()
    };
    // in a chain each rebuild supersedes the one before it, so only the last one runs and both
    // earlier ones name it
    assert_eq!(
        shape(&compose(&SUPERSEDES_CHAIN, BASE, NEXT)),
        ["step rebuild-c"]
    );
    assert_eq!(
        dropped(&SUPERSEDES_CHAIN),
        [
            ("rebuild-a".to_owned(), "rebuild-c"),
            ("rebuild-b".to_owned(), "rebuild-c"),
        ]
    );
    // in a diamond every path of supersession ends at the last rebuild
    assert_eq!(
        shape(&compose(&SUPERSEDES_DIAMOND, BASE, NEXT)),
        ["step rebuild-d"]
    );
    assert_eq!(
        dropped(&SUPERSEDES_DIAMOND),
        [
            ("rebuild-a".to_owned(), "rebuild-d"),
            ("rebuild-b".to_owned(), "rebuild-d"),
            ("rebuild-c".to_owned(), "rebuild-d"),
        ]
    );
}

/// All additive steps on a path become one converge step placed first, and contract steps are
/// placed last, never ahead of the migrate step they depend on
#[test]
fn converge_runs_first_and_contract_steps_last() {
    // both edges' additive steps become one converge to the target, and the contract step
    // runs after the users pass and the later check
    assert_eq!(
        shape(&compose(&PHASES, BASE, NEXT)),
        [
            "converge 2026-10-v03 [add-theme-column,add-table]",
            "step config-1",
            "passes redis:users [users-v2]",
            "step check-2",
            "step drop-dark",
        ]
    );
    // the production catalog's edges up to the first Helm revision have no phases or transforms,
    // so a plan across them is their steps in order
    let state = Catalog::PRODUCTION.reached(&rev(CATALOG[0].id), "0.0.0");
    let target = rev("2026-10-v02");
    let plan = Catalog::PRODUCTION
        .compose(&state, &target)
        .expect("compose");
    let steps: Vec<PlanItem> = Catalog::PRODUCTION
        .plan(&state, &target)
        .expect("plan")
        .into_iter()
        .map(PlanItem::Step)
        .collect();
    assert_eq!(plan.items, steps);
    // the edge after it applies the schema, then converts the users in a pass
    assert_eq!(
        shape(&compose(&Catalog::PRODUCTION, "2026-10-v02", "2026-10-v03")),
        [
            "step schema-apply",
            "passes redis:users [users-settings-theme]",
        ]
    );
}

/// Only an infra step without a migration folds into the converge step when tagged `Expand`;
/// config steps and migrations tagged `Expand` run on their own where they sit
#[test]
fn only_additive_schema_folds_into_the_converge_step() {
    // the additive step converges, and the other three run as steps in path order
    assert_eq!(
        shape(&compose(&EXPAND_KINDS, BASE, RELEASE)),
        [
            "converge 2026-10-v02 [add-table]",
            "step expand-config",
            "step expand-data",
            "step expand-backfill",
        ]
    );
}

/// A pass whose check reads an earlier pass's output keeps a later transform on that family out
/// of the earlier pass, and a link reading its own family or naming the previous link still fuses
#[test]
fn check_inputs_order_passes_without_splitting_chains() {
    // the files check reads the users after users-v2, so users-v3 can't join that pass
    assert_eq!(
        shape(&compose(&READS_AFTER, BASE, NEXT)),
        [
            "passes redis:users [users-v2]",
            "passes s3:files/ [files-v2] | redis:users [users-v3]",
        ]
    );
    // a link reading its own family, whose check names the link before it, joins that pass
    assert_eq!(
        shape(&compose(&FUSED_SELF_READS, BASE, NEXT)),
        ["passes redis:users [users-v2,users-v3]"]
    );
}

/// A step's work is re-checked right before it runs, and a step that became satisfied after
/// planning is skipped
#[test]
fn work_is_checked_right_before_a_step_runs() {
    // plan while the reindex still has work
    let mut state = CHECKED.reached(&rev(BASE), "0.0.0");
    let plan = CHECKED.compose(&state, &rev(NEXT)).expect("compose");
    assert_eq!(shape(&plan), ["step reindex", "step repair"]);
    // an admin fixes the data after planning, so the check made before running finds nothing
    let fixed = Cell::new(true);
    let checks = Cell::new(0);
    let check = |step: &PlannedStep| {
        checks.set(checks.get() + 1);
        if step.step == "reindex" && fixed.get() {
            Work::Nothing("fixed manually".to_owned())
        } else {
            Work::Needed(format!("{} has work", step.step))
        }
    };
    let NextWork::Skip { step, reason } = plan.next_work(check) else {
        panic!("a satisfied step is skipped");
    };
    assert_eq!(
        (step.step.as_str(), reason.as_str()),
        ("reindex", "fixed manually")
    );
    // once the skip is recorded the next plan starts at the repair, which still has work
    state.record(AppliedStep {
        revision: step.revision.clone(),
        step: step.step.clone(),
        outcome: StepState::Skipped,
        message: Some(reason),
        ..AppliedStep::default()
    });
    let plan = CHECKED.compose(&state, &rev(NEXT)).expect("compose");
    assert!(matches!(
        plan.next_work(check),
        NextWork::Run(PlanItem::Step(step)) if step.step == "repair"
    ));
    assert_eq!(checks.get(), 2);
    // passes check their own work as they run, so they aren't checked first
    let fused = compose(&FUSED, BASE, NEXT);
    assert!(matches!(
        fused.next_work(check),
        NextWork::Run(PlanItem::Passes(_))
    ));
    assert_eq!(checks.get(), 2);
    // a plan with nothing left is finished
    let done = compose(&CHECKED, NEXT, NEXT);
    assert_eq!(done.next_work(check), NextWork::Finished);
}

/// `classify` gives the expected class on fixtures of every layout and every broken variant
#[test]
fn transforms_classify_every_layout_and_broken_variant() {
    // the expected class of each variant under each users transform
    let unrecognized = |category, field: &str| RecordClass::Unrecognized {
        reason: IssueReason::new(category, field),
    };
    let expected = [
        (
            UserVariant::Layout1,
            RecordClass::Migratable { layout: 1 },
            RecordClass::Migratable { layout: 1 },
        ),
        (
            UserVariant::Layout2,
            RecordClass::Current,
            RecordClass::Migratable { layout: 2 },
        ),
        (
            UserVariant::Layout3,
            RecordClass::Current,
            RecordClass::Current,
        ),
        (
            UserVariant::BadDark,
            unrecognized(IssueCategory::InvalidValue, "dark"),
            RecordClass::Migratable { layout: 1 },
        ),
        (
            UserVariant::BadAi,
            RecordClass::Migratable { layout: 1 },
            RecordClass::Migratable { layout: 1 },
        ),
        (
            UserVariant::UnknownTheme,
            unrecognized(IssueCategory::UnknownVariant, "theme"),
            unrecognized(IssueCategory::UnknownVariant, "theme"),
        ),
        (
            UserVariant::MalformedSettings,
            RecordClass::Current,
            unrecognized(IssueCategory::MalformedJson, "settings"),
        ),
        (
            UserVariant::SettingsThemeType,
            RecordClass::Current,
            unrecognized(IssueCategory::BadType, "settings.theme"),
        ),
        (
            UserVariant::Empty,
            unrecognized(IssueCategory::MissingField, "dark"),
            unrecognized(IssueCategory::MissingField, "theme"),
        ),
    ];
    for (variant, v2, v3) in expected {
        let user = plan_fixtures::user(0, variant, true, Some(true));
        assert_eq!((USERS_V2.classify)(&user), v2, "{variant:?} under users-v2");
        assert_eq!((USERS_V3.classify)(&user), v3, "{variant:?} under users-v3");
    }
}

/// `up` and `down` give the expected output on fixtures of every layout, and the expected reason
/// code on every broken variant
#[test]
fn transforms_convert_every_layout_and_refuse_broken_variants() {
    // up converts each layout to the next, carrying the optional ai flag
    let layout = |variant, ai| plan_fixtures::user(0, variant, false, ai);
    for ai in [None, Some(true), Some(false)] {
        let up_v2 = (USERS_V2.up)(layout(UserVariant::Layout1, ai)).expect("users-v2 up");
        assert_eq!(up_v2, layout(UserVariant::Layout2, ai));
        let up_v3 = (USERS_V3.up)(layout(UserVariant::Layout2, ai)).expect("users-v3 up");
        assert_eq!(up_v3, layout(UserVariant::Layout3, ai));
        // down converts each layout back to the previous one
        let down = USERS_V3.down.expect("users-v3 down");
        assert_eq!(
            down(layout(UserVariant::Layout3, ai)),
            Ok(layout(UserVariant::Layout2, ai))
        );
        let down = USERS_V2.down.expect("users-v2 down");
        assert_eq!(
            down(layout(UserVariant::Layout2, ai)),
            Ok(layout(UserVariant::Layout1, ai))
        );
    }
    // broken variants fail up and down with their reason codes
    let up_reason = |transform: &TransformDef, variant| {
        (transform.up)(plan_fixtures::user(0, variant, true, None))
            .map_err(|issue| issue.reason.to_string())
            .err()
    };
    assert_eq!(
        up_reason(&USERS_V2, UserVariant::BadDark).as_deref(),
        Some("invalid_value:dark")
    );
    // users-v3 converts only layout 2, so a layout 1 user lacks the theme it reads
    assert_eq!(
        (USERS_V3.up)(plan_fixtures::user(0, UserVariant::BadAi, true, None))
            .map(|_| ())
            .map_err(|issue| issue.reason.to_string()),
        Err("missing_field:theme".to_owned())
    );
    let bad_ai = (USERS_V2.up)(plan_fixtures::user(0, UserVariant::BadAi, true, None))
        .expect("users-v2 leaves ai alone");
    assert_eq!(
        (USERS_V3.up)(bad_ai).map_err(|issue| issue.reason.to_string()),
        Err("invalid_value:ai".to_owned())
    );
    assert_eq!(
        up_reason(&USERS_V3, UserVariant::MalformedSettings).as_deref(),
        Some("malformed_json:settings")
    );
    let down_reason = |transform: &TransformDef, variant| {
        (transform.down.expect("down"))(plan_fixtures::user(0, variant, true, None))
            .map_err(|issue| issue.reason.to_string())
            .err()
    };
    assert_eq!(
        down_reason(&USERS_V2, UserVariant::UnknownTheme).as_deref(),
        Some("unknown_variant:theme")
    );
    assert_eq!(
        down_reason(&USERS_V3, UserVariant::SettingsThemeType).as_deref(),
        Some("bad_type:settings.theme")
    );
    assert_eq!(
        down_reason(&USERS_V3, UserVariant::Layout2).as_deref(),
        Some("missing_field:settings")
    );
    // files move under the prefix once
    let file = Record::S3 {
        key: "file-0".to_owned(),
    };
    let moved = (plan_fixtures::FILES_V2.up)(file).expect("files up");
    assert_eq!(
        moved,
        Record::S3 {
            key: "v2/file-0".to_owned()
        }
    );
    assert_eq!(
        (plan_fixtures::FILES_V2.classify)(&moved),
        RecordClass::Current
    );
}

/// Applying a fused chain in memory equals applying each transform with a write in between, for
/// many generated records per layout
#[test]
fn fused_chain_equals_transforms_with_writes_between() {
    // the fused chain and the same transforms run one at a time
    let links: [(&'static str, &'static TransformDef); 2] =
        [("users-v2", &USERS_V2), ("users-v3", &USERS_V3)];
    let fused = chain(&links, false);
    for variant in UserVariant::ALL {
        for user in plan_fixtures::users(variant, 64) {
            // run each transform as its own pass, writing between them, until one fails
            let mut stepwise = user.clone();
            let mut failed = None;
            for (step, transform) in links {
                let converted = match (transform.classify)(&stepwise) {
                    RecordClass::Current => continue,
                    RecordClass::Migratable { .. } => (transform.up)(stepwise.clone()),
                    RecordClass::Unrecognized { reason } => Err(super::TransformIssue {
                        reason,
                        layouts_tried: vec![transform.from],
                    }),
                };
                match converted {
                    Ok(converted) => stepwise = converted,
                    Err(issue) => {
                        failed = Some((step.to_owned(), issue.reason));
                        break;
                    }
                }
            }
            // the fused chain ends in the same place, or fails at the same link for the same
            // reason without writing
            match (fused.apply(&user), failed) {
                (ChainOutcome::Current, None) => assert_eq!(stepwise, user, "{variant:?}"),
                (ChainOutcome::Converted(converted), None) => {
                    assert_eq!(converted, stepwise, "{variant:?}");
                }
                (ChainOutcome::Issue { step, issue }, Some(expected)) => {
                    assert_eq!((step, issue.reason), expected, "{variant:?}");
                }
                (outcome, failed) => panic!("{variant:?}: fused {outcome:?}, stepwise {failed:?}"),
            }
        }
    }
}

/// A record older than a chain's first link is converted by that link, and a record no link
/// recognizes is an issue of the first link naming every layout the chain looked for
#[test]
fn records_before_the_first_link_are_its_issues() {
    // a layout 1 user under a chain starting at layout 2 fails in the first link's conversion
    let later = chain(&[("users-v3", &USERS_V3)], false);
    let user = plan_fixtures::user(0, UserVariant::Layout1, true, None);
    let ChainOutcome::Issue { step, issue } = later.apply(&user) else {
        panic!("a layout 1 user is an issue of users-v3");
    };
    assert_eq!(
        (step.as_str(), issue.reason.to_string().as_str()),
        ("users-v3", "missing_field:theme")
    );
    // a user with no field of any layout is tried at every layout, newest first
    let fused = chain(&[("users-v2", &USERS_V2), ("users-v3", &USERS_V3)], false);
    let empty = plan_fixtures::user(0, UserVariant::Empty, true, None);
    assert_eq!(
        fused.apply(&empty),
        ChainOutcome::Issue {
            step: "users-v2".to_owned(),
            issue: super::TransformIssue {
                reason: IssueReason::new(IssueCategory::MissingField, "dark"),
                layouts_tried: vec![3, 2, 1],
            },
        }
    );
}

/// `up(up(x)) == up(x)` for current records, and `classify` of an upgraded record returns
/// `current`
#[test]
fn upgrades_are_idempotent_and_current() {
    // generated users of every layout for the users transforms, and the pre-release
    // transform's own fixtures, whose layout 2 keeps the flag it derives the theme from
    let layouts = [
        UserVariant::Layout1,
        UserVariant::Layout2,
        UserVariant::Layout3,
    ]
    .into_iter()
    .flat_map(|variant| plan_fixtures::users(variant, 32))
    .collect::<Vec<_>>();
    let theme = fixtures::theme_fixtures()
        .into_iter()
        .map(|user| (fixtures::THEME.up)(user).expect("up"))
        .chain(fixtures::theme_fixtures())
        .collect::<Vec<_>>();
    for (transform, users) in [
        (&USERS_V2, &layouts),
        (&USERS_V3, &layouts),
        (&fixtures::THEME, &theme),
    ] {
        for user in users.iter().cloned() {
            match (transform.classify)(&user) {
                // a current record is left as it is
                RecordClass::Current => {
                    assert_eq!((transform.up)(user.clone()), Ok(user));
                }
                // an upgraded record is current, and upgrading it again changes nothing
                RecordClass::Migratable { layout } if layout == transform.from => {
                    let once = (transform.up)(user).expect("up");
                    assert_eq!((transform.classify)(&once), RecordClass::Current);
                    assert_eq!((transform.up)(once.clone()), Ok(once));
                }
                // other layouts are for other transforms
                _ => (),
            }
        }
    }
}

/// A forward transform reversed since its last `Done` runs in its after-a-reverse mode, which
/// recomputes the new form from the old one instead of skipping objects that hold the new form
#[test]
fn reversed_transforms_run_after_a_reverse() {
    // the first pre-release's users transform ran, was reverted, and is planned again
    let catalog = fixtures::SERIES;
    let mut state = catalog.reached(&rev(RELEASE), "0.0.0");
    for (revision, step) in [(STACK[0], "stack-1-step"), (RELEASE, "stack-1-revert")] {
        state.record(AppliedStep {
            revision: revision.to_owned(),
            step: step.to_owned(),
            outcome: StepState::Done,
            ..AppliedStep::default()
        });
    }
    let plan = catalog.compose(&state, &rev(STACK[0])).expect("compose");
    let pass = passes(&plan)[0];
    assert_eq!(pass.chain_ids(), ["stack-1-step"]);
    assert!(pass.after_reverse);
    // a user changed by the base code after the revert holds a stale theme
    let user = Record::Redis {
        key: "users:stale".to_owned(),
        fields: [
            ("dark".to_owned(), "false".to_owned()),
            ("theme".to_owned(), "dark".to_owned()),
        ]
        .into(),
    };
    // the after-a-reverse run derives the theme from the changed flag
    let resolved = catalog.chain(pass).expect("the pass resolves");
    let ChainOutcome::Converted(converted) = resolved.apply(&user) else {
        panic!("the stale user is recomputed");
    };
    assert_eq!(field(&converted, "theme"), Some("light"));
    // a plain run would take the stale theme as current
    assert_eq!(
        chain(&[("stack-1-step", &fixtures::THEME)], false).apply(&user),
        ChainOutcome::Current
    );
    // without a reverse in the history the pass runs normally
    let fresh = catalog.reached(&rev(RELEASE), "0.0.0");
    let plan = catalog.compose(&fresh, &rev(STACK[0])).expect("compose");
    assert!(!passes(&plan)[0].after_reverse);
    // the users family names the pass in the status
    assert_eq!(passes(&plan)[0].family, USERS);
    // a run after the reverse finished the step again, so once a reset lowers the deployment
    // below it the step is planned in its normal mode
    for (revision, step) in [(STACK[0], "stack-1-step"), (RELEASE, RESET_STATE_STEP)] {
        state.record(AppliedStep {
            revision: revision.to_owned(),
            step: step.to_owned(),
            outcome: StepState::Done,
            ..AppliedStep::default()
        });
    }
    let plan = catalog.compose(&state, &rev(STACK[0])).expect("compose");
    assert_eq!(passes(&plan)[0].chain_ids(), ["stack-1-step"]);
    assert!(!passes(&plan)[0].after_reverse);
}

/// A fused pass runs in its after-a-reverse mode when any one of its links was reversed since
/// its last `Done`
#[test]
fn a_pass_runs_after_a_reverse_when_any_link_was_reversed() {
    // both users transforms ran, the second was reverted, and a reset then lowered both
    let mut state = REVERSIBLE.reached(&rev(RELEASE), "0.0.0");
    for (revision, step) in [
        (STACK[0], "users-v2"),
        (STACK[1], "users-v3"),
        (STACK[0], "users-v3-revert"),
        (RELEASE, RESET_STATE_STEP),
    ] {
        state.record(AppliedStep {
            revision: revision.to_owned(),
            step: step.to_owned(),
            outcome: StepState::Done,
            ..AppliedStep::default()
        });
    }
    // the two links fuse, and the reverse of the second puts the whole pass in that mode
    let plan = REVERSIBLE.compose(&state, &rev(STACK[1])).expect("compose");
    assert_eq!(shape(&plan), ["passes redis:users [users-v2,users-v3]"]);
    assert!(passes(&plan)[0].after_reverse);
}

/// A downward path runs the reverse of the forward transform as a pass over its family
#[test]
fn downward_paths_run_reverse_transforms_as_passes() {
    // revert the first pre-release after it ran
    let catalog = fixtures::SERIES;
    let state: UpgradeState = catalog.reached(&rev(STACK[0]), "0.0.0");
    let plan = catalog.compose(&state, &rev(RELEASE)).expect("compose");
    assert_eq!(plan.direction, PlanDirection::Downgrade);
    assert_eq!(shape(&plan), ["passes redis:users [stack-1-revert]"]);
    assert_eq!(plan.loss, None);
}
