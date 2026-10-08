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
    /// Record the new revision, which the runner does once every step is done
    Finalize,
}

/// The executors of steps that change data
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Guarded {
    /// Reindex Elastic indexes whose `group` isn't a keyword
    ElasticReindex,
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
        "elastic-reindex-keyword-mappings",
        Executor::Guarded(Guarded::ElasticReindex),
    ),
    ("finalize", Executor::Unattended(Unattended::Finalize)),
];

/// Find the executor for a step id
///
/// # Arguments
///
/// * `id` - The id of the step
fn executor(id: &str) -> Option<Executor> {
    EXECUTORS
        .iter()
        .find(|(step, _)| *step == id)
        .map(|(_, executor)| *executor)
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

/// What a guarded step's check found
#[derive(Debug, Clone, PartialEq, Eq)]
enum Work {
    /// Nothing needs changing, and why
    Nothing(String),
    /// Data needs changing, and what
    Needed(String),
}

/// What to do with a guarded step once its check ran
#[derive(Debug, Clone, PartialEq, Eq)]
enum Guard {
    /// There is nothing to do, so skip the step without needing an approval
    Skip(String),
    /// There is work to do but no usable approval, so block on one with this message
    NeedsApproval(String),
    /// There is work to do and it is approved, with what the work is
    Approved(String),
}

/// Describe a guarded step waiting for an approval
///
/// # Arguments
///
/// * `step` - The step waiting for an approval
/// * `work` - What the step would change
/// * `empty_backup` - Whether an approval was set for the step without a backup
fn approval_needed(step: &StepDef, work: &str, empty_backup: bool) -> String {
    // point out an approval that can't be used since it names no backup
    let empty = if empty_backup {
        format!(" The approval set for step {} names no backup.", step.id)
    } else {
        String::new()
    };
    format!(
        "{work} Waiting for an approval: step {id} changes data and needs an approval: add \
         {{step: {id}, backup: <where your backup is>}} to spec.upgrade.approvals (Helm \
         value operator.cluster.upgrade.approvals).{empty} The operator does not take backups; \
         the backup is your reference to one you took yourself.",
        id = step.id
    )
}

/// Describe an approved step the operator can't run on its own
///
/// # Arguments
///
/// * `step` - The approved step
/// * `work` - What the step must change
fn manual_work_needed(step: &StepDef, work: &str) -> String {
    format!(
        "{work} Waiting for manual work: step {id} is approved, but the operator can't run it \
         on its own, so follow its procedure: {procedure} The operator checks again and \
         continues on its own once the work is done.",
        id = step.id,
        procedure = step.manual_procedure.unwrap_or_default()
    )
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
fn guard(step: &StepDef, work: Work, approvals: &[UpgradeApproval]) -> Guard {
    // a step with nothing to change needs no approval
    let work = match work {
        Work::Nothing(reason) => return Guard::Skip(reason),
        Work::Needed(work) => work,
    };
    // find the approvals for this step and whether any of them names a backup
    let mut matching = approvals.iter().filter(|approval| approval.step == step.id);
    let mut empty_backup = false;
    let approved = matching.any(|approval| {
        let named = !approval.backup.trim().is_empty();
        empty_backup |= !named;
        named
    });
    // only an approval naming a backup lets the step change data
    if approved {
        Guard::Approved(work)
    } else {
        Guard::NeedsApproval(approval_needed(step, &work, empty_backup))
    }
}

/// Check for Elastic indexes whose `group` isn't a keyword
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
async fn reindex_work(meta: &ClusterMeta) -> Result<Work, CheckError> {
    // find the indexes that need reindexing
    let bad = app::bootstrap::indexes_needing_reindex(meta).await?;
    // nothing to do when every index is correct or not created yet
    if bad.is_empty() {
        return Ok(Work::Nothing(
            "every existing Elastic index maps group as a keyword".to_owned(),
        ));
    }
    Ok(Work::Needed(format!(
        "Elastic indexes {} do not map group as a keyword and must be reindexed.",
        bad.join(", ")
    )))
}

/// Run a step that runs unattended
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `unattended` - The step's executor
async fn run_unattended(meta: &ClusterMeta, unattended: Unattended) -> Result<StepResult, Error> {
    match unattended {
        Unattended::ScyllaRole => scylla_role(meta).await,
        Unattended::ElasticIdentity => elastic_identity(meta).await,
        // the runner records the revision once every step is done
        Unattended::Finalize => Ok(StepResult::Done(None)),
    }
}

/// Run a step that changes data, checking for work before asking for an approval
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `step` - The step to run
/// * `guarded` - The step's executor
/// * `approvals` - The approvals set on the cluster
async fn run_guarded(
    meta: &ClusterMeta,
    step: &StepDef,
    guarded: Guarded,
    approvals: &[UpgradeApproval],
) -> Result<StepResult, Error> {
    // check whether the step has anything to change
    let checked = match guarded {
        Guarded::ElasticReindex => reindex_work(meta).await,
    };
    let work = match checked {
        Ok(work) => work,
        Err(error) => return interrupted(error),
    };
    // skip, wait for an approval, or do the approved work
    match guard(step, work, approvals) {
        Guard::Skip(reason) => Ok(StepResult::Skipped(reason)),
        Guard::NeedsApproval(message) => Ok(StepResult::Blocked(message)),
        Guard::Approved(work) => match guarded {
            // reindexing means stopping the search-streamer and deleting indexes, which the
            // operator doesn't do on its own, so an approved reindex is done by hand
            Guarded::ElasticReindex => Ok(StepResult::Blocked(manual_work_needed(step, &work))),
        },
    }
}

/// Run one upgrade step
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `step` - The step to run
/// * `approvals` - The approvals set on the cluster
pub async fn execute(
    meta: &ClusterMeta,
    step: &StepDef,
    approvals: &[UpgradeApproval],
) -> Result<StepResult, Error> {
    // find this step's executor
    let Some(executor) = executor(step.id) else {
        return Err(Error::new(format!(
            "This operator has no executor for upgrade step {}",
            step.id
        )));
    };
    // run it, refusing an executor that doesn't match whether the step needs an approval
    match (executor, step.kind.needs_approval()) {
        (Executor::Unattended(unattended), false) => run_unattended(meta, unattended).await,
        (Executor::Guarded(guarded), true) => run_guarded(meta, step, guarded, approvals).await,
        _ => Err(Error::new(format!(
            "Upgrade step {} is a {:?} step, which this operator's executor for it does not \
             handle",
            step.id, step.kind
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::clusters::tests::{FakeKube, full_spec, meta_for, namespaced_cluster};
    use thorium::models::upgrades::{CATALOG, StepKind};

    /// Every step in the catalog has an executor matching whether it needs an approval, and
    /// every executor belongs to a catalog step
    #[test]
    fn every_step_has_an_executor() {
        // check every step of every revision
        for entry in CATALOG {
            for step in entry.steps {
                let executor = executor(step.id).unwrap_or_else(|| {
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

    /// Get the reindex step from the catalog
    fn reindex_step() -> &'static StepDef {
        thorium::models::upgrades::step("elastic-reindex-keyword-mappings")
            .expect("reindex step")
            .1
    }

    /// Build an approval
    ///
    /// # Arguments
    ///
    /// * `step` - The approved step
    /// * `backup` - Where the backup is
    fn approval(step: &str, backup: &str) -> UpgradeApproval {
        UpgradeApproval {
            step: step.to_owned(),
            backup: backup.to_owned(),
        }
    }

    /// A guarded step with nothing to do is skipped without an approval
    #[test]
    fn guard_skips_without_work() {
        // nothing to change skips the step with or without an approval
        let step = reindex_step();
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
        let step = reindex_step();
        let work = || Work::Needed("index x is wrong.".to_owned());
        // without an approval the step waits for one and says how to add it
        let Guard::NeedsApproval(message) = guard(step, work(), &[]) else {
            panic!("an unapproved step should wait for an approval");
        };
        assert!(message.starts_with("index x is wrong."));
        assert!(message.contains("Waiting for an approval"));
        assert!(message.contains(
            "add {step: elastic-reindex-keyword-mappings, backup: <where your backup is>} to \
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
        let step = reindex_step();
        let message = manual_work_needed(step, "Elastic indexes thorium_repo_results are wrong.");
        assert!(message.starts_with("Elastic indexes thorium_repo_results are wrong."));
        assert!(message.contains("Waiting for manual work"));
        assert!(message.contains("search-streamer"));
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
            ("elastic-reindex-keyword-mappings", StepKind::Config),
        ] {
            let step = StepDef {
                id,
                kind,
                description: "",
                quiesce: &[],
                thoradm_migration: None,
                manual_procedure: None,
            };
            let error = execute(&meta, &step, &[]).await.expect_err("mismatch");
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
            execute(&meta, finalize, &[]).await.expect("finalize"),
            StepResult::Done(None)
        );
        assert!(fake.requests().is_empty());
        // a step this operator doesn't know is an error naming it
        let unknown = StepDef {
            id: "from-a-newer-catalog",
            kind: StepKind::Config,
            description: "",
            quiesce: &[],
            thoradm_migration: None,
            manual_procedure: None,
        };
        let error = execute(&meta, &unknown, &[]).await.expect_err("unknown");
        assert!(error.to_string().contains("from-a-newer-catalog"));
    }
}
