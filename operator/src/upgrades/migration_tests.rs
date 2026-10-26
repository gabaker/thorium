//! Tests of the migration Jobs the operator runs: the check Job per gated step, the approval gate,
//! pausing and restoring components, retries, cancellation, and the issues a run leaves, all
//! against a stateful fake of the Kubernetes API

use chrono::{DateTime, Duration, Utc};
use kube::runtime::controller::Action;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use thorium::Error;
use thorium::models::schema::{CheckReport, CheckWait, PassCounts, ReasonSummary};
use thorium::models::upgrades::{
    AppliedStep, BackendSchemaStatus, Catalog, CatalogEntry, DriftEntry, DriftKind, DriftSummary,
    PlannedStep, RECORD_MARKER, SchemaBackend, SchemaTarget, StepDef, StepKind, StepState,
    UpgradeComponent, UpgradeConditionType, UpgradeState,
};

use super::migration::{self, Advance, NOW};
use super::pause::{self, RESTORE_STEP};
use super::record::Moved;
use super::sim::{Finish, Sim};
use super::tests::{chart_cluster, rev, tracked};
use super::{CATALOG_OVERRIDE, Gate, OPERATOR_VERSION, Pending, RecordOps, gate, run_with};
use crate::k8s::clusters::tests::meta_for;
use crate::k8s::crds::ThoriumCluster;

/// What a fixture step tells an admin to do by hand
const PROCEDURE: &str = "Run `thoradm migrate run --id <id>` by hand.";

/// Build a step thoradm runs as a migration
///
/// # Arguments
///
/// * `id` - The step's id, which is also its migration's
/// * `pause` - The components it pauses
/// * `reads_after` - The steps whose output its check reads
const fn gated(
    id: &'static str,
    pause: &'static [UpgradeComponent],
    reads_after: &'static [&'static str],
) -> StepDef {
    // a data step with a migration of its own and a procedure
    StepDef {
        id,
        kind: StepKind::Data,
        description: "A fixture migration",
        pause,
        thoradm_migration: Some(id),
        manual_procedure: Some(PROCEDURE),
        reads_after,
        ..StepDef::BASE
    }
}

/// Build a step that runs unattended
///
/// # Arguments
///
/// * `id` - The step's id
const fn plain(id: &'static str) -> StepDef {
    // an infra step with nothing to pause
    StepDef {
        id,
        kind: StepKind::Infra,
        description: "A fixture step",
        ..StepDef::BASE
    }
}

/// Build an edge of a fixture catalog
///
/// # Arguments
///
/// * `from` - The revision it starts from
/// * `id` - The revision it reaches
/// * `steps` - Its steps
const fn edge(
    from: Option<&'static str>,
    id: &'static str,
    steps: &'static [StepDef],
) -> CatalogEntry {
    // a release edge with no snapshot
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

/// The components the fixture steps pause
const PAUSES: &[UpgradeComponent] = &[UpgradeComponent::Api, UpgradeComponent::Scaler];

/// One gated step
const ONE: Catalog = Catalog {
    edges: &[
        edge(None, "2026-10-v01", &[]),
        edge(
            Some("2026-10-v01"),
            "2026-10-v02",
            &[gated("users-v2", PAUSES, &[])],
        ),
    ],
    floors: &[],
};

/// The Redis users a converting fixture step targets
const USERS: &[SchemaTarget] = &[SchemaTarget {
    backend: SchemaBackend::Redis,
    object: "users",
    field: None,
}];

/// One gated step that converts the users layout
const CONVERTING: Catalog = Catalog {
    edges: &[
        edge(None, "2026-10-v01", &[]),
        edge(
            Some("2026-10-v01"),
            "2026-10-v02",
            &[StepDef {
                targets: USERS,
                ..gated("users-v2", PAUSES, &[])
            }],
        ),
    ],
    floors: &[],
};

/// Three gated steps: the second reads the first's output, and the third reads nothing
const THREE: Catalog = Catalog {
    edges: &[
        edge(None, "2026-10-v01", &[]),
        edge(
            Some("2026-10-v01"),
            "2026-10-v02",
            &[
                gated("users-v2", PAUSES, &[]),
                gated("files-v2", &[], &["users-v2"]),
                gated("tags-v2", &[], &[]),
            ],
        ),
    ],
    floors: &[],
};

/// Two gated steps that read nothing
const TWO: Catalog = Catalog {
    edges: &[
        edge(None, "2026-10-v01", &[]),
        edge(
            Some("2026-10-v01"),
            "2026-10-v02",
            &[gated("users-v2", &[], &[]), gated("tags-v2", &[], &[])],
        ),
    ],
    floors: &[],
};

/// The schema-apply step, then a gated step
const ORDER: Catalog = Catalog {
    edges: &[
        edge(None, "2026-10-v01", &[]),
        edge(
            Some("2026-10-v01"),
            "2026-10-v02",
            &[plain("schema-apply"), gated("users-v2", PAUSES, &[])],
        ),
    ],
    floors: &[],
};

/// Puts the catalog and the clock back when a test ends
struct Guard;

impl Drop for Guard {
    /// Remove the catalog and the time a test injected
    fn drop(&mut self) {
        // the thread may run another test
        CATALOG_OVERRIDE.with(|catalog| *catalog.borrow_mut() = None);
        NOW.with(|now| *now.borrow_mut() = None);
    }
}

/// The record moved locally, so these tests need no status Job to plan or finish an upgrade
struct Local;

impl RecordOps for Local {
    /// Plan from the state the gate read
    ///
    /// # Arguments
    ///
    /// * `_meta` - Thorium cluster client and metadata
    /// * `_pending` - The upgrade about to run
    fn refresh(
        &self,
        _meta: &crate::k8s::clusters::ClusterMeta,
        _pending: &mut Pending,
    ) -> impl std::future::Future<Output = Result<Option<Action>, Error>> {
        // the state is the record
        std::future::ready(Ok(None))
    }

    /// Record the finalize step of a plan that has one and advance the state's revision to the
    /// target
    ///
    /// # Arguments
    ///
    /// * `meta` - Thorium cluster client and metadata
    /// * `pending` - The upgrade whose state is advanced and saved
    /// * `revision` - The revision to record
    async fn move_revision(
        &self,
        meta: &crate::k8s::clusters::ClusterMeta,
        pending: &mut Pending,
        revision: &thorium::models::upgrades::RevisionId,
    ) -> Result<Moved, Error> {
        // a state already at the revision has nothing to move
        if pending.stored.state.revision == *revision {
            return Ok(Moved::Done);
        }
        // thoradm records the finalize step of a plan that has one, as the record Job does
        let finalized = pending
            .stored
            .state
            .applied
            .iter()
            .any(|applied| applied.step == "finalize" && applied.revision == revision.as_str());
        if !finalized && pending.steps.iter().any(|step| step.step == "finalize") {
            pending.stored.state.record(AppliedStep {
                revision: revision.to_string(),
                step: "finalize".to_owned(),
                outcome: StepState::Done,
                by: Some("thoradm test".to_owned()),
                ..AppliedStep::default()
            });
        }
        super::catalog().advance(&mut pending.stored.state, &pending.target, OPERATOR_VERSION);
        super::state::save(&meta.client, &meta.namespace, &mut pending.stored).await?;
        Ok(Moved::Done)
    }
}

/// A cluster, its fake kube API, and the upgrade the tests run
struct Scene {
    /// The fake kube API
    sim: Sim,
    /// The cluster, with the approvals and annotations of the moment
    cluster: ThoriumCluster,
    /// Puts the catalog back at the end
    _guard: Guard,
}

impl Scene {
    /// Build a scene upgrading from the first revision to the second of a fixture catalog
    ///
    /// # Arguments
    ///
    /// * `catalog` - The fixture catalog
    /// * `approvals` - The approvals the cluster starts with
    /// * `deployments` - The Deployments that exist, with their replicas
    fn new(catalog: Catalog, approvals: &Value, deployments: &[(&str, i32)]) -> Scene {
        // plan with the fixture catalog on this thread
        CATALOG_OVERRIDE.with(|found| *found.borrow_mut() = Some(catalog));
        let cluster = chart_cluster(json!({
            "target_revision": "2026-10-v02",
            "approvals": approvals,
        }));
        let sim = Sim::new(&cluster);
        // the deployment at the first revision, mirroring a record
        let mut state = UpgradeState::new(rev("2026-10-v01"), "1.8.1");
        state
            .schema
            .insert(RECORD_MARKER.to_owned(), "scylla".to_owned());
        sim.set_state(&state);
        for (name, replicas) in deployments {
            sim.add_deployment(name, *replicas);
        }
        // status Jobs read the record and finish at once
        sim.auto_complete("schema-status", json!({"outcome": "done"}));
        Scene {
            sim,
            cluster,
            _guard: Guard,
        }
    }

    /// Set the approvals on the cluster
    ///
    /// # Arguments
    ///
    /// * `approvals` - The approvals
    fn approve(&mut self, approvals: &Value) {
        // keep the annotations the cluster has
        let annotations = self.cluster.metadata.annotations.clone();
        self.cluster = chart_cluster(json!({
            "target_revision": "2026-10-v02",
            "approvals": approvals,
        }));
        self.cluster.metadata.annotations = annotations;
    }

    /// Set an annotation on the cluster
    ///
    /// # Arguments
    ///
    /// * `key` - The annotation's key
    /// * `value` - Its value
    fn annotate(&mut self, key: &str, value: &str) {
        // add it to the cluster's metadata
        self.cluster
            .metadata
            .annotations
            .get_or_insert_with(BTreeMap::new)
            .insert(key.to_owned(), value.to_owned());
    }

    /// Set the cluster's target revision, or clear it, keeping its approvals and annotations
    ///
    /// # Arguments
    ///
    /// * `target` - The target revision, or none to clear it
    fn retarget(&mut self, target: Option<&str>) {
        // only the target changes
        self.cluster.spec.upgrade.target_revision = target.map(ToOwned::to_owned);
    }

    /// Run one reconcile of the upgrade
    ///
    /// Returns when the reconcile asks to be run again: the upgrade's action, or the hold's.
    async fn reconcile(&self) -> Option<Action> {
        // gate the cluster with the status the last reconcile left
        let mut cluster = self.cluster.clone();
        cluster.status = Some(self.sim.status());
        let client = self.sim.fake().client();
        // a cluster that is held or reached its target has nothing left to run
        let tracker = tracked(&cluster);
        let pending = match gate(&client, &tracker).await {
            Ok(Gate::Upgrade(pending)) => pending,
            Ok(Gate::Hold(action)) => return Some(action),
            // a hold with a run open stops its Job first, through the gate's status tracker
            Ok(Gate::Stop { action, stored }) => {
                let mut meta = meta_for(cluster, &client);
                meta.status = std::sync::Arc::new(tracker);
                return Some(super::stop(&meta, action, stored).await.expect("stop"));
            }
            _ => return None,
        };
        let meta = meta_for(cluster, &client);
        Box::pin(run_with(&meta, pending, &Local))
            .await
            .expect("run")
    }

    /// Run several reconciles
    ///
    /// # Arguments
    ///
    /// * `times` - How many
    async fn rounds(&self, times: usize) {
        // each one sees what the last left
        for _ in 0..times {
            self.reconcile().await;
        }
    }

    /// Get a planned step from the status
    ///
    /// # Arguments
    ///
    /// * `id` - The step's id
    fn step(&self, id: &str) -> PlannedStep {
        // the step as the operator last reported it
        self.sim
            .status()
            .upgrade
            .expect("an upgrade status")
            .steps
            .into_iter()
            .find(|step| step.step == id)
            .unwrap_or_else(|| panic!("the status has no step {id}"))
    }

    /// Get the history entries of a step
    ///
    /// # Arguments
    ///
    /// * `id` - The step's id
    fn history(&self, id: &str) -> Vec<AppliedStep> {
        // the recorded entries, oldest first
        self.sim
            .state()
            .expect("a state")
            .applied
            .into_iter()
            .filter(|entry| entry.step == id)
            .collect()
    }

    /// End every check Job that is running with a report
    ///
    /// # Arguments
    ///
    /// * `report` - The report in the Jobs' result rows
    fn finish_checks(&self, report: &Value) {
        // the row first, then the Job's condition and summary
        for job in self.sim.active("migration-check") {
            self.sim.leave_result(&job, report.clone());
            self.sim.finish(
                &job,
                &Finish::Complete(json!({"outcome": "done", "result_id": format!("uid-{job}")})),
            );
        }
    }

    /// Run reconciles until a check report is cached for every gated step the plan holds, ending
    /// each check Job with the report
    ///
    /// # Arguments
    ///
    /// * `report` - The report every check leaves
    /// * `rounds` - The most reconciles to run
    async fn settle_checks(&self, report: &Value, rounds: usize) {
        // a reconcile creates or reads a Job, and a test ends the Jobs between them
        for _ in 0..rounds {
            self.reconcile().await;
            self.finish_checks(report);
        }
        self.reconcile().await;
    }

    /// Get the arguments of a created Job
    ///
    /// # Arguments
    ///
    /// * `job` - The Job as posted
    fn args(job: &Value) -> Vec<String> {
        // the container's arguments after the config path thoradm is started with
        job["spec"]["template"]["spec"]["containers"][0]["args"]
            .as_array()
            .expect("arguments")
            .iter()
            .filter_map(|arg| arg.as_str().map(ToOwned::to_owned))
            .skip(2)
            .collect()
    }

    /// Get the value after a flag in a Job's arguments
    ///
    /// # Arguments
    ///
    /// * `job` - The Job as posted
    /// * `flag` - The flag
    fn flag(job: &Value, flag: &str) -> Option<String> {
        // the argument after the flag
        let args = Scene::args(job);
        let at = args.iter().position(|arg| arg == flag)?;
        args.get(at + 1).cloned()
    }
}

/// Build a check report as a check Job's result row holds it
///
/// # Arguments
///
/// * `migration` - The migration checked
/// * `migratable` - How many objects need migrating
/// * `reasons` - The issue groups as `(code, count)`
/// * `wait` - How long the check asks to wait, if it does
fn check_report(
    migration: &str,
    migratable: u64,
    reasons: &[(&str, u64)],
    wait: Option<u64>,
) -> Value {
    // ten objects read, the rest current
    let report = CheckReport {
        migration: migration.to_owned(),
        counts: PassCounts {
            read: 10,
            current: 10 - migratable,
            migratable,
            ..PassCounts::default()
        },
        families: vec!["users".to_owned()],
        rows: BTreeMap::from([("users".to_owned(), migratable)]),
        estimated_seconds: Some(90),
        reasons: reasons
            .iter()
            .map(|(code, count)| ReasonSummary {
                code: (*code).to_owned(),
                class: thorium::models::schema::ObjectClass::Unrecognized,
                count: *count,
            })
            .collect(),
        more: 0,
        wait: wait.map(|retry_after_secs| CheckWait {
            reason: "workers are still registered".to_owned(),
            retry_after_secs,
        }),
        summary: None,
        loss: None,
    };
    serde_json::to_value(report).expect("a report serializes")
}

/// Build the termination summary of a run that finished
///
/// # Arguments
///
/// * `outcome` - How the run ended
/// * `migratable` - How many objects it converted
/// * `groups` - The issue groups it left as `(code, count)`
fn run_summary(outcome: &str, migratable: u64, groups: &[(&str, u64)]) -> Value {
    // the counts of the first pass and the largest groups
    json!({
        "outcome": outcome,
        "migration": "users-v2",
        "counts": PassCounts {
            read: 10,
            current: 10 - migratable,
            migratable,
            written: migratable,
            ..PassCounts::default()
        },
        "groups": groups
            .iter()
            .map(|(code, count)| json!({"code": code, "class": "unrecognized", "count": count}))
            .collect::<Vec<_>>(),
    })
}

/// Approve the first fixture step with a backup
fn approved() -> Value {
    // the approval an admin adds once a backup exists
    json!([{"step": "users-v2", "backup": "snap-1"}])
}

/// Get the migration Jobs created, as posted
///
/// # Arguments
///
/// * `scene` - The scene
fn migrations(scene: &Scene) -> Vec<Value> {
    // the creates of the migration kind
    scene.sim.created_of("migration")
}

/// A gated step is checked by a read-only Job and blocks for an approval naming a backup, whose
/// message names the check's findings, `thoradm backup`, and the page on backup limitations; no
/// migration Job is created without an approval or with a blank backup, and one is with a backup
#[tokio::test]
async fn a_gated_step_is_checked_then_waits_for_an_approval_with_a_backup() {
    // a step that reads ten objects and finds three to migrate
    let mut scene = Scene::new(ONE, &json!([]), &[("api", 3), ("scaler", 1)]);
    let report = check_report("users-v2", 3, &[("malformed_json:settings", 1)], None);
    scene.settle_checks(&report, 3).await;
    // a read-only check Job ran, and no migration Job
    let checks = scene.sim.created_of("migration-check");
    assert_eq!(checks.len(), 1, "{checks:#?}");
    assert_eq!(migrations(&scene).len(), 0);
    let args = Scene::args(&checks[0]);
    assert_eq!(&args[..4], ["migrate", "check", "--id", "users-v2"]);
    assert!(
        !args
            .iter()
            .any(|arg| arg == "--running-entry" || arg == "--backup")
    );
    // the pods run as the jobs account with no token
    let pod = &checks[0]["spec"]["template"]["spec"];
    assert_eq!(pod["serviceAccountName"], "thorium-jobs");
    assert_eq!(pod["automountServiceAccountToken"], false);
    // the step waits for an approval and says what it found and how to take the backup
    let step = scene.step("users-v2");
    assert_eq!(step.state, StepState::Blocked, "{step:?}");
    let message = step.message.expect("a message");
    assert!(message.contains("3 of 10 objects"), "{message}");
    assert!(message.contains("malformed_json:settings"), "{message}");
    assert!(message.contains("thoradm backup"), "{message}");
    assert!(message.contains("taking_backups.md"), "{message}");
    assert!(
        message.contains("backup: <where your backup is>"),
        "{message}"
    );
    // an approval without a backup doesn't start it, and says so
    scene.approve(&json!([{"step": "users-v2", "backup": "  "}]));
    scene.settle_checks(&report, 3).await;
    assert_eq!(migrations(&scene).len(), 0);
    assert!(
        scene
            .step("users-v2")
            .message
            .expect("message")
            .contains("names no backup")
    );
    // an approval naming a backup starts it
    scene.approve(&approved());
    scene.rounds(2).await;
    let created = migrations(&scene);
    assert_eq!(created.len(), 1, "{created:#?}");
    assert_eq!(
        Scene::flag(&created[0], "--backup").as_deref(),
        Some("snap-1")
    );
    assert_eq!(
        created[0]["metadata"]["annotations"]["thorium.sandia.gov/backup"],
        "snap-1"
    );
    assert_eq!(scene.step("users-v2").state, StepState::Running);
}

/// A step's check is made once per plan: no more check Jobs over ten requeues, one more for a
/// changed approval, one more per step for the refresh annotation, and two gated steps' checks run
/// one after the other
#[tokio::test]
async fn checks_are_cached_and_run_one_at_a_time() {
    // two gated steps, so two checks wait to run
    let mut scene = Scene::new(TWO, &json!([]), &[]);
    scene.rounds(2).await;
    // only one check Job runs at a time
    assert_eq!(scene.sim.created_of("migration-check").len(), 1);
    let report = check_report("users-v2", 3, &[], None);
    scene.finish_checks(&report);
    scene.rounds(3).await;
    scene.finish_checks(&report);
    scene.rounds(3).await;
    assert_eq!(scene.sim.created_of("migration-check").len(), 2);
    // ten requeues of the blocked steps create no more
    scene.rounds(10).await;
    assert_eq!(scene.sim.created_of("migration-check").len(), 2);
    // a changed approval is checked again
    scene.approve(&json!([{"step": "users-v2", "backup": ""}]));
    scene.rounds(2).await;
    assert_eq!(scene.sim.created_of("migration-check").len(), 3);
    scene.finish_checks(&report);
    scene.rounds(3).await;
    // so is the refresh annotation, which checks each step once more, one after the other
    scene.annotate("thorium.sandia.gov/refresh-check", "1");
    scene.rounds(2).await;
    assert_eq!(scene.sim.created_of("migration-check").len(), 4);
    scene.finish_checks(&report);
    scene.rounds(3).await;
    scene.finish_checks(&report);
    scene.rounds(3).await;
    assert_eq!(scene.sim.created_of("migration-check").len(), 5);
    // and then no more, however long the annotation stays
    scene.rounds(10).await;
    assert_eq!(scene.sim.created_of("migration-check").len(), 5);
}

/// With the report cached, the check result shows in the status entry of the step that wasn't
/// reached yet
#[tokio::test]
async fn a_later_steps_scope_is_shown_before_it_is_reached() {
    // the first step has no approval, so the second is never run
    let scene = Scene::new(TWO, &json!([]), &[]);
    let report = check_report("tags-v2", 4, &[], None);
    scene.settle_checks(&report, 6).await;
    let message = scene.step("tags-v2").message.expect("the scope");
    assert!(message.contains("4 of 10 objects"), "{message}");
}

/// A step whose objects are all current is skipped without an approval
#[tokio::test]
async fn a_step_with_nothing_to_change_is_skipped() {
    // the check finds every object current
    let scene = Scene::new(ONE, &json!([]), &[]);
    let report = check_report("users-v2", 0, &[], None);
    scene.settle_checks(&report, 4).await;
    assert_eq!(migrations(&scene).len(), 0);
    let entries = scene.history("users-v2");
    assert_eq!(
        entries.last().map(|entry| entry.outcome),
        Some(StepState::Skipped)
    );
}

/// An approval records the audit trail: the backup, the work the run found, the Job, the chain, and
/// the outcome, after the `Running` entry that holds the paused counts
#[tokio::test]
async fn a_finished_step_records_the_approval_and_what_it_found() {
    // an approved step that runs and finishes with an issue group
    let scene = Scene::new(ONE, &approved(), &[("api", 3), ("scaler", 1)]);
    let report = check_report("users-v2", 3, &[], None);
    scene.settle_checks(&report, 3).await;
    let job = scene.sim.jobs("migration").remove(0);
    scene.sim.finish(
        &job,
        &Finish::Complete(run_summary(
            "done_with_issues",
            3,
            &[("malformed_json:settings", 2)],
        )),
    );
    scene.rounds(2).await;
    // the history holds the start with its counts, then the outcome
    let entries = scene.history("users-v2");
    let running = entries
        .iter()
        .find(|entry| entry.outcome == StepState::Running)
        .expect("started");
    assert_eq!(running.paused.get("api"), Some(&3));
    assert_eq!(running.paused.get("scaler"), Some(&1));
    assert_eq!(running.job.as_deref(), Some(job.as_str()));
    let done = entries
        .iter()
        .find(|entry| entry.outcome == StepState::Done)
        .expect("finished");
    assert_eq!(done.backup.as_deref(), Some("snap-1"));
    assert_eq!(done.job.as_deref(), Some(job.as_str()));
    assert_eq!(done.chain, vec!["users-v2".to_owned()]);
    assert_eq!(
        done.work.as_deref(),
        Some("10 objects read: 7 current, 3 migratable, 0 unrecognized, 0 error; 3 written")
    );
    let issues = done.issues.as_ref().expect("issues");
    assert_eq!((issues.groups, issues.objects), (1, 2));
    // the issues are in the status, with the codes and the command to read them
    let upgrade = scene.sim.status().upgrade.expect("upgrade");
    let condition = upgrade
        .conditions
        .iter()
        .find(|condition| condition.kind == UpgradeConditionType::MigrationIssues)
        .expect("a MigrationIssues condition");
    assert_eq!(condition.status, "True");
    assert!(
        condition.message.contains("2 objects in 1 groups"),
        "{}",
        condition.message
    );
    assert!(
        condition
            .message
            .contains("malformed_json:settings (unrecognized, 2)")
    );
    assert!(
        condition
            .message
            .contains("thoradm migrate issues --id users-v2")
    );
}

/// Seed the history with a step that finished with issues, as an earlier step of the upgrade did
///
/// # Arguments
///
/// * `scene` - The scene, whose recorded state gains the entry
/// * `step` - The step that finished
/// * `objects` - How many objects its issue groups hold
fn finished_with_issues(scene: &Scene, step: &str, objects: u64) {
    // the entry a finished migration leaves, with its issue summary
    let mut state = scene.sim.state().expect("state");
    state.record(AppliedStep {
        revision: "2026-10-v02".to_owned(),
        step: step.to_owned(),
        outcome: StepState::Done,
        issues: Some(thorium::models::upgrades::IssueSummary {
            groups: 1,
            objects,
            accepted: Vec::new(),
        }),
        ..AppliedStep::default()
    });
    scene.sim.set_state(&state);
}

/// Run the approved `tags-v2` step to the end with the issue groups the run leaves
///
/// # Arguments
///
/// * `scene` - The scene
/// * `groups` - The issue groups the run leaves as `(code, count)`
async fn run_tags(scene: &Scene, groups: &[(&str, u64)]) {
    // check, then run, then read the finished Job
    let report = check_report("tags-v2", 3, &[], None);
    scene.settle_checks(&report, 3).await;
    let job = scene.sim.jobs("migration").remove(0);
    let outcome = if groups.is_empty() {
        "done"
    } else {
        "done_with_issues"
    };
    scene
        .sim
        .finish(&job, &Finish::Complete(run_summary(outcome, 3, groups)));
    scene.rounds(2).await;
}

/// Get the `MigrationIssues` condition of the status
///
/// # Arguments
///
/// * `scene` - The scene
fn issues_condition(scene: &Scene) -> thorium::models::upgrades::UpgradeCondition {
    // the condition as the operator last reported it
    scene
        .sim
        .status()
        .upgrade
        .expect("upgrade")
        .conditions
        .into_iter()
        .find(|condition| condition.kind == UpgradeConditionType::MigrationIssues)
        .expect("a MigrationIssues condition")
}

/// The condition lists every step of the upgrade that ended with issues, the newest first, and a
/// later step that ended clean doesn't hide an earlier one's
#[tokio::test]
async fn the_issues_condition_lists_every_step_that_ended_with_issues() {
    // one step finished with issues earlier, and the next step finishes with issues too
    let scene = Scene::new(TWO, &json!([{"step": "tags-v2", "backup": "snap-1"}]), &[]);
    finished_with_issues(&scene, "users-v2", 2);
    run_tags(&scene, &[("bad_type:x", 5)]).await;
    let condition = issues_condition(&scene);
    assert_eq!(condition.status, "True");
    let message = condition.message;
    let (tags, users) = (message.find("tags-v2:"), message.find("users-v2:"));
    assert!(tags.is_some() && users.is_some(), "{message}");
    assert!(tags < users, "{message}");
    assert!(message.contains("bad_type:x"), "{message}");
    assert!(message.contains("2 objects in 1 groups"), "{message}");
    assert!(
        message.contains("thoradm migrate issues --id users-v2"),
        "{message}"
    );
    // a following reconcile leaves it as it is
    scene.rounds(2).await;
    assert_eq!(issues_condition(&scene).message, message);
}

/// A step that ends with no issues leaves an earlier step's issues in the condition
#[tokio::test]
async fn a_clean_step_doesnt_clear_the_issues_of_an_earlier_one() {
    // one step finished with issues earlier, and the next one finishes clean
    let scene = Scene::new(TWO, &json!([{"step": "tags-v2", "backup": "snap-1"}]), &[]);
    finished_with_issues(&scene, "users-v2", 2);
    run_tags(&scene, &[]).await;
    let condition = issues_condition(&scene);
    assert_eq!(condition.status, "True", "{}", condition.message);
    assert!(
        condition.message.contains("users-v2:"),
        "{}",
        condition.message
    );
}

/// An approval for a step that isn't in the plan, or is already done, is reported as stale
#[tokio::test]
async fn approvals_for_steps_outside_the_plan_are_stale() {
    // approvals for a step of the plan and for two that aren't
    let scene = Scene::new(
        ONE,
        &json!([
            {"step": "users-v2", "backup": "snap-1"},
            {"step": "gone-step", "backup": "snap-1"},
            {"step": "elastic-reindex-keyword-mappings", "backup": "snap-1"}
        ]),
        &[],
    );
    scene.rounds(1).await;
    let stale = scene.sim.status().upgrade.expect("upgrade").stale_approvals;
    assert_eq!(stale.len(), 2, "{stale:?}");
    assert!(stale.iter().any(|entry| entry.starts_with("gone-step")));
    assert!(
        stale
            .iter()
            .any(|entry| entry.starts_with("elastic-reindex"))
    );
    assert!(!stale.iter().any(|entry| entry.starts_with("users-v2")));
}

/// A stale approval leaves the status once it is removed from the values
#[tokio::test]
async fn a_stale_approval_is_cleared_once_it_is_gone() {
    // an approval for a step that doesn't exist
    let mut scene = Scene::new(
        ONE,
        &json!([{"step": "gone-step", "backup": "snap-1"}]),
        &[],
    );
    scene.rounds(1).await;
    let stale = scene.sim.status().upgrade.expect("upgrade").stale_approvals;
    assert_eq!(stale.len(), 1, "{stale:?}");
    // removed from the values, it is no longer reported
    scene.approve(&json!([]));
    scene.rounds(1).await;
    let stale = scene.sim.status().upgrade.expect("upgrade").stale_approvals;
    assert!(stale.is_empty(), "{stale:?}");
}

/// A Job whose pod was killed past the retry limit holds the step failed under the same approval,
/// with the last 20 log lines in the status and only the Job and pod names in the history
#[tokio::test]
async fn a_failed_job_holds_the_step_with_a_capped_log_tail() {
    // a started step whose pod logged 500 lines and was killed past the backoff limit
    let scene = Scene::new(ONE, &approved(), &[("api", 3)]);
    let report = check_report("users-v2", 3, &[], None);
    scene.settle_checks(&report, 3).await;
    let job = scene.sim.jobs("migration").remove(0);
    scene.sim.finish(
        &job,
        &Finish::Failed {
            reason: "BackoffLimitExceeded".to_owned(),
            summary: None,
            log_lines: 500,
        },
    );
    scene.rounds(3).await;
    // the step is held failed, and no second Job starts under the same approval
    let step = scene.step("users-v2");
    assert_eq!(step.state, StepState::Failed, "{step:?}");
    assert!(
        step.message
            .expect("message")
            .contains("BackoffLimitExceeded")
    );
    assert_eq!(migrations(&scene).len(), 1);
    // the status has the last 20 lines
    let failure = scene
        .sim
        .status()
        .upgrade
        .expect("upgrade")
        .failure
        .expect("a failure");
    assert_eq!(failure.job, job);
    assert_eq!(failure.log_tail.len(), 20);
    assert_eq!(
        failure.log_tail.last().map(String::as_str),
        Some("line 500")
    );
    // the history keeps the names and no log lines
    let entries = scene.history("users-v2");
    let failed = entries
        .iter()
        .find(|entry| entry.outcome == StepState::Failed)
        .expect("recorded");
    let recorded = failed.failure.as_ref().expect("a failure");
    assert_eq!(recorded.job, job);
    assert!(recorded.pod.is_some());
    assert_eq!(recorded.log_tail.len(), 0);
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.outcome == StepState::Failed)
            .count(),
        1
    );
}

/// The Job's failure policy fails it at once for thoradm's data-issue and systematic exit codes, so
/// a run over its group limit creates one pod, and ignores an eviction
#[tokio::test]
async fn the_job_fails_at_once_for_data_issues_and_never_counts_an_eviction() {
    // a started step
    let scene = Scene::new(ONE, &approved(), &[]);
    let report = check_report("users-v2", 3, &[], None);
    scene.settle_checks(&report, 3).await;
    let job = &migrations(&scene)[0];
    let policy = &job["spec"]["podFailurePolicy"]["rules"];
    let fail_codes = policy[0]["onExitCodes"]["values"]
        .as_array()
        .expect("codes");
    for code in [1, 2, 3, 5, 6] {
        assert!(fail_codes.contains(&json!(code)), "{code}");
    }
    // a transient exit and a killed pod count against the backoff limit
    assert!(!fail_codes.contains(&json!(4)));
    assert!(!fail_codes.contains(&json!(137)));
    assert_eq!(policy[1]["action"], "Ignore");
    assert_eq!(policy[1]["onPodConditions"][0]["type"], "DisruptionTarget");
    assert_eq!(job["spec"]["backoffLimit"], 2);
    assert_eq!(
        job["spec"]["template"]["spec"]["terminationGracePeriodSeconds"],
        120
    );
    // a run over its limit leaves its largest groups in the step's message and no second Job
    let name = scene.sim.jobs("migration").remove(0);
    scene.sim.finish(
        &name,
        &Finish::Failed {
            reason: "PodFailurePolicy".to_owned(),
            summary: Some(run_summary(
                "failed_issues",
                0,
                &[
                    ("a:x", 9),
                    ("b:x", 8),
                    ("c:x", 7),
                    ("d:x", 6),
                    ("e:x", 5),
                    ("f:x", 1),
                ],
            )),
            log_lines: 3,
        },
    );
    scene.rounds(3).await;
    assert_eq!(migrations(&scene).len(), 1);
    let message = scene.step("users-v2").message.expect("message");
    for code in ["a:x (unrecognized, 9)", "b:x", "c:x", "d:x", "e:x"] {
        assert!(message.contains(code), "{message}");
    }
    assert!(!message.contains("f:x"), "{message}");
}

/// A failed step stays held once its Job is deleted, as the Job's TTL does a week later: no Job is
/// created again under the same approval, until the approval's backup changes
#[tokio::test]
async fn a_failed_step_stays_held_after_its_job_is_deleted() {
    // a step whose Job failed over its issue limit
    let mut scene = Scene::new(ONE, &approved(), &[("api", 3)]);
    let report = check_report("users-v2", 3, &[], None);
    scene.settle_checks(&report, 3).await;
    let job = scene.sim.jobs("migration").remove(0);
    scene.sim.finish(
        &job,
        &Finish::Failed {
            reason: "PodFailurePolicy".to_owned(),
            summary: Some(run_summary("failed_issues", 0, &[("bad_type:x", 6)])),
            log_lines: 3,
        },
    );
    scene.rounds(3).await;
    assert_eq!(scene.step("users-v2").state, StepState::Failed);
    let recorded = scene.history("users-v2").len();
    // the Job goes, as its TTL or an admin deletes it
    scene.sim.reap(&job);
    scene.rounds(3).await;
    // the step stays failed with the advice to change the approval, and no Job is created again
    let step = scene.step("users-v2");
    assert_eq!(step.state, StepState::Failed, "{step:?}");
    assert!(
        step.message
            .as_deref()
            .is_some_and(|message| message.contains("not retried by itself")),
        "{step:?}"
    );
    assert_eq!(migrations(&scene).len(), 1);
    let jobs = scene.sim.jobs("migration");
    assert!(jobs.is_empty(), "{jobs:?}");
    assert_eq!(scene.history("users-v2").len(), recorded);
    // changed accepted codes are a fresh approval, which starts a new Job
    scene.approve(&json!([{
        "step": "users-v2",
        "backup": "snap-1",
        "accept_issues": ["bad_type:x"]
    }]));
    scene.rounds(3).await;
    let created = migrations(&scene);
    assert_eq!(created.len(), 2, "{created:#?}");
    assert_ne!(
        created[1]["metadata"]["name"],
        created[0]["metadata"]["name"]
    );
    assert_eq!(scene.step("users-v2").state, StepState::Running);
}

/// Fail a step's first run under no accepted codes and a second under `bad_type:x`, reaping each
/// Job, so a third run under the first approval reproduces the first Job's name, and return how
/// both runs failed
///
/// # Arguments
///
/// * `scene` - The scene, approved for `users-v2` under `snap-1` with no codes
async fn fail_two_runs_and_reap_them(scene: &mut Scene) -> Finish {
    // the first run fails under snap-1 with no accepted codes, and its Job is reaped
    let report = check_report("users-v2", 3, &[], None);
    scene.settle_checks(&report, 3).await;
    let first = scene.sim.jobs("migration").remove(0);
    let failed = Finish::Failed {
        reason: "PodFailurePolicy".to_owned(),
        summary: Some(run_summary("failed_issues", 0, &[("bad_type:x", 6)])),
        log_lines: 3,
    };
    scene.sim.finish(&first, &failed);
    scene.rounds(3).await;
    scene.sim.reap(&first);
    scene.rounds(2).await;
    // a second run under changed codes fails too, and its Job is reaped
    scene.approve(&json!([{
        "step": "users-v2",
        "backup": "snap-1",
        "accept_issues": ["bad_type:x"]
    }]));
    scene.rounds(3).await;
    let second = scene.sim.jobs("migration").remove(0);
    scene.sim.finish(&second, &failed);
    scene.rounds(3).await;
    scene.sim.reap(&second);
    scene.rounds(2).await;
    assert_eq!(migrations(scene).len(), 2);
    failed
}

/// A run started under an approval tuple an earlier failed run used, whose Job isn't created yet,
/// resumes instead of being held by the earlier run's failure
#[tokio::test]
async fn a_reused_approval_resumes_its_open_run_after_the_old_run_failed() {
    // two failed runs whose Jobs are reaped
    let mut scene = Scene::new(ONE, &approved(), &[("api", 3)]);
    fail_two_runs_and_reap_them(&mut scene).await;
    // the api was brought back up and is slow to stop, so the next start waits for its pods
    scene.sim.add_deployment("api", 3);
    scene.sim.stick("api");
    // the first approval again: with no live Jobs its run reproduces the first Job's name
    scene.approve(&json!([{"step": "users-v2", "backup": "snap-1"}]));
    scene.rounds(2).await;
    let step = scene.step("users-v2");
    assert_ne!(step.state, StepState::Failed, "{step:?}");
    // once the api's pods stop, the open run creates its Job
    scene.sim.unstick("api");
    scene.rounds(2).await;
    let created = migrations(&scene);
    assert_eq!(created.len(), 3, "{created:#?}");
    assert_eq!(
        created[2]["metadata"]["name"],
        created[0]["metadata"]["name"]
    );
    assert_eq!(scene.step("users-v2").state, StepState::Running);
}

/// A run under a reused approval tuple whose Job fails and is then deleted stays held, instead of
/// running again
#[tokio::test]
async fn a_reused_tuple_run_that_fails_stays_held_after_its_job_is_deleted() {
    // two failed runs whose Jobs are reaped
    let mut scene = Scene::new(ONE, &approved(), &[("api", 3)]);
    let failed = fail_two_runs_and_reap_them(&mut scene).await;
    // the first approval again: the new run reuses the first Job's name, runs, and fails
    scene.approve(&json!([{"step": "users-v2", "backup": "snap-1"}]));
    scene.rounds(3).await;
    let created = migrations(&scene);
    assert_eq!(created.len(), 3, "{created:#?}");
    let third = scene.sim.jobs("migration").remove(0);
    scene.sim.finish(&third, &failed);
    scene.rounds(3).await;
    assert_eq!(scene.step("users-v2").state, StepState::Failed);
    // its Job is deleted, as its TTL does: the step stays held and no fourth Job is created
    scene.sim.reap(&third);
    scene.rounds(3).await;
    let created = migrations(&scene);
    let step = scene.step("users-v2");
    assert_eq!(created.len(), 3, "{step:?} {created:#?}");
    assert_eq!(step.state, StepState::Failed, "{step:?}");
}

/// Changing the approval's backup on a failed step starts a new Job that resumes, while the same
/// approval stays held
#[tokio::test]
async fn a_changed_backup_reapproves_a_failed_step() {
    // a failed step
    let mut scene = Scene::new(ONE, &approved(), &[]);
    let report = check_report("users-v2", 3, &[], None);
    scene.settle_checks(&report, 3).await;
    let first = scene.sim.jobs("migration").remove(0);
    scene.sim.finish(
        &first,
        &Finish::Failed {
            reason: "BackoffLimitExceeded".to_owned(),
            summary: None,
            log_lines: 1,
        },
    );
    scene.rounds(3).await;
    assert_eq!(migrations(&scene).len(), 1);
    // a newer backup is a fresh approval
    scene.approve(&json!([{"step": "users-v2", "backup": "snap-2"}]));
    scene.rounds(3).await;
    let created = migrations(&scene);
    assert_eq!(created.len(), 2, "{created:#?}");
    assert_ne!(
        created[1]["metadata"]["name"],
        created[0]["metadata"]["name"]
    );
    assert_eq!(
        Scene::flag(&created[1], "--backup").as_deref(),
        Some("snap-2")
    );
    // the new Job tells thoradm to take the lease an earlier pod left
    assert!(
        Scene::args(&created[1])
            .iter()
            .any(|arg| arg == "--force-break-lease")
    );
    assert_eq!(scene.step("users-v2").state, StepState::Running);
}

/// A failed first run of a step that converts the users layout leaves the recorded layout ahead
/// of the data, so the drift read shows the users as changed; a changed backup still runs the
/// step again, since the step itself converts that layout
#[tokio::test]
async fn a_changed_backup_reruns_a_converting_step_whose_layout_shows_as_drift() {
    // a converting step that failed on its first run
    let mut scene = Scene::new(CONVERTING, &approved(), &[]);
    let report = check_report("users-v2", 3, &[], None);
    scene.settle_checks(&report, 3).await;
    let first = scene.sim.jobs("migration").remove(0);
    scene.sim.finish(
        &first,
        &Finish::Failed {
            reason: "BackoffLimitExceeded".to_owned(),
            summary: None,
            log_lines: 1,
        },
    );
    scene.rounds(3).await;
    assert_eq!(migrations(&scene).len(), 1);
    // the next drift read finds the users' layout changed, as the schema apply recorded the new one
    let mut state = scene.sim.state().expect("state");
    let changed = DriftSummary {
        changed: 1,
        objects: vec![DriftEntry {
            backend: SchemaBackend::Redis,
            object: "users".to_owned(),
            kind: DriftKind::Changed,
            fingerprint: None,
            lapsed: false,
        }],
        backends: BTreeMap::from([(SchemaBackend::Redis, BackendSchemaStatus::Drift)]),
        ..DriftSummary::default()
    };
    super::drift::note(&mut state, Some(&changed), "[]");
    scene.sim.set_state(&state);
    // a newer backup starts a new Job instead of blocking on the step's own target
    scene.approve(&json!([{"step": "users-v2", "backup": "snap-2"}]));
    scene.rounds(3).await;
    let created = migrations(&scene);
    assert_eq!(created.len(), 2, "{created:#?}");
    assert_eq!(scene.step("users-v2").state, StepState::Running);
}

/// Accepted issue codes re-approve a failed step: the Job gets them, the entry records the ones that
/// matched, and a code that matched no group is ignored and shown
#[tokio::test]
async fn accepted_issue_codes_reapprove_a_failed_step() {
    // a step that failed on two groups
    let mut scene = Scene::new(ONE, &approved(), &[]);
    let report = check_report("users-v2", 3, &[], None);
    scene.settle_checks(&report, 3).await;
    let first = scene.sim.jobs("migration").remove(0);
    scene.sim.finish(
        &first,
        &Finish::Failed {
            reason: "PodFailurePolicy".to_owned(),
            summary: Some(run_summary(
                "failed_issues",
                0,
                &[("bad_type:x", 6), ("missing_field:y", 2)],
            )),
            log_lines: 1,
        },
    );
    scene.rounds(2).await;
    assert_eq!(scene.step("users-v2").state, StepState::Failed);
    // accept the big group and a code no group has
    scene.approve(&json!([{
        "step": "users-v2",
        "backup": "snap-1",
        "accept_issues": ["bad_type:x", "nothing:z"]
    }]));
    scene.rounds(3).await;
    let created = migrations(&scene);
    assert_eq!(created.len(), 2, "{created:#?}");
    assert_eq!(
        Scene::flag(&created[1], "--accept-issues").as_deref(),
        Some("bad_type:x,nothing:z")
    );
    // the run finishes with the accepted group
    let second = scene.sim.jobs("migration").pop().expect("a second Job");
    scene.sim.finish(
        &second,
        &Finish::Complete(run_summary("done_with_issues", 8, &[("bad_type:x", 6)])),
    );
    scene.rounds(2).await;
    let done = scene
        .history("users-v2")
        .into_iter()
        .find(|entry| entry.outcome == StepState::Done)
        .expect("finished");
    let issues = done.issues.expect("issues");
    assert_eq!(issues.accepted, vec!["bad_type:x".to_owned()]);
    assert_eq!(done.backup.as_deref(), Some("snap-1"));
    // the ignored code is shown in the status
    let condition = scene
        .sim
        .status()
        .upgrade
        .expect("upgrade")
        .conditions
        .into_iter()
        .find(|condition| condition.kind == UpgradeConditionType::MigrationIssues)
        .expect("a condition");
    assert!(
        condition.message.contains("nothing:z"),
        "{}",
        condition.message
    );
    // a later run of the step doesn't reuse the acceptance
    let state = scene.sim.state().expect("state");
    let approval = thorium::models::upgrades::UpgradeApproval {
        step: "users-v2".to_owned(),
        backup: "snap-1".to_owned(),
        accept_issues: vec!["bad_type:x".to_owned(), "nothing:z".to_owned()],
        ..thorium::models::upgrades::UpgradeApproval::default()
    };
    assert_eq!(
        migration::effective_accepted(&state, "users-v2", &approval),
        vec!["nothing:z".to_owned()]
    );
}

/// Removing the approval while a Job runs deletes the Job in the foreground, and the step returns
/// to blocked once its pod is gone
#[tokio::test]
async fn removing_the_approval_cancels_the_job() {
    // a running Job whose delete leaves it terminating until its pod is gone
    let mut scene = Scene::new(ONE, &approved(), &[]);
    scene.sim.slow_delete();
    let report = check_report("users-v2", 3, &[], None);
    scene.settle_checks(&report, 3).await;
    let job = scene.sim.jobs("migration").remove(0);
    // the approval is removed
    scene.approve(&json!([]));
    scene.rounds(2).await;
    let deletes = scene
        .sim
        .fake()
        .writes()
        .into_iter()
        .filter(|request| request.method == "DELETE" && request.path.ends_with(&job))
        .collect::<Vec<_>>();
    assert_eq!(deletes.len(), 1, "{deletes:?}");
    assert_eq!(deletes[0].body["propagationPolicy"], "Foreground");
    // while its pod stops the step waits, and no new Job starts
    let step = scene.step("users-v2");
    assert_eq!(step.state, StepState::Running);
    assert!(step.message.expect("message").contains("stop"));
    scene.rounds(2).await;
    assert_eq!(migrations(&scene).len(), 1);
    // once the Job is gone the step is blocked again
    scene.sim.reap(&job);
    scene.rounds(2).await;
    assert_eq!(scene.step("users-v2").state, StepState::Blocked);
    assert_eq!(migrations(&scene).len(), 1);
}

/// An operator that restarts mid-step re-attaches to the Job it started, or starts a resuming Job
/// when it is gone, without pausing the components again
#[tokio::test]
async fn a_restarted_operator_reattaches_or_resumes() {
    // a step whose Job runs
    let scene = Scene::new(ONE, &approved(), &[("api", 3), ("scaler", 1)]);
    let report = check_report("users-v2", 3, &[], None);
    scene.settle_checks(&report, 3).await;
    let job = scene.sim.jobs("migration").remove(0);
    let scaled = scene.sim.scaled().len();
    // reconciles after a restart find the same Job and create nothing
    scene.rounds(3).await;
    assert_eq!(migrations(&scene).len(), 1);
    assert_eq!(scene.sim.scaled().len(), scaled);
    // a Job that is gone is made again, resuming under the same approval
    scene.sim.reap(&job);
    scene.rounds(2).await;
    assert_eq!(scene.sim.jobs("migration").len(), 1);
    assert_eq!(scene.sim.scaled().len(), scaled);
    // the history still holds the first counts
    let running = scene
        .history("users-v2")
        .into_iter()
        .filter(|entry| entry.outcome == StepState::Running)
        .collect::<Vec<_>>();
    assert_eq!(running[0].paused.get("api"), Some(&3));
}

/// The step's components are scaled to zero before its Job, with their counts recorded; the Job
/// waits while their pods stop
#[tokio::test]
async fn components_are_paused_before_the_job() {
    // an api that is slow to stop
    let scene = Scene::new(
        ONE,
        &approved(),
        &[("api", 3), ("scaler", 1), ("event-handler", 1)],
    );
    scene.sim.stick("api");
    let report = check_report("users-v2", 3, &[], None);
    scene.settle_checks(&report, 3).await;
    // both paused components were scaled to zero, the event handler left alone
    assert_eq!(scene.sim.replicas("api"), Some(0));
    assert_eq!(scene.sim.replicas("scaler"), Some(0));
    assert_eq!(scene.sim.replicas("event-handler"), Some(1));
    // and no Job runs while the api's pods are still up
    assert_eq!(migrations(&scene).len(), 0);
    assert!(
        scene
            .step("users-v2")
            .message
            .expect("message")
            .contains("api")
    );
    scene.sim.unstick("api");
    scene.rounds(2).await;
    assert_eq!(migrations(&scene).len(), 1);
    // the counts are on the Running entry, which the Job writes into the record first
    let job = &migrations(&scene)[0];
    let entry: AppliedStep =
        serde_json::from_str(&Scene::flag(job, "--running-entry").expect("an entry"))
            .expect("json");
    assert_eq!(entry.outcome, StepState::Running);
    assert_eq!(entry.paused.get("api"), Some(&3));
    assert_eq!(entry.paused.get("scaler"), Some(&1));
}

/// The paused components come back only after the new release's Deployments are applied, each to
/// its recorded count, and the counts come from the history a record rebuilds
#[tokio::test]
async fn paused_components_are_restored_after_their_deployments_are_applied() {
    // a finished step that paused two components
    let scene = Scene::new(ONE, &approved(), &[("api", 3), ("scaler", 1)]);
    let report = check_report("users-v2", 3, &[], None);
    scene.settle_checks(&report, 3).await;
    let job = scene.sim.jobs("migration").remove(0);
    scene
        .sim
        .finish(&job, &Finish::Complete(run_summary("done", 3, &[])));
    scene.rounds(3).await;
    // the components stay at zero through the rest of the plan
    assert_eq!(scene.sim.replicas("api"), Some(0));
    assert_eq!(scene.sim.replicas("scaler"), Some(0));
    // the counts are in the history, so a state rebuilt from the record still has them
    let state = scene.sim.state().expect("state");
    assert_eq!(
        pause::paused(&state),
        BTreeMap::from([("api".to_owned(), 3), ("scaler".to_owned(), 1)])
    );
    let rebuilt = super::record::mirror(&record_of(&state), None);
    assert_eq!(pause::paused(&rebuilt), pause::paused(&state));
    // applying the api's Deployment patches it at zero, then scales it to its count
    let client = scene.sim.fake().client();
    let mut cluster = scene.cluster.clone();
    cluster.status = Some(scene.sim.status());
    let meta = meta_for(cluster, &client);
    let before = scene.sim.scaled().len();
    apply_deployment(&meta, "api").await;
    let scaled = scene.sim.scaled()[before..].to_vec();
    assert_eq!(
        scaled,
        vec![("api".to_owned(), 0), ("api".to_owned(), 3)],
        "{scaled:?}"
    );
    let patches = scene
        .sim
        .fake()
        .writes()
        .into_iter()
        .filter(|request| request.method == "PATCH" && request.path.ends_with("/deployments/api"))
        .collect::<Vec<_>>();
    // the template was patched with zero replicas before the scale up
    assert_eq!(patches[0].body["spec"]["replicas"], 0);
    assert_eq!(patches.last().expect("a patch").body["spec"]["replicas"], 3);
    assert_eq!(scene.sim.replicas("api"), Some(3));
    // the scaler is still paused, and the restore is recorded
    assert_eq!(scene.sim.replicas("scaler"), Some(0));
    let state = scene.sim.state().expect("state");
    assert!(state.applied.iter().any(|entry| entry.step == RESTORE_STEP));
    assert_eq!(
        pause::paused(&state),
        BTreeMap::from([("scaler".to_owned(), 1)])
    );
    // the scaler comes back when its own Deployment is applied
    apply_deployment(&meta, "scaler").await;
    assert_eq!(scene.sim.replicas("scaler"), Some(1));
}

/// Apply a Deployment the way the rollout does, with a template that asks for one replica
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `name` - The Deployment's name
async fn apply_deployment(meta: &crate::k8s::clusters::ClusterMeta, name: &str) {
    // a minimal Deployment with the rollout's usual one replica
    let deployment: k8s_openapi::api::apps::v1::Deployment = serde_json::from_value(json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {"name": name, "namespace": "thorium"},
        "spec": {
            "replicas": 1,
            "selector": {"matchLabels": {"app": name}},
            "template": {
                "metadata": {"labels": {"app": name}},
                "spec": {"containers": [{"name": name, "image": "registry/thorium:1.8.1"}]}
            }
        }
    }))
    .expect("a deployment");
    let hashes = crate::k8s::deployments::RolloutHashes {
        config: "config".to_owned(),
        mounts: "mounts".to_owned(),
    };
    crate::k8s::deployments::create_or_update(deployment, meta, &hashes)
        .await
        .expect("applied");
}

/// Build the record a status Job would return for a state's history
///
/// # Arguments
///
/// * `state` - The state whose history the record holds
fn record_of(state: &UpgradeState) -> thorium::models::schema::DeploymentRecord {
    // the record holds the same revision and history
    serde_json::from_value(json!({
        "revision": state.revision,
        "applied_by_version": "1.8.1",
        "history": state.applied,
        "history_len": state.applied.len(),
        "finished": [],
        "schema": {"deployment": "applied", "backends": {}, "layouts": {}},
        "config": {"partition_sizes": {}, "earliest": {}, "files_password": ""},
    }))
    .expect("a record")
}

/// With the recorded counts lost before the Job's first write, the pause records what the cluster's
/// spec declares and the status says so
#[tokio::test]
async fn lost_pause_counts_fall_back_to_the_spec() {
    // an api already at zero, with nothing recorded about why
    let scene = Scene::new(ONE, &approved(), &[("api", 0), ("scaler", 1)]);
    let report = check_report("users-v2", 3, &[], None);
    scene.settle_checks(&report, 3).await;
    let running = scene
        .history("users-v2")
        .into_iter()
        .find(|entry| entry.outcome == StepState::Running)
        .expect("started");
    // the chart's api runs three replicas
    assert_eq!(running.paused.get("api"), Some(&3), "{running:?}");
    assert_eq!(running.paused.get("scaler"), Some(&1));
    let message = scene.step("users-v2").message.expect("message");
    assert!(message.contains("lost"), "{message}");
    assert!(message.contains("api"), "{message}");
}

/// The event handler, which a fixture step pauses
const EVENT_HANDLER: &[UpgradeComponent] = &[UpgradeComponent::EventHandler];

/// One gated step that pauses the event handler
const EVENTS: Catalog = Catalog {
    edges: &[
        edge(None, "2026-10-v01", &[]),
        edge(
            Some("2026-10-v01"),
            "2026-10-v02",
            &[gated("users-v2", EVENT_HANDLER, &[])],
        ),
    ],
    floors: &[],
};

/// A component at zero that the cluster's spec doesn't declare keeps its observed zero: the pause
/// records zero without calling the count lost, and restoring it leaves it at zero
#[tokio::test]
async fn an_undeclared_component_at_zero_stays_at_zero() {
    // an event handler Deployment at zero that the spec doesn't declare
    let mut scene = Scene::new(EVENTS, &approved(), &[("event-handler", 0)]);
    scene.cluster.spec.components.event_handler = None;
    let report = check_report("users-v2", 3, &[], None);
    scene.settle_checks(&report, 3).await;
    let running = scene
        .history("users-v2")
        .into_iter()
        .find(|entry| entry.outcome == StepState::Running)
        .expect("started");
    // the pause records the observed zero, and the status doesn't call it a lost count
    assert_eq!(running.paused.get("event-handler"), Some(&0), "{running:?}");
    let message = scene.step("users-v2").message.unwrap_or_default();
    assert!(!message.contains("lost"), "{message}");
    // restoring the component leaves it at zero
    let client = scene.sim.fake().client();
    let meta = meta_for(scene.cluster.clone(), &client);
    pause::restore(&meta, "event-handler")
        .await
        .expect("restored");
    assert_eq!(scene.sim.replicas("event-handler"), Some(0));
}

/// No second writing Job starts while one runs or its pod is still terminating, and a read-only
/// check Job still runs beside a writer
#[tokio::test]
async fn one_writing_job_runs_at_a_time() {
    // an approved step and a schema-apply Job that is running
    let scene = Scene::new(TWO, &json!([{"step": "users-v2", "backup": "snap-1"}]), &[]);
    scene.sim.slow_delete();
    let report = check_report("users-v2", 3, &[], None);
    scene.settle_checks(&report, 6).await;
    // the migration Job already started for the approved step; end it, then run a writer beside
    let first = scene.sim.jobs("migration").remove(0);
    scene
        .sim
        .finish(&first, &Finish::Complete(run_summary("done", 3, &[])));
    scene.rounds(2).await;
    let writer = json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {
            "name": "schema-apply-other-aaaaaaaaaaaa",
            "namespace": "thorium",
            "labels": {
                "thorium.sandia.gov/job-kind": "schema-apply",
                "thorium.sandia.gov/step": "other",
            },
        },
        "spec": {"template": {"spec": {"containers": []}}},
    });
    post_job(&scene, &writer).await;
    // approve the second step: its Job waits for the writer, though its check Job may run
    let mut scene = scene;
    scene.approve(&json!([
        {"step": "users-v2", "backup": "snap-1"},
        {"step": "tags-v2", "backup": "snap-1"}
    ]));
    scene.rounds(3).await;
    assert_eq!(migrations(&scene).len(), 1, "{:#?}", migrations(&scene));
    let message = scene.step("tags-v2").message.expect("message");
    assert!(
        message.contains("writing Job schema-apply-other"),
        "{message}"
    );
    // the writer is deleted, but its pod is still terminating, so nothing starts
    delete_job(&scene, "schema-apply-other-aaaaaaaaaaaa").await;
    scene.rounds(2).await;
    assert_eq!(migrations(&scene).len(), 1);
    // once the pod is gone the second Job starts
    scene.sim.reap("schema-apply-other-aaaaaaaaaaaa");
    scene.rounds(3).await;
    assert_eq!(migrations(&scene).len(), 2);
}

/// Create a Job through the fake kube API, as another writer would
///
/// # Arguments
///
/// * `scene` - The scene
/// * `job` - The Job to create
async fn post_job(scene: &Scene, job: &Value) {
    // post it where the operator lists Jobs
    let client = scene.sim.fake().client();
    let request = http::Request::builder()
        .method("POST")
        .uri("/apis/batch/v1/namespaces/thorium/jobs")
        .header("content-type", "application/json")
        .body(serde_json::to_vec(job).expect("json"))
        .expect("a request");
    client.request_text(request).await.expect("created");
}

/// Delete a Job through the fake kube API
///
/// # Arguments
///
/// * `scene` - The scene
/// * `name` - The Job's name
async fn delete_job(scene: &Scene, name: &str) {
    // delete it where the operator lists Jobs
    let client = scene.sim.fake().client();
    let request = http::Request::builder()
        .method("DELETE")
        .uri(format!("/apis/batch/v1/namespaces/thorium/jobs/{name}"))
        .body(Vec::new())
        .expect("a request");
    client.request_text(request).await.expect("deleted");
}

/// A step that reads an earlier step's output is checked again when that step finishes, before its
/// approval, with no component scaled down for it; a step that reads nothing isn't
#[tokio::test]
async fn a_check_runs_again_after_the_step_it_reads_after() {
    // three steps: the second reads the first's output
    let mut scene = Scene::new(THREE, &json!([]), &[("api", 3), ("scaler", 1)]);
    let plan_time = |migration: &str, migratable| check_report(migration, migratable, &[], None);
    // every step is checked once at plan time, one Job at a time
    for _ in 0..8 {
        scene.reconcile().await;
        for job in scene.sim.active("migration-check") {
            let name = job_migration(&scene, &job);
            let report = plan_time(&name, 5);
            scene.sim.leave_result(&job, report);
            scene.sim.finish(
                &job,
                &Finish::Complete(json!({"outcome": "done", "result_id": format!("uid-{job}")})),
            );
        }
    }
    let checks = |scene: &Scene, migration: &str| {
        scene
            .sim
            .created_of("migration-check")
            .into_iter()
            .filter(|job| Scene::flag(job, "--id").as_deref() == Some(migration))
            .count()
    };
    assert_eq!(
        (
            checks(&scene, "users-v2"),
            checks(&scene, "files-v2"),
            checks(&scene, "tags-v2")
        ),
        (1, 1, 1)
    );
    // approve and run the first step to its end
    scene.approve(&json!([{"step": "users-v2", "backup": "snap-1"}]));
    scene.rounds(2).await;
    let job = scene.sim.jobs("migration").remove(0);
    scene
        .sim
        .finish(&job, &Finish::Complete(run_summary("done", 5, &[])));
    scene.rounds(1).await;
    // the second step's check runs again, and the third's doesn't
    let before = scene.sim.scaled().len();
    for _ in 0..6 {
        scene.reconcile().await;
        for job in scene.sim.active("migration-check") {
            let name = job_migration(&scene, &job);
            // the first step's output changed what the second finds
            let report = plan_time(&name, 9);
            scene.sim.leave_result(&job, report);
            scene.sim.finish(
                &job,
                &Finish::Complete(json!({"outcome": "done", "result_id": format!("uid-{job}")})),
            );
        }
    }
    assert_eq!(checks(&scene, "files-v2"), 2);
    assert_eq!(checks(&scene, "tags-v2"), 1);
    assert_eq!(
        scene.sim.scaled().len(),
        before,
        "no component is scaled for a check"
    );
    // the second step shows the re-run's counts
    let message = scene.step("files-v2").message.expect("message");
    assert!(message.contains("9 of 10 objects"), "{message}");
}

/// Get the migration a check Job checks
///
/// # Arguments
///
/// * `scene` - The scene
/// * `job` - The check Job's name
fn job_migration(scene: &Scene, job: &str) -> String {
    // the `--id` of the Job as it was posted
    scene
        .sim
        .created_of("migration-check")
        .into_iter()
        .find(|created| created["metadata"]["name"] == job)
        .and_then(|created| Scene::flag(&created, "--id"))
        .expect("a check Job with an id")
}

/// A step whose earlier step is already finished is checked once
#[tokio::test]
async fn a_check_runs_once_when_the_step_it_reads_after_is_done() {
    // the first step finished before the plan started
    let scene = Scene::new(THREE, &json!([]), &[]);
    let mut state = scene.sim.state().expect("state");
    state.record(AppliedStep {
        revision: "2026-10-v02".to_owned(),
        step: "users-v2".to_owned(),
        outcome: StepState::Done,
        ..AppliedStep::default()
    });
    scene.sim.set_state(&state);
    for _ in 0..8 {
        scene.reconcile().await;
        for job in scene.sim.active("migration-check") {
            let name = job_migration(&scene, &job);
            scene
                .sim
                .leave_result(&job, check_report(&name, 4, &[], None));
            scene.sim.finish(
                &job,
                &Finish::Complete(json!({"outcome": "done", "result_id": format!("uid-{job}")})),
            );
        }
    }
    let files = scene
        .sim
        .created_of("migration-check")
        .into_iter()
        .filter(|job| Scene::flag(job, "--id").as_deref() == Some("files-v2"))
        .count();
    assert_eq!(files, 1);
}

/// Set the time the operator acts at
///
/// # Arguments
///
/// * `at` - The time
fn set_now(at: DateTime<Utc>) {
    // the injected time stands in for the clock on this thread
    NOW.with(|now| *now.borrow_mut() = Some(at));
}

/// A check that asks to wait holds the step `Waiting` with its reason and asks for no approval, and
/// runs again only once the wait, clamped to 5 to 600 seconds, has passed
#[tokio::test]
async fn a_check_that_waits_is_run_again_after_its_wait() {
    // a check that asks to wait 30 seconds
    let start = Utc::now();
    set_now(start);
    let scene = Scene::new(ONE, &approved(), &[("api", 3)]);
    let report = check_report("users-v2", 3, &[], Some(30));
    scene.settle_checks(&report, 3).await;
    assert_eq!(scene.sim.created_of("migration-check").len(), 1);
    let step = scene.step("users-v2");
    assert_eq!(step.state, StepState::Waiting, "{step:?}");
    assert_eq!(
        step.message.as_deref(),
        Some("workers are still registered")
    );
    // an approval is set, but no migration Job starts and nothing is paused
    assert_eq!(migrations(&scene).len(), 0);
    assert_eq!(scene.sim.replicas("api"), Some(3));
    // requeues inside the wait create no check Job
    set_now(start + Duration::seconds(29));
    scene.rounds(3).await;
    assert_eq!(scene.sim.created_of("migration-check").len(), 1);
    // after the wait the next check Job is created
    set_now(start + Duration::seconds(31));
    scene.rounds(2).await;
    assert_eq!(scene.sim.created_of("migration-check").len(), 2);
    // a check that no longer waits ends the waiting: the approved step starts
    scene.finish_checks(&check_report("users-v2", 3, &[], None));
    scene.rounds(4).await;
    assert_eq!(migrations(&scene).len(), 1);
}

/// A step waiting on a check's reported reason keeps showing that reason, in its own message and
/// in the cluster's status message, while the next check Job runs and its report is read
#[tokio::test]
async fn a_waiting_step_keeps_showing_its_reason_while_it_is_checked_again() {
    // a check that asks to wait 30 seconds, then the wait runs out
    let start = Utc::now();
    set_now(start);
    let scene = Scene::new(ONE, &approved(), &[("api", 3)]);
    let report = check_report("users-v2", 3, &[], Some(30));
    scene.settle_checks(&report, 3).await;
    set_now(start + Duration::seconds(31));
    // each reconcile of the second check shows the reason: the Job being created, running, and read
    for _ in 0..6 {
        scene.reconcile().await;
        let step = scene.step("users-v2");
        assert_eq!(step.state, StepState::Waiting, "{step:?}");
        assert_eq!(
            step.message.as_deref(),
            Some("workers are still registered"),
            "{step:?}"
        );
        let message = scene.sim.status().message.unwrap_or_default();
        assert!(
            message.contains("workers are still registered"),
            "{message}"
        );
        scene.finish_checks(&report);
    }
}

/// The wait a check asks for is clamped to between 5 and 600 seconds
#[tokio::test]
async fn the_wait_is_clamped() {
    for (asked, early, late) in [(0, 4, 6), (86_400, 599, 601)] {
        // a check that asks to wait as long as the case says
        let start = Utc::now();
        set_now(start);
        let scene = Scene::new(ONE, &json!([]), &[]);
        let report = check_report("users-v2", 3, &[], Some(asked));
        scene.settle_checks(&report, 3).await;
        assert_eq!(scene.sim.created_of("migration-check").len(), 1, "{asked}");
        set_now(start + Duration::seconds(early));
        scene.rounds(2).await;
        assert_eq!(scene.sim.created_of("migration-check").len(), 1, "{asked}");
        set_now(start + Duration::seconds(late));
        scene.rounds(2).await;
        assert_eq!(scene.sim.created_of("migration-check").len(), 2, "{asked}");
    }
}

/// Over ten requeues only the waiting step's check is created again; once the blocked step is
/// approved its migration Job is created with no check Job before it, and its entry's work equals
/// the class counts of the run; a step whose objects all became current records skipped
#[tokio::test]
async fn only_the_waiting_step_is_checked_again() {
    // one step waits and the other is blocked on a cached report
    let start = Utc::now();
    set_now(start);
    let mut scene = Scene::new(TWO, &json!([]), &[]);
    for _ in 0..8 {
        scene.reconcile().await;
        for job in scene.sim.active("migration-check") {
            let name = job_migration(&scene, &job);
            let wait = (name == "tags-v2").then_some(30);
            scene
                .sim
                .leave_result(&job, check_report(&name, 3, &[], wait));
            scene.sim.finish(
                &job,
                &Finish::Complete(json!({"outcome": "done", "result_id": format!("uid-{job}")})),
            );
        }
    }
    assert_eq!(scene.sim.created_of("migration-check").len(), 2);
    assert_eq!(scene.step("users-v2").state, StepState::Blocked);
    assert_eq!(scene.step("tags-v2").state, StepState::Waiting);
    // ten requeues inside the wait create nothing; after it only the waiting step is checked
    set_now(start + Duration::seconds(10));
    scene.rounds(10).await;
    assert_eq!(scene.sim.created_of("migration-check").len(), 2);
    set_now(start + Duration::seconds(40));
    scene.rounds(2).await;
    let created = scene.sim.created_of("migration-check");
    assert_eq!(created.len(), 3);
    assert_eq!(Scene::flag(&created[2], "--id").as_deref(), Some("tags-v2"));
    // the blocked step is approved: its Job comes with no new check
    scene.approve(&json!([{"step": "users-v2", "backup": "snap-1"}]));
    scene.rounds(2).await;
    assert_eq!(scene.sim.created_of("migration-check").len(), 3);
    let jobs = migrations(&scene);
    assert_eq!(jobs.len(), 1);
    assert_eq!(Scene::flag(&jobs[0], "--id").as_deref(), Some("users-v2"));
    // its entry's work is the class counts of the run
    let job = scene.sim.jobs("migration").remove(0);
    scene
        .sim
        .finish(&job, &Finish::Complete(run_summary("done", 3, &[])));
    scene.rounds(2).await;
    let done = scene
        .history("users-v2")
        .into_iter()
        .find(|entry| entry.outcome == StepState::Done)
        .expect("finished");
    assert_eq!(
        done.work,
        Some(
            PassCounts {
                read: 10,
                current: 7,
                migratable: 3,
                written: 3,
                ..PassCounts::default()
            }
            .work_text()
        )
    );
}

/// A run whose first pass finds every object current is recorded as skipped
#[tokio::test]
async fn a_run_that_changed_nothing_is_recorded_skipped() {
    // the objects became current between the check and the run
    let scene = Scene::new(ONE, &approved(), &[]);
    let report = check_report("users-v2", 3, &[], None);
    scene.settle_checks(&report, 3).await;
    let job = scene.sim.jobs("migration").remove(0);
    scene
        .sim
        .finish(&job, &Finish::Complete(run_summary("done", 0, &[])));
    scene.rounds(2).await;
    let outcomes = scene
        .history("users-v2")
        .into_iter()
        .map(|entry| entry.outcome)
        .collect::<Vec<_>>();
    assert_eq!(outcomes, vec![StepState::Running, StepState::Skipped]);
}

/// A step finished by hand (a `--reason` run whose entry thoradm wrote) is recorded finished
/// without a new Job, and its outcome is recorded once
#[tokio::test]
async fn a_step_finished_by_hand_needs_no_new_job() {
    // a failed step
    let scene = Scene::new(ONE, &approved(), &[]);
    let report = check_report("users-v2", 3, &[], None);
    scene.settle_checks(&report, 3).await;
    let job = scene.sim.jobs("migration").remove(0);
    scene.sim.finish(
        &job,
        &Finish::Failed {
            reason: "PodFailurePolicy".to_owned(),
            summary: Some(run_summary("failed_issues", 0, &[("bad_type:x", 9)])),
            log_lines: 1,
        },
    );
    scene.rounds(2).await;
    assert_eq!(scene.step("users-v2").state, StepState::Failed);
    // an admin runs the group by hand and thoradm writes the step's entry into the record, which the
    // state mirrors
    let mut state = scene.sim.state().expect("state");
    state.record(AppliedStep {
        revision: "2026-10-v02".to_owned(),
        step: "users-v2".to_owned(),
        outcome: StepState::Done,
        by: Some("thoradm 1.8.1 (admin)".to_owned()),
        ..AppliedStep::default()
    });
    scene.sim.set_state(&state);
    scene.rounds(2).await;
    // the plan went on without a new Job
    assert_eq!(migrations(&scene).len(), 1);
    assert_eq!(
        scene.sim.state().expect("state").revision.as_str(),
        "2026-10-v02"
    );
    // the step's outcome is in the history once: the operator adds none beside thoradm's
    let finishing = scene
        .history("users-v2")
        .into_iter()
        .filter(|entry| entry.outcome.finishes())
        .count();
    assert_eq!(finishing, 1);
}

/// With the image's thoradm knowing another catalog, no check or migration Job is created and the
/// status names both catalogs, while the schema-apply Job still runs
#[tokio::test]
async fn catalog_skew_blocks_data_steps_but_not_the_schema_apply() {
    // a plan of the schema-apply step and a gated step, with a status Job reporting another catalog
    let scene = Scene::new(ORDER, &approved(), &[]);
    let ours = thorium::models::schema::catalog_identity(&super::catalog());
    let theirs = thorium::models::schema::CatalogIdentity {
        latest: rev("2026-10-v09"),
        hash: serde_json::from_value(json!("feedface")).expect("a fingerprint"),
    };
    scene.sim.set_catalog(theirs.clone());
    scene.sim.set_record(json!({
        "state": "recorded",
        "record": record_of(&scene.sim.state().expect("state")),
    }));
    // the real ops read the record through a status Job
    let mut cluster = scene.cluster.clone();
    for _ in 0..4 {
        cluster.status = Some(scene.sim.status());
        let client = scene.sim.fake().client();
        let Ok(Gate::Upgrade(pending)) = gate(&client, &tracked(&cluster)).await else {
            panic!("an upgrade");
        };
        let meta = meta_for(cluster.clone(), &client);
        Box::pin(run_with(&meta, pending, &super::Jobs))
            .await
            .expect("run");
        // end whatever schema-apply Job exists
        for job in scene.sim.active("schema-apply") {
            scene.sim.finish(
                &job,
                &Finish::Complete(json!({"outcome": "done", "fingerprint": "f"})),
            );
        }
    }
    // the schema-apply Job ran once and its step finished, and no data Job did
    assert_eq!(scene.sim.created_of("schema-apply").len(), 1);
    assert!(
        scene
            .history("schema-apply")
            .iter()
            .any(|entry| entry.outcome == StepState::Done),
        "{:?}",
        scene.history("schema-apply")
    );
    assert_eq!(scene.sim.created_of("migration-check").len(), 0);
    assert_eq!(migrations(&scene).len(), 0);
    // the condition names both catalogs
    let conditions = scene.sim.status().upgrade.expect("upgrade").conditions;
    let skew = conditions
        .iter()
        .find(|condition| condition.kind == UpgradeConditionType::CatalogSkew)
        .expect("a CatalogSkew condition");
    assert_eq!(skew.status, "True");
    assert!(
        skew.message.contains(theirs.latest.as_str()),
        "{}",
        skew.message
    );
    assert!(
        skew.message.contains(ours.latest.as_str()),
        "{}",
        skew.message
    );
    assert!(skew.message.contains("feedface"), "{}", skew.message);
}

/// The schema-apply step runs before the check of any step that changes data: no check or migration
/// Job exists until the schema-apply Job has finished
#[tokio::test]
async fn the_schema_is_applied_before_any_step_that_changes_data() {
    // a plan of the schema-apply step and an approved gated step
    let scene = Scene::new(ORDER, &approved(), &[("api", 3)]);
    scene.rounds(3).await;
    // only the schema-apply Job exists, and nothing was paused
    assert_eq!(scene.sim.created_of("schema-apply").len(), 1);
    assert_eq!(scene.sim.created_of("migration-check").len(), 0);
    assert_eq!(migrations(&scene).len(), 0);
    assert_eq!(scene.sim.replicas("api"), Some(3));
    // once it finishes the step is checked
    for job in scene.sim.active("schema-apply") {
        scene.sim.finish(
            &job,
            &Finish::Complete(json!({"outcome": "done", "fingerprint": "f"})),
        );
    }
    scene.rounds(2).await;
    assert_eq!(scene.sim.created_of("migration-check").len(), 1);
    // the apply is in the history before the step's start
    let state = scene.sim.state().expect("state");
    let apply = state
        .applied
        .iter()
        .position(|entry| entry.step == "schema-apply");
    assert!(apply.is_some(), "{state:?}");
}

/// `Advance` is what the runner works from: an unapproved step with work to do is held `Blocked` with
/// what its check found, and a step whose check found every object current is skipped
#[tokio::test]
async fn advancing_a_step_says_how_it_stands() {
    // the first step has three objects to change and the second has none
    for (migratable, expect_held) in [(3, true), (0, false)] {
        // the step's check Job is created and ends with its report
        let scene = Scene::new(ONE, &json!([]), &[]);
        let report = check_report("users-v2", migratable, &[], None);
        scene.reconcile().await;
        scene.finish_checks(&report);
        // advance the step directly, over the state the reconcile left
        let mut cluster = scene.cluster.clone();
        cluster.status = Some(scene.sim.status());
        let client = scene.sim.fake().client();
        let Ok(Gate::Upgrade(mut pending)) = gate(&client, &tracked(&cluster)).await else {
            panic!("an upgrade is planned");
        };
        let meta = meta_for(cluster, &client);
        let step = &ONE.edges[1].steps[0];
        // reading the report takes a status Job, so the step waits until it is read
        let mut advance = Advance::Skipped(String::new());
        for _ in 0..6 {
            advance = migration::advance(&meta, &mut pending, 0, step)
                .await
                .expect("advance");
            if !matches!(
                advance,
                Advance::Held {
                    state: StepState::Waiting,
                    ..
                }
            ) {
                break;
            }
        }
        // work to do waits for an approval with the check's findings, and none is skipped
        match advance {
            Advance::Held { state, message } if expect_held => {
                assert_eq!(state, StepState::Blocked, "{message}");
                assert!(message.contains("3 of 10 objects"), "{message}");
            }
            Advance::Skipped(why) if !expect_held => {
                assert!(why.contains("already in the target layout"), "{why}");
            }
            other => panic!("unexpected advance for {migratable} objects: {other:?}"),
        }
    }
}

/// A `Running` entry older than a `reset-state` entry is no live run: nothing re-attaches to it, and
/// the step is checked again from the start
#[tokio::test]
async fn a_reset_ends_an_open_run() {
    // a step that started, then a namespace reset that voided its pass
    let scene = Scene::new(ONE, &approved(), &[("api", 3)]);
    let mut state = scene.sim.state().expect("state");
    state.record(AppliedStep {
        revision: "2026-10-v02".to_owned(),
        step: "users-v2".to_owned(),
        outcome: StepState::Running,
        job: Some("migration-users-v2-000000000000".to_owned()),
        at: Some("2026-10-22T10:00:00Z".to_owned()),
        paused: BTreeMap::from([("api".to_owned(), 3)]),
        ..AppliedStep::default()
    });
    state.record(AppliedStep {
        revision: "2026-10-v01".to_owned(),
        step: "reset-state".to_owned(),
        outcome: StepState::Done,
        at: Some("2026-10-22T11:00:00Z".to_owned()),
        ..AppliedStep::default()
    });
    scene.sim.set_state(&state);
    assert!(migration::open_run(&state, "users-v2").is_none());
    // the step is checked before anything starts
    scene.rounds(2).await;
    assert_eq!(scene.sim.created_of("migration-check").len(), 1);
    assert_eq!(migrations(&scene).len(), 0);
}

/// Build a history entry of a step's run naming its Job
///
/// # Arguments
///
/// * `outcome` - The entry's outcome
/// * `job` - The name of the run's Job
fn job_entry(outcome: StepState, job: &str) -> AppliedStep {
    // a users-v2 entry at the second revision
    AppliedStep {
        revision: "2026-10-v02".to_owned(),
        step: "users-v2".to_owned(),
        outcome,
        job: Some(job.to_owned()),
        ..AppliedStep::default()
    }
}

/// Only a failure recorded since a run of a Job name started counts as that run's: an earlier
/// run's failure under the same name doesn't, while the run's own failure does
#[test]
fn a_failure_counts_only_since_its_runs_start() {
    // two runs that failed, then a run that reuses the first one's Job name
    let first = "migration-users-v2-000000000000";
    let second = "migration-users-v2-222222222222";
    let mut state = UpgradeState::new(rev("2026-10-v02"), "1.8.1");
    state.applied = vec![
        job_entry(StepState::Running, first),
        job_entry(StepState::Failed, first),
        job_entry(StepState::Running, second),
        job_entry(StepState::Failed, second),
        job_entry(StepState::Running, first),
    ];
    // the reused name's run has no failure of its own, so neither lookup finds one
    let open = migration::open_run(&state, "users-v2")
        .expect("an open run")
        .clone();
    assert_eq!(open.job.as_deref(), Some(first));
    assert_eq!(migration::recorded_failure(&state, &open), None);
    assert_eq!(
        migration::failure_since_start(&state, "users-v2", first),
        None
    );
    // the second run's failure is still its own
    assert_eq!(
        migration::failure_since_start(&state, "users-v2", second),
        Some(&state.applied[3])
    );
    // once the reused name's run fails, its own failure is found
    state.applied.push(job_entry(StepState::Failed, first));
    assert_eq!(
        migration::recorded_failure(&state, &open),
        Some(&state.applied[5])
    );
    // the first run alone: its failure is found
    state.applied.truncate(2);
    let open = migration::open_run(&state, "users-v2")
        .expect("an open run")
        .clone();
    assert_eq!(
        migration::recorded_failure(&state, &open),
        Some(&state.applied[1])
    );
}

/// A partition size the schema apply refuses plans a re-bucket step that checks, waits for an
/// approval, pauses the writers, and runs before the schema apply tries again; a refused files
/// password plans no step and is named in the status
#[tokio::test]
// the scenario follows one refusal through to the schema apply running again, so it reads as one
#[allow(clippy::too_many_lines)]
async fn a_refused_partition_size_plans_a_rebucket_step() {
    // a cluster at the first Helm revision whose schema-apply Job the apply refuses
    let cluster = chart_cluster(json!({"approvals": []}));
    let sim = Sim::new(&cluster);
    let mut state = UpgradeState::new(rev("2026-10-v02"), "1.8.1");
    state
        .schema
        .insert(RECORD_MARKER.to_owned(), "scylla".to_owned());
    sim.set_state(&state);
    for (name, replicas) in [
        ("api", 3),
        ("scaler", 1),
        ("baremetal-scaler", 1),
        ("event-handler", 1),
    ] {
        sim.add_deployment(name, replicas);
    }
    sim.auto_complete("schema-status", json!({"outcome": "done"}));
    let ensure = |cluster: ThoriumCluster| {
        let sim = sim.clone();
        async move {
            let client = sim.fake().client();
            let mut cluster = cluster;
            cluster.status = Some(sim.status());
            let meta = meta_for(cluster, &client);
            super::schema_apply::ensure_at(&meta, Some("fp"))
                .await
                .expect("ensure")
        }
    };
    // the apply runs first and is refused
    assert!(Box::pin(ensure(cluster.clone())).await.is_some());
    assert_eq!(sim.created_of("schema-apply").len(), 1);
    let apply = sim.jobs("schema-apply").remove(0);
    sim.finish(
        &apply,
        &Finish::Failed {
            reason: "PodFailurePolicy".to_owned(),
            summary: Some(json!({
                "outcome": "failed_systematic",
                "exit_code": 1,
                "refused": ["thorium.files.partition_size"],
            })),
            log_lines: 2,
        },
    );
    // the refusal plans the re-bucket step, whose check runs before anything else
    for _ in 0..2 {
        assert!(Box::pin(ensure(cluster.clone())).await.is_some());
    }
    let checks = sim.created_of("migration-check");
    assert_eq!(checks.len(), 1, "{checks:#?}");
    assert_eq!(
        Scene::flag(&checks[0], "--id").as_deref(),
        Some("rebucket-files")
    );
    assert_eq!(sim.created_of("schema-apply").len(), 1);
    assert_eq!(sim.scaled().len(), 0);
    // the check's report says how many rows move and for how long
    let check = sim.active("migration-check").remove(0);
    sim.leave_result(&check, check_report("rebucket-files", 4, &[], None));
    sim.finish(
        &check,
        &Finish::Complete(json!({"outcome": "done", "result_id": format!("uid-{check}")})),
    );
    for _ in 0..3 {
        Box::pin(ensure(cluster.clone())).await;
    }
    let step = sim
        .status()
        .upgrade
        .expect("upgrade")
        .steps
        .into_iter()
        .find(|step| step.step == "rebucket-files")
        .expect("the step is in the status");
    assert_eq!(step.state, StepState::Blocked, "{step:?}");
    assert_eq!(step.kind, StepKind::Data);
    let message = step.message.expect("message");
    assert!(message.contains("4 rows to rewrite"), "{message}");
    assert!(message.contains("about 2 minutes"), "{message}");
    assert!(message.contains("approval"), "{message}");
    // the writers are still up and no migration Job exists without an approval
    assert_eq!(sim.created_of("migration").len(), 0);
    assert_eq!(sim.replicas("api"), Some(3));
    // approved, the API and every Scylla writer are paused before the Job, and nothing else of the
    // components changes
    let approved =
        chart_cluster(json!({"approvals": [{"step": "rebucket-files", "backup": "snap-1"}]}));
    for _ in 0..3 {
        Box::pin(ensure(approved.clone())).await;
    }
    for name in ["api", "scaler", "baremetal-scaler", "event-handler"] {
        assert_eq!(sim.replicas(name), Some(0), "{name}");
    }
    let jobs = sim.created_of("migration");
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        Scene::flag(&jobs[0], "--id").as_deref(),
        Some("rebucket-files")
    );
    let writes = sim.fake().writes();
    let first_job = writes
        .iter()
        .position(|request| {
            request.method == "POST"
                && request.path.ends_with("/jobs")
                && request.body["metadata"]["labels"]["thorium.sandia.gov/job-kind"] == "migration"
        })
        .expect("the migration Job was created");
    let scaled_before = writes[..first_job]
        .iter()
        .filter(|request| request.method == "PATCH" && request.path.contains("/deployments/"))
        .count();
    assert_eq!(scaled_before, 4);
    // every Deployment write was a scale down: no pod of the new release started
    assert!(
        writes
            .iter()
            .filter(|request| request.path.contains("/deployments"))
            .all(|request| request.method == "PATCH" && request.body["spec"]["replicas"] == 0)
    );
    // once the rows moved, the schema apply runs again under a new name
    let job = sim.jobs("migration").remove(0);
    sim.finish(&job, &Finish::Complete(run_summary("done", 4, &[])));
    for _ in 0..3 {
        Box::pin(ensure(approved.clone())).await;
    }
    let applies = sim.created_of("schema-apply");
    assert_eq!(applies.len(), 2, "{applies:#?}");
    assert_ne!(
        applies[0]["metadata"]["name"],
        applies[1]["metadata"]["name"]
    );
}

/// Have the schema apply refuse a setting, and check that no step is planned for it and the status
/// names it
///
/// # Arguments
///
/// * `setting` - The refused setting as a config path
async fn refused_setting_plans_no_step(setting: &str) {
    // an apply that refuses the setting
    let cluster = chart_cluster(json!({"approvals": []}));
    let sim = Sim::new(&cluster);
    let mut state = UpgradeState::new(rev("2026-10-v02"), "1.8.1");
    state
        .schema
        .insert(RECORD_MARKER.to_owned(), "scylla".to_owned());
    sim.set_state(&state);
    let run = async |cluster: &ThoriumCluster| {
        let client = sim.fake().client();
        let mut cluster = cluster.clone();
        cluster.status = Some(sim.status());
        let meta = meta_for(cluster, &client);
        super::schema_apply::ensure_at(&meta, Some("fp"))
            .await
            .expect("ensure")
    };
    run(&cluster).await;
    let apply = sim.jobs("schema-apply").remove(0);
    sim.finish(
        &apply,
        &Finish::Failed {
            reason: "PodFailurePolicy".to_owned(),
            summary: Some(json!({
                "outcome": "failed_systematic",
                "exit_code": 1,
                "refused": [setting],
            })),
            log_lines: 1,
        },
    );
    // the refusal is read, and no check, migration, or pause follows
    for _ in 0..3 {
        run(&cluster).await;
    }
    assert_eq!(sim.created_of("migration-check").len(), 0);
    assert_eq!(sim.created_of("migration").len(), 0);
    assert_eq!(sim.scaled().len(), 0);
    // the status names the setting
    let message = sim.status().message.expect("a message");
    assert!(message.contains(setting), "{message}");
}

/// A refused files password plans no step, and the status names the setting
#[tokio::test]
async fn a_refused_password_plans_no_step() {
    // the password is hashed into the record and never moved by a step
    Box::pin(refused_setting_plans_no_step("thorium.files.password")).await;
}

/// A refused `earliest` value plans no step, and the status names the setting
#[tokio::test]
async fn a_refused_earliest_plans_no_step() {
    // no step moves the data of a refused earliest value
    Box::pin(refused_setting_plans_no_step("thorium.files.earliest")).await;
}

/// The approval-required message and the `MigrationIssues` condition list every group of the check or
/// the run, the largest first, with the objects they hold in total
#[tokio::test]
async fn several_groups_are_listed_largest_first() {
    // a check that found three groups, and a run that ends with two of them
    let scene = Scene::new(ONE, &json!([]), &[]);
    let reasons = [("bad_type:a", 2), ("bad_type:b", 9), ("bad_type:c", 5)];
    scene
        .settle_checks(&check_report("users-v2", 3, &reasons, None), 3)
        .await;
    // the approval message names every group, the largest first
    let message = scene.step("users-v2").message.expect("the message");
    let at = |code: &str| {
        message
            .find(code)
            .unwrap_or_else(|| panic!("{code} in {message}"))
    };
    assert!(
        at("bad_type:b") < at("bad_type:c") && at("bad_type:c") < at("bad_type:a"),
        "{message}"
    );
    // the run's condition names both groups and the sum of their objects
    let mut scene = scene;
    scene.approve(&approved());
    scene.rounds(3).await;
    let job = scene.sim.jobs("migration").remove(0);
    scene.sim.finish(
        &job,
        &Finish::Complete(run_summary(
            "done_with_issues",
            3,
            &[("bad_type:a", 2), ("bad_type:b", 9)],
        )),
    );
    scene.rounds(2).await;
    let condition = issues_condition(&scene);
    assert_eq!(condition.status, "True", "{}", condition.message);
    let message = condition.message;
    assert!(message.contains("11 objects in 2 groups"), "{message}");
    assert!(
        message.find("bad_type:b") < message.find("bad_type:a") && message.contains("bad_type:a"),
        "{message}"
    );
}

/// Run a writing Job of a kind in the namespace and check that a check Job still runs beside it
/// while no migration Job starts for an approved step
///
/// # Arguments
///
/// * `kind` - The writing Job's kind label
async fn a_check_runs_beside_a_writer_of(kind: &str) {
    // an approved step and a writing Job that is already running
    let scene = Scene::new(ONE, &approved(), &[]);
    let writer = json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {
            "name": format!("{kind}-other-aaaaaaaaaaaa"),
            "namespace": "thorium",
            "labels": {
                "thorium.sandia.gov/job-kind": kind,
                "thorium.sandia.gov/step": "other",
            },
        },
        "spec": {"template": {"spec": {"containers": []}}},
    });
    post_job(&scene, &writer).await;
    // the step's read-only check runs, and ends with its report
    scene.rounds(2).await;
    assert_eq!(scene.sim.created_of("migration-check").len(), 1);
    scene.finish_checks(&check_report("users-v2", 3, &[], None));
    scene.rounds(4).await;
    // the step's migration Job waits for the writer, which is itself the one created Job when it is
    // of the migration kind, and starts once the writer is gone
    let writers = usize::from(kind == "migration");
    assert_eq!(migrations(&scene).len(), writers, "{kind}");
    let message = scene.step("users-v2").message.expect("message");
    assert!(
        message.contains(&format!("writing Job {kind}-other")),
        "{message}"
    );
    scene.sim.reap(&format!("{kind}-other-aaaaaaaaaaaa"));
    scene.rounds(3).await;
    assert_eq!(migrations(&scene).len(), writers + 1, "{kind}");
}

/// A running schema-apply, migration, or record Job holds back a migration Job but not a check Job
#[tokio::test]
async fn a_check_job_runs_beside_every_kind_of_writer() {
    // each writing kind in turn
    for kind in ["schema-apply", "migration", "record"] {
        a_check_runs_beside_a_writer_of(kind).await;
    }
}

/// A writing Job deleted while its pod still shuts down holds back a new writing Job until the pod is
/// gone, though no Job of its kind is listed any more
#[tokio::test]
async fn a_pod_without_its_job_still_counts_as_a_writer() {
    // an approved step whose check is done, and a writing Job with a running pod
    let scene = Scene::new(ONE, &approved(), &[]);
    let writer = json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {
            "name": "schema-apply-other-aaaaaaaaaaaa",
            "namespace": "thorium",
            "labels": {
                "thorium.sandia.gov/job-kind": "schema-apply",
                "thorium.sandia.gov/step": "other",
            },
        },
        "spec": {"template": {"spec": {"containers": []}}},
    });
    post_job(&scene, &writer).await;
    scene.rounds(2).await;
    scene.finish_checks(&check_report("users-v2", 3, &[], None));
    scene.rounds(3).await;
    assert_eq!(migrations(&scene).len(), 0);
    // the Job object is gone and its pod is not
    scene.sim.orphan("schema-apply-other-aaaaaaaaaaaa");
    assert_eq!(scene.sim.jobs("schema-apply"), Vec::<String>::new());
    scene.rounds(3).await;
    assert_eq!(migrations(&scene).len(), 0);
    // once the pod is gone the step's Job starts
    scene.sim.reap("schema-apply-other-aaaaaaaaaaaa");
    scene.rounds(3).await;
    assert_eq!(migrations(&scene).len(), 1);
}

/// A step that failed and is approved again with a new backup runs a new Job, and one that finds
/// every worker at the end of its slice ends `Done` with the step's outcome recorded once and no
/// `Skipped` entry for work the first Job did
#[tokio::test]
async fn a_reapproved_step_that_resumes_to_the_end_is_recorded_done_once() {
    // a step whose first Job failed on a group
    let mut scene = Scene::new(ONE, &approved(), &[]);
    let report = check_report("users-v2", 3, &[], None);
    scene.settle_checks(&report, 3).await;
    let first = scene.sim.jobs("migration").remove(0);
    scene.sim.finish(
        &first,
        &Finish::Failed {
            reason: "PodFailurePolicy".to_owned(),
            summary: Some(run_summary("failed_issues", 0, &[("bad_type:x", 9)])),
            log_lines: 1,
        },
    );
    scene.rounds(3).await;
    // re-approved before anything finishes the step by hand, a new Job resumes and ends the pass
    scene.approve(&json!([{"step": "users-v2", "backup": "snap-2"}]));
    scene.rounds(3).await;
    assert_eq!(migrations(&scene).len(), 2);
    let second = scene.sim.jobs("migration").pop().expect("the new Job");
    let mut summary = run_summary("done", 0, &[]);
    summary["resumed"] = json!(true);
    scene.sim.finish(&second, &Finish::Complete(summary));
    scene.rounds(3).await;
    // exactly one finishing entry, and it says done
    let finishing = scene
        .history("users-v2")
        .into_iter()
        .filter(|entry| entry.outcome.finishes())
        .map(|entry| entry.outcome)
        .collect::<Vec<_>>();
    assert_eq!(finishing, vec![StepState::Done]);
}

/// A step held failed reads the record again within a minute, so the entry of a `--reason` run by
/// hand is seen soon: the record's read marker is moved to lapse then
#[tokio::test]
async fn a_failed_step_has_the_record_read_again_soon() {
    // a failed step, in a state whose record was read just now
    let scene = Scene::new(ONE, &approved(), &[]);
    let report = check_report("users-v2", 3, &[], None);
    scene.settle_checks(&report, 3).await;
    let mut state = scene.sim.state().expect("state");
    state
        .schema
        .insert("record_read".to_owned(), Utc::now().to_rfc3339());
    scene.sim.set_state(&state);
    let job = scene.sim.jobs("migration").remove(0);
    scene.sim.finish(
        &job,
        &Finish::Failed {
            reason: "PodFailurePolicy".to_owned(),
            summary: Some(run_summary("failed_issues", 0, &[("bad_type:x", 9)])),
            log_lines: 1,
        },
    );
    scene.rounds(2).await;
    assert_eq!(scene.step("users-v2").state, StepState::Failed);
    // the read now lapses within a minute instead of after a quarter of an hour
    let marker = scene.sim.state().expect("state").schema["record_read"].clone();
    let at = DateTime::parse_from_rfc3339(&marker)
        .expect("a time")
        .with_timezone(&Utc);
    let age = (Utc::now() - at).num_seconds();
    assert!((830..=850).contains(&age), "{age}");
}

/// One gated step on the way up and the step of the edge back down that reverses it
const DOWN: Catalog = Catalog {
    edges: &[
        edge(None, "2026-10-v01", &[]),
        edge(
            Some("2026-10-v01"),
            "2026-10-v02",
            &[gated("users-v2", &[], &[])],
        ),
        edge(
            Some("2026-10-v02"),
            "2026-10-v01",
            &[StepDef {
                reverses: Some("users-v2"),
                ..gated("users-v2-reverse", &[], &[])
            }],
        ),
    ],
    floors: &[],
};

/// With the image's thoradm knowing another catalog, a reverse step creates no check or migration
/// Job either, and the step waits with the catalogs named; with the catalogs agreeing it is checked
#[tokio::test]
async fn catalog_skew_blocks_a_reverse_step_too() {
    // a deployment at the second revision whose forward step finished, downgrading to the first
    CATALOG_OVERRIDE.with(|found| *found.borrow_mut() = Some(DOWN));
    let cluster = chart_cluster(json!({
        "target_revision": "2026-10-v01",
        "approvals": [{"step": "users-v2-reverse", "backup": "snap-1"}],
    }));
    let sim = Sim::new(&cluster);
    let mut state = UpgradeState::new(rev("2026-10-v02"), "1.8.1");
    state
        .schema
        .insert(RECORD_MARKER.to_owned(), "scylla".to_owned());
    state.record(AppliedStep {
        revision: "2026-10-v02".to_owned(),
        step: "users-v2".to_owned(),
        outcome: StepState::Done,
        at: Some(Utc::now().to_rfc3339()),
        ..AppliedStep::default()
    });
    // the last status Job reported another catalog
    let theirs = thorium::models::schema::CatalogIdentity {
        latest: rev("2026-10-v09"),
        hash: serde_json::from_value(json!("feedface")).expect("a fingerprint"),
    };
    let mut skewed = state.clone();
    skewed.schema.insert(
        migration::CATALOG_MARKER.to_owned(),
        serde_json::to_string(&theirs).expect("an identity"),
    );
    sim.set_state(&skewed);
    sim.auto_complete("schema-status", json!({"outcome": "done"}));
    let scene = Scene {
        sim,
        cluster,
        _guard: Guard,
    };
    // no Job of any data kind is created, and the step's message names both catalogs
    scene.rounds(4).await;
    assert_eq!(scene.sim.created_of("migration-check").len(), 0);
    assert_eq!(migrations(&scene).len(), 0);
    let message = scene.step("users-v2-reverse").message.expect("a message");
    assert!(message.contains("2026-10-v09"), "{message}");
    assert!(message.contains("feedface"), "{message}");
    // the same plan with the catalogs agreeing is checked, so the skew is what held it
    scene.sim.set_state(&state);
    scene.rounds(2).await;
    assert_eq!(scene.sim.created_of("migration-check").len(), 1);
}

/// Tests of the Jobs a hold stops or records while a step's run is open
#[path = "stop_tests.rs"]
mod stop_tests;

/// Tests of the derived Elastic rebuild, which run on this file's scene
#[path = "reingest_tests.rs"]
mod reingest_tests;

/// Tests of a step's approval, check, and issues across repeated runs and several edges
#[path = "rerun_tests.rs"]
mod rerun_tests;

/// Build a catalog of the production drain and image conversion steps, from the first revision to
/// the second
fn agentic_catalog() -> Catalog {
    // copy the production steps, which are plain values, into a catalog that lives for the test
    let steps = ["drain-reactions", "images-to-tools"]
        .into_iter()
        .map(|id| {
            *thorium::models::upgrades::step(id)
                .expect("a production step")
                .1
        })
        .collect::<Vec<_>>();
    let steps: &'static [StepDef] = Box::leak(steps.into_boxed_slice());
    let edges = vec![
        edge(None, "2026-10-v01", &[]),
        edge(Some("2026-10-v01"), "2026-10-v02", steps),
    ];
    Catalog {
        edges: Box::leak(edges.into_boxed_slice()),
        floors: &[],
    }
}

/// Run reconciles, ending each check Job with the report its step's migration leaves
///
/// # Arguments
///
/// * `scene` - The scene
/// * `drain` - The report the drain's check leaves
/// * `images` - The report the image conversion's check leaves
/// * `rounds` - How many reconciles to run
async fn settle_agentic(scene: &Scene, drain: &Value, images: &Value, rounds: usize) {
    // a reconcile creates or reads a Job, and the test ends the Jobs between them
    for _ in 0..rounds {
        scene.reconcile().await;
        for job in scene.sim.active("migration-check") {
            let report = if job.contains("drain-reactions") {
                drain
            } else {
                images
            };
            scene.sim.leave_result(&job, report.clone());
            scene.sim.finish(
                &job,
                &Finish::Complete(json!({"outcome": "done", "result_id": format!("uid-{job}")})),
            );
        }
    }
    scene.reconcile().await;
}

/// The drain only checks: while its check waits the step is `Waiting` with the reason, no approval
/// is asked, no migration Job runs, and no component is paused; once nothing is in flight the step
/// is skipped with no migration Job and no pause
#[tokio::test]
async fn the_drain_waits_before_anything_is_paused_and_skips_when_nothing_is_in_flight() {
    // a cluster with every component the conversions pause
    let start = Utc::now();
    set_now(start);
    let components = [
        ("api", 3),
        ("scaler", 1),
        ("baremetal-scaler", 1),
        ("event-handler", 1),
        ("search-streamer", 1),
    ];
    let scene = Scene::new(agentic_catalog(), &json!([]), &components);
    // the drain's check asks to wait and names a count, and the images have nothing to convert
    let mut waiting = check_report("drain-reactions", 0, &[], Some(15));
    waiting["wait"]["reason"] = json!("2 reaction(s) are still Created or Started");
    let images = check_report("images-to-tools", 0, &[], None);
    settle_agentic(&scene, &waiting, &images, 3).await;
    // the check is a read-only check Job of the drain
    let checks = scene.sim.created_of("migration-check");
    let drain_check = checks
        .iter()
        .find(|job| Scene::args(job).get(3).map(String::as_str) == Some("drain-reactions"))
        .expect("a check Job for the drain");
    assert_eq!(&Scene::args(drain_check)[..3], ["migrate", "check", "--id"]);
    // the step waits with the reason and asks for no approval
    let step = scene.step("drain-reactions");
    assert_eq!(step.state, StepState::Waiting, "{step:?}");
    assert_eq!(
        step.message.as_deref(),
        Some("2 reaction(s) are still Created or Started")
    );
    // no migration Job ran and every component keeps its replicas
    assert_eq!(migrations(&scene).len(), 0);
    for (name, replicas) in components {
        assert_eq!(scene.sim.replicas(name), Some(replicas), "{name}");
    }
    assert_eq!(scene.sim.scaled(), Vec::<(String, i32)>::new());
    // the wait ends once nothing is in flight: the step is skipped, with nothing paused or run
    set_now(start + Duration::seconds(60));
    let clear = check_report("drain-reactions", 0, &[], None);
    settle_agentic(&scene, &clear, &images, 4).await;
    let entries = scene.history("drain-reactions");
    assert_eq!(
        entries.last().map(|entry| entry.outcome),
        Some(StepState::Skipped),
        "{entries:?}"
    );
    // the skip says the check found nothing for the step, not that objects were converted
    let why = entries.last().and_then(|entry| entry.message.clone());
    assert!(
        why.as_deref()
            .is_some_and(|why| why.contains("the check read needs this step")),
        "{why:?}"
    );
    assert_eq!(migrations(&scene).len(), 0);
    assert_eq!(scene.sim.scaled(), Vec::<(String, i32)>::new());
}
