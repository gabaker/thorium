//! The operator's executor for each upgrade step in the catalog

use thorium::Error;
use thorium::models::upgrades::{StepDef, UpgradeApproval};

use crate::app;
use crate::app::bootstrap::StepOutcome;
use crate::app::helpers::CheckError;
use crate::k8s::clusters::ClusterMeta;

/// What runs a step from the catalog
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Executor {
    /// A step that runs unattended
    Unattended(Unattended),
    /// A step that changes data, which needs an approval once its check finds work to do
    Guarded(Guarded),
}

/// The executors of steps that run unattended
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unattended {
    /// Make sure Thorium's Scylla role logs in
    ScyllaRole,
    /// Make sure Thorium's Elastic user authenticates with the privileges it needs
    ElasticIdentity,
    /// Run the schema-apply Job, converging Scylla to the target schema before any step that
    /// changes data
    SchemaApply,
    /// Record the new revision, which the runner does once every step is done; also the step that
    /// undoes that record, which has nothing of its own to run
    Finalize,
}

/// The executors of steps that change data
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Guarded {
    /// Run the step's thoradm migration as a migration Job, after a check Job and an approval
    ThoradmJob,
    /// Check and guard a test's fixture step in process, with the check its test scripted
    #[cfg(test)]
    Fixture,
}

/// The executor of every step id this operator can run
///
/// This is the only place step ids are tied to executors, so the tests checking it against
/// the catalog check exactly what [`execute`] dispatches on.
const EXECUTORS: &[(&str, Executor)] = &[
    ("scylla-role", Executor::Unattended(Unattended::ScyllaRole)),
    (
        "elastic-identity",
        Executor::Unattended(Unattended::ElasticIdentity),
    ),
    (
        "schema-apply",
        Executor::Unattended(Unattended::SchemaApply),
    ),
    ("finalize", Executor::Unattended(Unattended::Finalize)),
    (
        "finalize-tool-only",
        Executor::Unattended(Unattended::Finalize),
    ),
    (
        "finalize-tool-only-reverse",
        Executor::Unattended(Unattended::Finalize),
    ),
];

/// Find the executor registered for a step id
///
/// # Arguments
///
/// * `id` - The id of the step
fn registered(id: &str) -> Option<Executor> {
    // a test's fixture step has the executor only tests build
    #[cfg(test)]
    if id == fixture::STEP {
        return Some(Executor::Guarded(Guarded::Fixture));
    }
    // find the executor registered for this step id
    EXECUTORS
        .iter()
        .find(|(step, _)| *step == id)
        .map(|(_, executor)| *executor)
}

/// Find the executor of a step: the one registered for its id, or, for a data step that names a
/// thoradm migration and has none of its own, the migration Job
///
/// # Arguments
///
/// * `step` - The step
fn executor(step: &StepDef) -> Option<Executor> {
    // a step registered here runs its own executor, and a data step naming a migration runs it
    registered(step.id).or_else(|| {
        (step.thoradm_migration.is_some() && step.kind.needs_approval())
            .then_some(Executor::Guarded(Guarded::ThoradmJob))
    })
}

/// Whether a step is run by thoradm as a migration Job
///
/// # Arguments
///
/// * `step` - The step
#[must_use]
pub fn runs_in_thoradm(step: &StepDef) -> bool {
    // the migration Job is the executor of a data step with a migration and none of its own
    matches!(executor(step), Some(Executor::Guarded(Guarded::ThoradmJob)))
}

/// Whether a step is the one that applies the schema
///
/// # Arguments
///
/// * `step` - The step
#[must_use]
pub fn is_schema_apply(step: &StepDef) -> bool {
    // the step the schema-apply Job runs for
    step.id == super::schema_apply::STEP
}

/// How running a step went
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepResult {
    /// The step finished, with optional details
    Done(Option<String>),
    /// The step had nothing to do, and why
    Skipped(String),
    /// The step can't finish until an admin acts, and what they need to do
    Blocked(String),
    /// A backend the step needs isn't up yet, and what it is waiting on
    Waiting(String),
}

/// Turn a failed check into a step result, keeping backends that aren't up yet as waits
///
/// # Arguments
///
/// * `error` - The error from the check
fn interrupted(error: CheckError) -> Result<StepResult, Error> {
    // keep backend waits as waits and fail on anything else
    match error {
        CheckError::Waiting(details) => Ok(StepResult::Waiting(details)),
        CheckError::Failed(error) => Err(error),
    }
}

/// Make sure Thorium's Scylla role logs in, creating it first when a Scylla bootstrap is set
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
async fn scylla_role(meta: &ClusterMeta) -> Result<StepResult, Error> {
    // create the role if a bootstrap admin is available and the role can't log in
    if let Err(error) = app::bootstrap::scylla(meta).await {
        return interrupted(error);
    }
    // make sure Thorium's own role logs in
    match app::checks::scylla_login(meta).await {
        Ok(()) => Ok(StepResult::Done(None)),
        Err(error) => interrupted(error),
    }
}

/// Make sure Thorium's Elastic user authenticates with the privileges Thorium needs
///
/// An external Elastic with `bootstrap.elastic` set gets the role and user created first.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
async fn elastic_identity(meta: &ClusterMeta) -> Result<StepResult, Error> {
    // create the role and user if an external Elastic bootstrap is set
    match app::bootstrap::elastic_identity(meta).await {
        Ok(StepOutcome::Done) => (),
        Ok(StepOutcome::MissingSecret(details)) => return Ok(StepResult::Blocked(details)),
        Err(error) => return interrupted(error),
    }
    // check Thorium's own user, leaving mapping problems to the reindex step
    match app::bootstrap::elastic_access(meta).await {
        Ok(_) => Ok(StepResult::Done(None)),
        Err(error) => interrupted(error),
    }
}

/// The words of the message of a step that waits only for an approval, which the history keeps
pub(super) const APPROVAL_WAIT: &str = "Waiting for an approval";

/// Describe a guarded step waiting for an approval
///
/// # Arguments
///
/// * `step` - The step waiting for an approval
/// * `work` - What the step would change
/// * `empty_backup` - Whether an approval was set for the step without a backup
pub(super) fn approval_needed(step: &StepDef, work: &str, empty_backup: bool) -> String {
    // point out an approval that can't be used since it names no backup
    let empty = if empty_backup {
        format!(" The approval set for step {} names no backup.", step.id)
    } else {
        String::new()
    };
    format!(
        "{work} {APPROVAL_WAIT}: step {id} changes data and needs an approval: add \
         {{step: {id}, backup: <where your backup is>}} to spec.upgrade.approvals (Helm \
         value operator.cluster.upgrade.approvals).{empty} The operator does not take backups; \
         the backup is your reference to one you took yourself.",
        id = step.id
    )
}

/// Check a step's approvals, returning whether one names a backup and whether one names none
///
/// # Arguments
///
/// * `step` - The step to check the approvals of
/// * `approvals` - The approvals set on the cluster
fn approval_state(step: &StepDef, approvals: &[UpgradeApproval]) -> (bool, bool) {
    // look at every approval for this step, noting any that names no backup
    let mut matching = approvals.iter().filter(|approval| approval.step == step.id);
    let mut empty_backup = false;
    let approved = matching.any(|approval| {
        let named = !approval.backup.trim().is_empty();
        empty_backup |= !named;
        named
    });
    (approved, empty_backup)
}

/// Get the first sentence of the first line of a text
///
/// # Arguments
///
/// * `text` - The text to take the sentence from
fn first_sentence(text: &str) -> &str {
    // keep the first line, then cut it after its first sentence
    let line = text.lines().next().unwrap_or_default();
    match line.find(". ") {
        Some(end) => &line[..=end],
        None => line,
    }
}

/// Say in one log line what an admin must do for a blocked step
///
/// The step's status keeps the full message; the line gives its start and points there.
///
/// # Arguments
///
/// * `step` - The blocked step
/// * `approvals` - The approvals set on the cluster
/// * `target` - The revision the upgrade is moving to
/// * `details` - The full message the step blocked with
pub fn block_log_line(
    step: &StepDef,
    approvals: &[UpgradeApproval],
    target: &str,
    details: &str,
) -> String {
    // a data step blocks on an approval until one names a backup, then on any manual procedure
    let approved = approval_state(step, approvals).0;
    let need = match (step.kind.needs_approval(), approved, step.manual_procedure) {
        (true, false, _) => ", which needs an approval naming a backup.".to_owned(),
        (true, true, Some(procedure)) => {
            format!(", which needs manual work: {}", first_sentence(procedure))
        }
        _ => format!(": {}", first_sentence(details)),
    };
    // end the summary as a sentence before pointing at the full message
    let period = if need.ends_with('.') { "" } else { "." };
    format!(
        "The upgrade to {target} is blocked at step {}{need}{period} Its full message is in the \
         ThoriumCluster's status.upgrade.steps.",
        step.id
    )
}

/// Run a step that runs unattended
///
/// The schema-apply step is refused here, since it only runs through the upgrade.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `unattended` - The step's executor
async fn run_unattended(meta: &ClusterMeta, unattended: Unattended) -> Result<StepResult, Error> {
    // dispatch to the step's check
    match unattended {
        Unattended::ScyllaRole => scylla_role(meta).await,
        Unattended::ElasticIdentity => elastic_identity(meta).await,
        // the schema-apply Job needs the upgrade in flight, so the runner runs it
        Unattended::SchemaApply => Err(Error::new(
            "The schema-apply step runs through the upgrade, not on its own",
        )),
        // the runner records the revision once every step is done
        Unattended::Finalize => Ok(StepResult::Done(None)),
    }
}

/// Run one upgrade step
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `step` - The step to run
pub async fn execute(meta: &ClusterMeta, step: &StepDef) -> Result<StepResult, Error> {
    // find this step's executor
    let Some(executor) = executor(step) else {
        return Err(Error::new(format!(
            "This operator has no executor for upgrade step {}",
            step.id
        )));
    };
    // run it, refusing an executor that doesn't match whether the step needs an approval
    match (executor, step.kind.needs_approval()) {
        (Executor::Unattended(unattended), false) => run_unattended(meta, unattended).await,
        // a migration Job is checked by a check Job and run by the upgrade, which needs it in flight
        (Executor::Guarded(Guarded::ThoradmJob), true) => Err(Error::new(format!(
            "Step {} runs through the upgrade as a migration Job, not on its own",
            step.id
        ))),
        // a test's fixture step is checked and guarded in process
        #[cfg(test)]
        (Executor::Guarded(Guarded::Fixture), true) => fixture::run(meta, step),
        _ => Err(Error::new(format!(
            "Upgrade step {} is a {:?} step, which this operator's executor for it does not \
             handle",
            step.id, step.kind
        ))),
    }
}

/// A test-only data step the operator checks and guards in process, standing in for a step whose own
/// executor checks for work, waits for an approval naming a backup, and then blocks on manual work:
/// the tests of how the upgrade runs, reports, and records a step's blocks, skips, waits, and
/// failures run it with the check they script
#[cfg(test)]
pub(crate) mod fixture {
    use std::cell::RefCell;
    use thorium::Error;
    use thorium::models::upgrades::{StepDef, StepKind, UpgradeApproval, UpgradeComponent, Work};

    use super::{StepResult, approval_needed, approval_state, interrupted};
    use crate::app::helpers::CheckError;
    use crate::k8s::clusters::ClusterMeta;

    /// The id of the fixture step
    pub(crate) const STEP: &str = "fixture-guarded";

    /// The fixture step's manual procedure, two sentences so a log line can give the first alone
    pub(crate) const PROCEDURE: &str =
        "Stop the fixture writer BEFORE changing anything. Then fix every fixture row by hand.";

    /// The fixture step: a data step that pauses the search-streamer, names no migration, and has a
    /// manual procedure
    pub(crate) const DEF: StepDef = StepDef {
        id: STEP,
        kind: StepKind::Data,
        description: "A fixture step the operator checks in process",
        pause: &[UpgradeComponent::SearchStreamer],
        thoradm_migration: None,
        manual_procedure: Some(PROCEDURE),
        ..StepDef::BASE
    };

    /// What the fixture step's check finds
    #[derive(Debug, Clone)]
    pub(crate) enum Check {
        /// Nothing needs changing, and why
        Nothing(String),
        /// Data needs changing, and what
        Needed(String),
        /// A backend the check needs isn't up yet, and which
        Waiting(String),
        /// The check failed, and why
        Failed(String),
    }

    thread_local! {
        /// The check the fixture step finds on this test's thread
        static CHECK: RefCell<Option<Check>> = const { RefCell::new(None) };
    }

    /// Make the fixture step's check find something on this thread until it is scripted again
    ///
    /// # Arguments
    ///
    /// * `check` - What the check finds
    pub(crate) fn script(check: Check) {
        // every later run on this thread finds it
        CHECK.with(|scripted| *scripted.borrow_mut() = Some(check));
    }

    /// Forget the scripted check, so the thread may run another test
    pub(crate) fn clear() {
        // the next test on this thread scripts its own
        CHECK.with(|scripted| *scripted.borrow_mut() = None);
    }

    /// What to do with a guarded step once its check ran
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) enum Guard {
        /// There is nothing to do, so skip the step without needing an approval
        Skip(String),
        /// There is work to do but no usable approval, so block on one with this message
        NeedsApproval(String),
        /// There is work to do and it is approved, with what the work is
        Approved(String),
    }

    /// Decide what to do with a guarded step from its check and the cluster's approvals
    ///
    /// A step with nothing to do is skipped without an approval. Otherwise it only runs with an
    /// approval for it naming a backup.
    ///
    /// # Arguments
    ///
    /// * `step` - The guarded step
    /// * `work` - What the step's check found
    /// * `approvals` - The approvals set on the cluster
    pub(super) fn guard(step: &StepDef, work: Work, approvals: &[UpgradeApproval]) -> Guard {
        // a step with nothing to change needs no approval
        let work = match work {
            Work::Nothing(reason) => return Guard::Skip(reason),
            Work::Needed(work) => work,
        };
        // find whether an approval for this step names a backup
        let (approved, empty_backup) = approval_state(step, approvals);
        // only an approval naming a backup lets the step change data
        if approved {
            Guard::Approved(work)
        } else {
            Guard::NeedsApproval(approval_needed(step, &work, empty_backup))
        }
    }

    /// Describe an approved step the operator can't run on its own
    ///
    /// # Arguments
    ///
    /// * `step` - The approved step
    /// * `work` - What the step must change
    pub(super) fn manual_work_needed(step: &StepDef, work: &str) -> String {
        // name the work, then point the admin at the step's manual procedure
        format!(
            "{work} Waiting for manual work: step {id} is approved, but the operator can't run \
             it on its own, so follow its procedure: {procedure} The operator checks again and \
             continues on its own once the work is done.",
            id = step.id,
            procedure = step.manual_procedure.unwrap_or_default()
        )
    }

    /// Run the fixture step: check it with the scripted check, skip it with nothing to do, wait
    /// for an approval naming a backup, and once approved block on its manual procedure
    ///
    /// # Arguments
    ///
    /// * `meta` - Thorium cluster client and metadata, whose cluster holds the approvals
    /// * `step` - The fixture step
    pub(super) fn run(meta: &ClusterMeta, step: &StepDef) -> Result<StepResult, Error> {
        // the scripted check stands in for a check of a backend
        let check = CHECK
            .with(|scripted| scripted.borrow().clone())
            .unwrap_or_else(|| panic!("the test scripted no check for step {STEP}"));
        let checked = match check {
            Check::Nothing(reason) => Ok(Work::Nothing(reason)),
            Check::Needed(work) => Ok(Work::Needed(work)),
            Check::Waiting(details) => Err(CheckError::Waiting(details)),
            Check::Failed(message) => Err(CheckError::Failed(Error::new(message))),
        };
        let work = match checked {
            Ok(work) => work,
            Err(error) => return interrupted(error),
        };
        // skip, wait for an approval, or block on the approved work, which is done by hand
        match guard(step, work, &meta.cluster.spec.upgrade.approvals) {
            Guard::Skip(reason) => Ok(StepResult::Skipped(reason)),
            Guard::NeedsApproval(message) => Ok(StepResult::Blocked(message)),
            Guard::Approved(work) => Ok(StepResult::Blocked(manual_work_needed(step, &work))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{Guard, guard, manual_work_needed};
    use super::*;
    use crate::k8s::clusters::tests::{FakeKube, full_spec, meta_for, namespaced_cluster};
    use thorium::models::upgrades::{CATALOG, StepKind, Work};

    /// Every step in the catalog has an executor matching whether it needs an approval, and
    /// every executor belongs to a catalog step
    #[test]
    fn every_step_has_an_executor() {
        // check every step of every revision
        for entry in CATALOG {
            for step in entry.steps {
                // a data step thoradm performs runs as a migration Job without an entry of its own
                let executor = executor(step).unwrap_or_else(|| {
                    panic!("step {} of revision {} has no executor", step.id, entry.id)
                });
                assert_eq!(
                    matches!(executor, Executor::Guarded(_)),
                    step.kind.needs_approval(),
                    "step {} is a {:?} step but its executor is {executor:?}",
                    step.id,
                    step.kind
                );
            }
        }
        // every executor is for a step in the catalog and is listed once
        for (index, (id, _)) in EXECUTORS.iter().enumerate() {
            assert!(
                thorium::models::upgrades::step(id).is_some(),
                "{id} is not in the catalog"
            );
            assert!(
                EXECUTORS[index + 1..].iter().all(|(other, _)| other != id),
                "{id} is listed twice"
            );
        }
    }

    /// Every data step that names a thoradm migration runs it as a migration Job, the first Helm
    /// revision's reindex included, and no step with an executor of its own names one
    #[test]
    fn every_step_naming_a_migration_runs_it_as_a_job() {
        // check every step of every revision
        for entry in CATALOG {
            for step in entry.steps {
                // a data step with a migration runs it, and any other step runs its own executor
                let names = step.thoradm_migration.is_some() && step.kind.needs_approval();
                assert_eq!(
                    runs_in_thoradm(step),
                    names,
                    "step {} of revision {}",
                    step.id,
                    entry.id
                );
                if names {
                    assert_eq!(
                        registered(step.id),
                        None,
                        "{} has its own executor",
                        step.id
                    );
                }
            }
        }
        // the reindex of the first Helm revision runs thoradm's Elastic rebuild
        let (_, reindex) =
            thorium::models::upgrades::step("elastic-reindex-keyword-mappings").expect("a step");
        assert_eq!(reindex.thoradm_migration, Some("elastic-rebuild"));
        assert!(runs_in_thoradm(reindex));
        // and its procedure for doing it by hand runs the same migration
        assert!(
            reindex
                .manual_procedure
                .is_some_and(|text| text.contains("thoradm migrate run --id elastic-rebuild"))
        );
    }

    /// The steps that convert agentic data are planned in a fixed order, each with the pause list
    /// and executor the design gives it: a cluster at the revision that applies the schema first
    /// runs only them, and one at the first Helm revision runs the schema and users steps first
    #[test]
    fn the_conversion_steps_are_planned_in_order_with_their_executors() {
        use thorium::models::upgrades::{Catalog, RevisionId, UpgradeComponent, UpgradeState};
        let catalog = Catalog::PRODUCTION;
        // the steps planned from a revision to the newest one
        let plan = |from: &str| {
            let state = UpgradeState::new(from.parse::<RevisionId>().expect("revision"), "0.0.0");
            catalog
                .plan(&state, &catalog.latest())
                .expect("a plan")
                .into_iter()
                .map(|step| step.step)
                .collect::<Vec<_>>()
        };
        // from the revision that applies the schema first the drain waits, then the conversions
        // run, and the record is last
        let conversions = [
            "drain-reactions",
            "images-to-tools",
            "pipelines-to-tool-stages",
            "provenance-to-legacy-tools",
            "finalize-tool-only",
        ];
        assert_eq!(plan("2026-10-v03"), conversions);
        // from the first Helm revision the schema is applied first and the drain still waits
        // before the users step pauses the API
        assert_eq!(
            plan("2026-10-v02"),
            [
                "schema-apply",
                "drain-reactions",
                "users-settings-theme",
                "images-to-tools",
                "pipelines-to-tool-stages",
                "provenance-to-legacy-tools",
                "finalize-tool-only",
            ]
        );
        // the conversions pause the five components and the drain pauses none
        let pauses = [
            UpgradeComponent::Api,
            UpgradeComponent::Scaler,
            UpgradeComponent::BaremetalScaler,
            UpgradeComponent::EventHandler,
            UpgradeComponent::SearchStreamer,
        ];
        let step = |id: &str| thorium::models::upgrades::step(id).expect("a step").1;
        assert_eq!(step("drain-reactions").pause, []);
        for id in [
            "images-to-tools",
            "pipelines-to-tool-stages",
            "provenance-to-legacy-tools",
        ] {
            assert_eq!(step(id).pause, pauses, "{id}");
        }
        assert_eq!(step("finalize-tool-only").pause, []);
        // the three conversions run as migration Jobs, and so does the drain's check
        for id in [
            "drain-reactions",
            "images-to-tools",
            "pipelines-to-tool-stages",
            "provenance-to-legacy-tools",
        ] {
            assert!(runs_in_thoradm(step(id)), "{id}");
            assert!(registered(id).is_none(), "{id} needs no row of its own");
        }
        // the record step has its own row and runs unattended
        assert!(!runs_in_thoradm(step("finalize-tool-only")));
        assert_eq!(
            registered("finalize-tool-only"),
            Some(Executor::Unattended(Unattended::Finalize))
        );
        // the pipeline conversion reads what the image conversion wrote, and every data step
        // after the drain checks again once the drain ends
        assert_eq!(
            step("pipelines-to-tool-stages").reads_after,
            ["drain-reactions", "images-to-tools"]
        );
        for id in ["images-to-tools", "provenance-to-legacy-tools"] {
            assert_eq!(step(id).reads_after, ["drain-reactions"], "{id}");
        }
        // the users step belongs to the earlier revision, which has no drain to wait for
        assert!(step("users-settings-theme").reads_after.is_empty());
    }

    /// Every data step of the newest revision names a block of text an admin can follow by hand
    #[test]
    fn the_conversion_steps_say_how_to_do_them_by_hand() {
        // the three conversions are run by thoradm, and the drain names the command that clears reactions
        for id in [
            "images-to-tools",
            "pipelines-to-tool-stages",
            "provenance-to-legacy-tools",
        ] {
            let step = thorium::models::upgrades::step(id).expect("a step").1;
            let text = step.manual_procedure.expect("a procedure");
            assert!(
                text.contains(&format!("thoradm migrate run --id {id}")),
                "{id}"
            );
        }
        let drain = thorium::models::upgrades::step("drain-reactions")
            .expect("a step")
            .1;
        assert!(
            drain
                .manual_procedure
                .is_some_and(|text| text.contains("thorctl reactions delete"))
        );
    }

    /// Build an approval
    ///
    /// # Arguments
    ///
    /// * `step` - The approved step
    /// * `backup` - Where the backup is
    fn approval(step: &str, backup: &str) -> UpgradeApproval {
        // build the approval from borrowed strings
        UpgradeApproval {
            step: step.to_owned(),
            backup: backup.to_owned(),
            ..UpgradeApproval::default()
        }
    }

    /// A guarded step with nothing to do is skipped without an approval
    #[test]
    fn guard_skips_without_work() {
        // nothing to change skips the step with or without an approval
        let step = &fixture::DEF;
        for approvals in [vec![], vec![approval(step.id, "snapshot-1")]] {
            assert_eq!(
                guard(step, Work::Nothing("all good".to_owned()), &approvals),
                Guard::Skip("all good".to_owned())
            );
        }
    }

    /// A guarded step with work to do needs an approval for it naming a backup
    #[test]
    fn guard_needs_approval_with_backup() {
        // check the fixture step against work that needs doing
        let step = &fixture::DEF;
        let work = || Work::Needed("index x is wrong.".to_owned());
        // without an approval the step waits for one and says how to add it
        let Guard::NeedsApproval(message) = guard(step, work(), &[]) else {
            panic!("an unapproved step should wait for an approval");
        };
        assert!(message.starts_with("index x is wrong."));
        assert!(message.contains("Waiting for an approval"));
        assert!(message.contains(
            "add {step: fixture-guarded, backup: <where your backup is>} to \
             spec.upgrade.approvals (Helm value operator.cluster.upgrade.approvals)"
        ));
        assert!(message.contains("does not take backups"));
        assert!(!message.contains("names no backup"));
        // an approval for another step doesn't count
        assert!(matches!(
            guard(step, work(), &[approval("finalize", "snapshot-1")]),
            Guard::NeedsApproval(_)
        ));
        // an approval without a backup doesn't count and is pointed out
        for backup in ["", "  "] {
            let Guard::NeedsApproval(message) = guard(step, work(), &[approval(step.id, backup)])
            else {
                panic!("an approval without a backup should not count");
            };
            assert!(message.contains("names no backup"), "{backup:?}");
        }
        // an approval naming a backup lets the work run, even next to an empty one
        assert_eq!(
            guard(
                step,
                work(),
                &[approval(step.id, ""), approval(step.id, "snapshot-1")]
            ),
            Guard::Approved("index x is wrong.".to_owned())
        );
    }

    /// An approved step without an automated executor asks for the manual procedure
    #[test]
    fn manual_work_message() {
        // the message names the work, says what it waits on, and gives the procedure
        let step = &fixture::DEF;
        let message = manual_work_needed(step, "Fixture rows a and b are wrong.");
        assert!(message.starts_with("Fixture rows a and b are wrong."));
        assert!(message.contains("Waiting for manual work"));
        assert!(message.contains(fixture::PROCEDURE));
        assert!(!message.contains("Waiting for an approval"));
    }

    /// A step whose executor doesn't match whether it needs an approval is refused untouched
    #[tokio::test]
    async fn mismatched_executor_is_refused() {
        // build a cluster whose kube API would refuse everything
        let fake = FakeKube::default();
        let meta = meta_for(namespaced_cluster(full_spec()), &fake.client());
        // a data step run by an unattended executor and the reverse are both refused
        for (id, kind) in [
            ("finalize", StepKind::Data),
            (fixture::STEP, StepKind::Config),
        ] {
            let step = StepDef {
                id,
                kind,
                description: "",
                pause: &[],
                thoradm_migration: None,
                manual_procedure: None,
                ..StepDef::BASE
            };
            let error = execute(&meta, &step).await.expect_err("mismatch");
            assert!(error.to_string().contains(id), "{error}");
        }
        assert!(fake.requests().is_empty());
    }

    /// A backend that isn't up yet is a wait while any other failure fails the step
    #[test]
    fn interrupted_keeps_waits() {
        // a wait on a backend is retried as a wait
        assert_eq!(
            interrupted(CheckError::Waiting("redis".to_owned())).expect("wait"),
            StepResult::Waiting("redis".to_owned())
        );
        // a failure fails the step with its error
        let error =
            interrupted(CheckError::Failed(Error::new("bad password"))).expect_err("failure");
        assert!(error.to_string().contains("bad password"));
    }

    /// The finalize step needs no backend and a step without an executor is an error
    #[tokio::test]
    async fn finalize_and_unknown_steps() {
        // build a cluster whose kube API would refuse everything
        let fake = FakeKube::default();
        let meta = meta_for(namespaced_cluster(full_spec()), &fake.client());
        let (_, finalize) = thorium::models::upgrades::step("finalize").expect("finalize step");
        // finalize finishes on its own without touching anything
        assert_eq!(
            execute(&meta, finalize).await.expect("finalize"),
            StepResult::Done(None)
        );
        assert!(fake.requests().is_empty());
        // a step this operator doesn't know is an error naming it
        let unknown = StepDef {
            id: "from-a-newer-catalog",
            kind: StepKind::Config,
            description: "",
            pause: &[],
            thoradm_migration: None,
            manual_procedure: None,
            ..StepDef::BASE
        };
        let error = execute(&meta, &unknown).await.expect_err("unknown");
        assert!(error.to_string().contains("from-a-newer-catalog"));
    }
}
