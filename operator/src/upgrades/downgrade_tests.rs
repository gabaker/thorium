//! Tests of downgrades through the operator: the reverse edge of a partial upgrade, a dual write's
//! reverse, a lossy step's loss report, confirmation code, and approvals, the dev setting, and the
//! steps whose history holds a value this build doesn't know, and handing the cluster over to the
//! previous operator when the downgrade ends, all against a stateful fake of the Kubernetes API

use kube::runtime::controller::Action;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use thorium::Error;
use thorium::models::schema::{CheckReport, PassCounts};
use thorium::models::upgrades::{
    AppliedStep, BlockReason, CONSUMED_REFUSAL, Catalog, CatalogEntry, ConfirmRefusal, Fill,
    FillValue, MigrationPhase, PlannedStep, RECORD_MARKER, Reversibility, StepDef, StepKind,
    StepLoss, StepState, UpgradeComponent, UpgradeConditionType, UpgradeState,
};

use super::n1;
use super::pause;
use super::record::Moved;
use super::sim::{Finish, Sim};
use super::tests::{chart_cluster, rev, tracked};
use super::{CATALOG_OVERRIDE, Gate, OPERATOR_VERSION, Pending, RecordOps, gate, run_with};
use crate::k8s::clusters::{ClusterMeta, tests::meta_for};
use crate::k8s::crds::ThoriumCluster;

/// What a fixture step tells an admin to do by hand
const PROCEDURE: &str = "Run `thoradm migrate run --id <id>` by hand.";

/// The reversibility of the lossy fixture steps: a contact and a phone number are lost
const LOSSY_REVERT: Reversibility = Reversibility::LossyRevert {
    fills: &[Fill {
        field: "contact",
        value: FillValue::Sentinel("none"),
    }],
    lost_fields: &["contact", "phone"],
    enum_mappings: &[],
    privilege_orders: &[],
    description: "Users keep only their first contact",
};

/// Build a data step a thoradm migration performs
///
/// # Arguments
///
/// * `id` - The step's id, which is also its migration's
/// * `reversibility` - How it is undone, when it can be
const fn forward(id: &'static str, reversibility: Option<Reversibility>) -> StepDef {
    // a data step with a migration and a procedure
    StepDef {
        id,
        kind: StepKind::Data,
        description: "A fixture migration",
        thoradm_migration: Some(id),
        manual_procedure: Some(PROCEDURE),
        phase: Some(MigrationPhase::Migrate),
        reversibility,
        ..StepDef::BASE
    }
}

/// Build the step of a reverse edge that undoes a forward step
///
/// # Arguments
///
/// * `id` - The step's id, which is also its migration's
/// * `reverses` - The forward step it undoes
const fn reverse(id: &'static str, reverses: &'static str) -> StepDef {
    // a data step of an edge leading down
    StepDef {
        reverses: Some(reverses),
        ..forward(id, None)
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
    // a release or reverse edge with no snapshot
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

/// The first reverse step, which loses data
const LOSSY_REVERSE: &str = "users-v2-reverse";

/// One lossy step and its reverse
const LOSSY: Catalog = Catalog {
    edges: &[
        edge(None, "2026-10-v01", &[]),
        edge(
            Some("2026-10-v01"),
            "2026-10-v02",
            &[forward("users-v2", Some(LOSSY_REVERT))],
        ),
        edge(
            Some("2026-10-v02"),
            "2026-10-v01",
            &[reverse(LOSSY_REVERSE, "users-v2")],
        ),
    ],
    floors: &[],
};

/// A lossless step and a lossy one, each with a reverse, the lossless one's running first
const MIXED: Catalog = Catalog {
    edges: &[
        edge(None, "2026-10-v01", &[]),
        edge(
            Some("2026-10-v01"),
            "2026-10-v02",
            &[
                forward("tags-v2", Some(Reversibility::Backfill)),
                forward("users-v2", Some(LOSSY_REVERT)),
            ],
        ),
        edge(
            Some("2026-10-v02"),
            "2026-10-v01",
            &[
                reverse("tags-v2-reverse", "tags-v2"),
                reverse(LOSSY_REVERSE, "users-v2"),
            ],
        ),
    ],
    floors: &[],
};

/// Two lossy steps on two edges, with a reverse edge each
const TWO_LOSSY: Catalog = Catalog {
    edges: &[
        edge(None, "2026-10-v01", &[]),
        edge(
            Some("2026-10-v01"),
            "2026-10-v02",
            &[forward("users-v2", Some(LOSSY_REVERT))],
        ),
        edge(
            Some("2026-10-v02"),
            "2026-10-v03",
            &[forward("files-v3", Some(LOSSY_REVERT))],
        ),
        edge(
            Some("2026-10-v03"),
            "2026-10-v02",
            &[reverse("files-v3-reverse", "files-v3")],
        ),
        edge(
            Some("2026-10-v02"),
            "2026-10-v01",
            &[reverse(LOSSY_REVERSE, "users-v2")],
        ),
    ],
    floors: &[],
};

/// Three lossless steps on one edge, whose reverse edge lists them newest first
const PARTIAL: Catalog = Catalog {
    edges: &[
        edge(None, "2026-10-v01", &[]),
        edge(
            Some("2026-10-v01"),
            "2026-10-v02",
            &[
                forward("a-v2", Some(Reversibility::Backfill)),
                forward("b-v2", Some(Reversibility::Backfill)),
                forward("c-v2", Some(Reversibility::Backfill)),
            ],
        ),
        edge(
            Some("2026-10-v02"),
            "2026-10-v01",
            &[
                reverse("c-v2-reverse", "c-v2"),
                reverse("b-v2-reverse", "b-v2"),
                reverse("a-v2-reverse", "a-v2"),
            ],
        ),
    ],
    floors: &[],
};

/// A step whose revision writes both forms of its data, and its reverse
const DUAL: Catalog = Catalog {
    edges: &[
        edge(None, "2026-10-v01", &[]),
        edge(
            Some("2026-10-v01"),
            "2026-10-v02",
            &[forward(
                "users-dw",
                Some(Reversibility::DualWrite {
                    until: "2026-12-v01",
                }),
            )],
        ),
        edge(
            Some("2026-10-v02"),
            "2026-10-v01",
            &[reverse("users-dw-reverse", "users-dw")],
        ),
    ],
    floors: &[],
};

/// The record moved locally over the fixture catalog, so these tests need no status Job to plan or
/// finish a downgrade
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
        _meta: &ClusterMeta,
        _pending: &mut Pending,
    ) -> impl std::future::Future<Output = Result<Option<Action>, Error>> {
        // the state is the record
        std::future::ready(Ok(None))
    }

    /// Advance the state's revision to the target along the fixture catalog's edges
    ///
    /// # Arguments
    ///
    /// * `meta` - Thorium cluster client and metadata
    /// * `pending` - The upgrade whose state is advanced and saved
    /// * `revision` - The revision to record
    async fn move_revision(
        &self,
        meta: &ClusterMeta,
        pending: &mut Pending,
        revision: &thorium::models::upgrades::RevisionId,
    ) -> Result<Moved, Error> {
        // a state already at the revision has nothing to move
        if pending.stored.state.revision == *revision {
            return Ok(Moved::Done);
        }
        super::catalog().advance(&mut pending.stored.state, &pending.target, OPERATOR_VERSION);
        super::state::save(&meta.client, &meta.namespace, &mut pending.stored).await?;
        Ok(Moved::Done)
    }
}

/// Puts the catalog back when a test ends
struct Guard;

impl Drop for Guard {
    /// Remove the catalog a test injected
    fn drop(&mut self) {
        // the thread may run another test
        CATALOG_OVERRIDE.with(|catalog| *catalog.borrow_mut() = None);
    }
}

/// Build a history entry
///
/// # Arguments
///
/// * `revision` - The revision the step belongs to
/// * `step` - The step
/// * `outcome` - Its outcome
/// * `confirm` - The code it records
fn entry(revision: &str, step: &str, outcome: StepState, confirm: Option<&str>) -> AppliedStep {
    // an entry recorded now
    AppliedStep {
        revision: revision.to_owned(),
        step: step.to_owned(),
        outcome,
        confirm: confirm.map(ToOwned::to_owned),
        at: Some(chrono::Utc::now().to_rfc3339()),
        by: Some("test".to_owned()),
        ..AppliedStep::default()
    }
}

/// Build the state of a deployment that finished some forward steps
///
/// # Arguments
///
/// * `revision` - The revision it is at
/// * `finished` - The forward steps it finished, as `(revision, step)`
fn deployed(revision: &str, finished: &[(&str, &str)]) -> UpgradeState {
    // mirroring a record, with each forward step done
    let mut state = UpgradeState::new(rev(revision), "1.8.1");
    state
        .schema
        .insert(RECORD_MARKER.to_owned(), "scylla".to_owned());
    for (edge, step) in finished {
        state.applied.push(entry(edge, step, StepState::Done, None));
    }
    state
}

/// A cluster, its fake kube API, and the downgrade the tests run
struct Scene {
    /// The fake kube API
    sim: Sim,
    /// Puts the catalog back at the end
    _guard: Guard,
}

impl Scene {
    /// Build a scene downgrading a deployment to an older revision
    ///
    /// # Arguments
    ///
    /// * `catalog` - The fixture catalog
    /// * `state` - The deployment's recorded state
    /// * `upgrade` - The cluster's `spec.upgrade`
    /// * `deployments` - The Deployments that exist, with their replicas
    fn new(
        catalog: Catalog,
        state: &UpgradeState,
        upgrade: &Value,
        deployments: &[(&str, i32)],
    ) -> Scene {
        // plan with the fixture catalog on this thread
        CATALOG_OVERRIDE.with(|found| *found.borrow_mut() = Some(catalog));
        let cluster = chart_cluster(upgrade.clone());
        let sim = Sim::new(&cluster);
        sim.set_state(state);
        for (name, replicas) in deployments {
            sim.add_deployment(name, *replicas);
        }
        // status Jobs read the record and finish at once
        sim.auto_complete("schema-status", json!({"outcome": "done"}));
        Scene { sim, _guard: Guard }
    }

    /// Build a scene that downgrades to the first revision with some approvals
    ///
    /// # Arguments
    ///
    /// * `catalog` - The fixture catalog
    /// * `state` - The deployment's recorded state
    /// * `approvals` - The approvals the cluster starts with
    fn to_v01(catalog: Catalog, state: &UpgradeState, approvals: &Value) -> Scene {
        // an admin targets the first revision
        Scene::new(
            catalog,
            state,
            &json!({"target_revision": "2026-10-v01", "approvals": approvals}),
            &[("api", 3), ("scaler", 1)],
        )
    }

    /// Replace the approvals on the cluster
    ///
    /// # Arguments
    ///
    /// * `approvals` - The approvals
    fn approve(&self, approvals: &Value) {
        // keep the rest of the spec the cluster has
        let mut cluster = self.sim.cluster();
        let mut raw = serde_json::to_value(&cluster).expect("cluster serializes");
        raw["spec"]["upgrade"]["approvals"] = approvals.clone();
        cluster = serde_json::from_value(raw).expect("cluster parses");
        self.sim.set_cluster(&cluster);
    }

    /// Set an annotation on the cluster
    ///
    /// # Arguments
    ///
    /// * `key` - The annotation's key
    /// * `value` - Its value
    fn annotate(&self, key: &str, value: &str) {
        // add it to the cluster's metadata
        let mut cluster = self.sim.cluster();
        cluster
            .metadata
            .annotations
            .get_or_insert_with(BTreeMap::new)
            .insert(key.to_owned(), value.to_owned());
        self.sim.set_cluster(&cluster);
    }

    /// Run one reconcile of the upgrade
    async fn reconcile(&self) {
        // gate the cluster with the status the last reconcile left
        let mut cluster: ThoriumCluster = self.sim.cluster();
        cluster.status = Some(self.sim.status());
        let client = self.sim.fake().client();
        // a cluster with nothing to run has nothing to do
        let Ok(Gate::Upgrade(pending)) = gate(&client, &tracked(&cluster)).await else {
            return;
        };
        let meta = meta_for(cluster, &client);
        Box::pin(run_with(&meta, pending, &Local))
            .await
            .expect("run");
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

    /// Run reconciles, ending each check Job with the report, until every check is cached
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

    /// Get the code a blocked step shows
    ///
    /// # Arguments
    ///
    /// * `id` - The step's id
    fn code(&self, id: &str) -> String {
        // every blocked reason that carries a code carries the current one
        match self.step(id).blocked {
            Some(
                BlockReason::LossAcceptance { confirm }
                | BlockReason::ConfirmationRefused { confirm, .. },
            ) => confirm,
            other => panic!("step {id} shows no code: {other:?}"),
        }
    }

    /// Get the migration Jobs created, as posted
    fn migrations(&self) -> Vec<Value> {
        // the creates of the migration kind
        self.sim.created_of("migration")
    }

    /// Get the value after a flag in a Job's arguments
    ///
    /// # Arguments
    ///
    /// * `job` - The Job as posted
    /// * `flag` - The flag
    fn flag(job: &Value, flag: &str) -> Option<String> {
        // the arguments after the config path thoradm is started with
        let args = job["spec"]["template"]["spec"]["containers"][0]["args"]
            .as_array()
            .expect("arguments")
            .iter()
            .filter_map(|arg| arg.as_str().map(ToOwned::to_owned))
            .skip(2)
            .collect::<Vec<_>>();
        let at = args.iter().position(|arg| arg == flag)?;
        args.get(at + 1).cloned()
    }
}

/// Build a check report that estimates a loss
///
/// # Arguments
///
/// * `migratable` - How many objects need migrating
/// * `fields` - The fields lost as `(name, objects, covered by backups)`
fn report(migratable: u64, fields: &[(&str, u64, bool)]) -> Value {
    // ten objects read, the rest current, and the loss the reverse would cause
    let loss = StepLoss {
        revision: "2026-10-v01".to_owned(),
        step: LOSSY_REVERSE.to_owned(),
        description: "Users keep only their first contact".to_owned(),
        fields: fields
            .iter()
            .map(
                |(field, objects, backed_up)| thorium::models::upgrades::FieldLoss {
                    field: (*field).to_owned(),
                    objects: *objects,
                    backed_up: *backed_up,
                },
            )
            .collect(),
    };
    let report = CheckReport {
        migration: LOSSY_REVERSE.to_owned(),
        counts: PassCounts {
            read: migratable.max(10),
            current: migratable.max(10) - migratable,
            migratable,
            ..PassCounts::default()
        },
        families: vec!["users".to_owned()],
        rows: BTreeMap::from([("users".to_owned(), migratable)]),
        estimated_seconds: Some(30),
        reasons: Vec::new(),
        more: 0,
        wait: None,
        summary: None,
        loss: Some(loss),
    };
    serde_json::to_value(report).expect("a report serializes")
}

/// The report most tests use: three users lose two fields, both covered by backups
fn covered() -> Value {
    // contact and phone are both backed up
    report(3, &[("contact", 3, true), ("phone", 2, true)])
}

/// The state of a deployment that finished the lossy forward step
fn at_v02() -> UpgradeState {
    // the forward step is done at the second revision
    deployed("2026-10-v02", &[("2026-10-v02", "users-v2")])
}

/// An approval that names a backup, accepts the loss, and repeats a code
///
/// # Arguments
///
/// * `step` - The step
/// * `confirm` - The code it repeats
fn accepting(step: &str, confirm: &str) -> Value {
    // every field a lossy approval carries
    json!({"step": step, "backup": "snap-1", "accept_loss": true, "confirm": confirm})
}

/// A lossy step is checked, then blocks with a confirmation code and its loss report in the status
/// and the history, before anything is approved
#[tokio::test]
async fn a_lossy_step_shows_its_loss_and_code_before_any_approval() {
    // a step whose check finds three users to rewrite
    let scene = Scene::to_v01(LOSSY, &at_v02(), &json!([]));
    scene.settle_checks(&covered(), 3).await;
    // it blocks for its loss to be accepted, with an eight character code
    let step = scene.step(LOSSY_REVERSE);
    assert_eq!(step.state, StepState::Blocked, "{step:?}");
    let code = scene.code(LOSSY_REVERSE);
    assert_eq!(code.len(), 8, "{code}");
    assert!(matches!(
        step.blocked,
        Some(BlockReason::LossAcceptance { .. })
    ));
    // the estimate, with its counts and backup coverage, is in the status before any approval
    let upgrade = scene.sim.status().upgrade.expect("an upgrade status");
    let loss = upgrade.loss.expect("a loss report");
    assert_eq!(loss.steps.len(), 1);
    let fields = &loss.steps[0].fields;
    assert_eq!(
        (
            fields[0].field.as_str(),
            fields[0].objects,
            fields[0].backed_up
        ),
        ("contact", 3, true)
    );
    assert_eq!((fields[1].field.as_str(), fields[1].objects), ("phone", 2));
    // the history records the block with the code and the loss
    let blocked = scene.history(LOSSY_REVERSE);
    assert_eq!(blocked.len(), 1, "{blocked:?}");
    assert_eq!(blocked[0].outcome, StepState::Blocked);
    assert_eq!(blocked[0].confirm.as_deref(), Some(code.as_str()));
    assert_eq!(
        blocked[0].loss.as_ref().map(|loss| loss.fields[0].objects),
        Some(3)
    );
    // no migration Job exists
    assert_eq!(scene.migrations().len(), 0);
}

/// A lossy step runs only with an approval that has a backup, `accept_loss`, and the code; missing
/// any of them keeps it blocked and the status says which
#[tokio::test]
async fn a_lossy_step_runs_only_with_a_complete_approval() {
    let scene = Scene::to_v01(LOSSY, &at_v02(), &json!([]));
    scene.settle_checks(&covered(), 3).await;
    let code = scene.code(LOSSY_REVERSE);
    // a backup alone doesn't accept the loss
    scene.approve(&json!([{"step": LOSSY_REVERSE, "backup": "snap-1"}]));
    scene.settle_checks(&covered(), 3).await;
    assert!(matches!(
        scene.step(LOSSY_REVERSE).blocked,
        Some(BlockReason::LossAcceptance { .. })
    ));
    // accepting without the code is refused as missing, and the current code is shown
    scene.approve(&json!([{"step": LOSSY_REVERSE, "backup": "snap-1", "accept_loss": true}]));
    scene.settle_checks(&covered(), 3).await;
    assert_eq!(
        scene.step(LOSSY_REVERSE).blocked,
        Some(BlockReason::ConfirmationRefused {
            confirm: code.clone(),
            refusal: ConfirmRefusal::Missing,
        })
    );
    // a wrong code is a mismatch
    scene.approve(&json!([accepting(LOSSY_REVERSE, "AAAAAAAA")]));
    scene.settle_checks(&covered(), 3).await;
    assert_eq!(
        scene.step(LOSSY_REVERSE).blocked,
        Some(BlockReason::ConfirmationRefused {
            confirm: code.clone(),
            refusal: ConfirmRefusal::Mismatch,
        })
    );
    // the right code with no backup still waits, and says it names none
    scene.approve(
        &json!([{"step": LOSSY_REVERSE, "backup": "", "accept_loss": true,
                           "confirm": code}]),
    );
    scene.settle_checks(&covered(), 3).await;
    assert!(
        scene
            .step(LOSSY_REVERSE)
            .message
            .expect("a message")
            .contains("names no backup")
    );
    assert_eq!(scene.migrations().len(), 0);
    // all three run it, with the backup and the code in its arguments and its history entry
    scene.approve(&json!([accepting(LOSSY_REVERSE, &code)]));
    scene.settle_checks(&covered(), 3).await;
    let created = scene.migrations();
    assert_eq!(created.len(), 1, "{created:#?}");
    assert_eq!(
        Scene::flag(&created[0], "--backup").as_deref(),
        Some("snap-1")
    );
    assert_eq!(
        Scene::flag(&created[0], "--confirm").as_deref(),
        Some(code.as_str())
    );
    let running = scene
        .history(LOSSY_REVERSE)
        .into_iter()
        .find(|entry| entry.outcome == StepState::Running)
        .expect("started");
    assert_eq!(running.confirm.as_deref(), Some(code.as_str()));
    assert_eq!(running.backup.as_deref(), Some("snap-1"));
    assert_eq!(running.loss.map(|loss| loss.fields[0].objects), Some(3));
}

/// A confirmed lossy step's history entry records the code, the backup, the loss counts, and the
/// outcome
#[tokio::test]
async fn a_confirmed_lossy_step_records_its_code_backup_loss_and_outcome() {
    let scene = Scene::to_v01(LOSSY, &at_v02(), &json!([]));
    scene.settle_checks(&covered(), 3).await;
    let code = scene.code(LOSSY_REVERSE);
    scene.approve(&json!([accepting(LOSSY_REVERSE, &code)]));
    scene.settle_checks(&covered(), 3).await;
    // the run finishes
    let job = scene.sim.jobs("migration").remove(0);
    scene.sim.finish(
        &job,
        &Finish::Complete(json!({
            "outcome": "done", "migration": LOSSY_REVERSE,
            "counts": PassCounts {read: 10, current: 7, migratable: 3, written: 3,
                                  ..PassCounts::default()},
            "groups": [],
        })),
    );
    scene.rounds(2).await;
    // its outcome entry carries what the approval and the check said
    let done = scene
        .history(LOSSY_REVERSE)
        .into_iter()
        .find(|entry| entry.outcome == StepState::Done)
        .expect("finished");
    assert_eq!(done.confirm.as_deref(), Some(code.as_str()));
    assert_eq!(done.backup.as_deref(), Some("snap-1"));
    let loss = done.loss.expect("the loss");
    assert_eq!(loss.fields[0].objects, 3);
    assert_eq!(loss.fields[1].objects, 2);
}

/// On a dev cluster with the setting, a lossy step runs without a code, an approval, or a backup,
/// and no `LossySettingRefused` condition is raised
#[tokio::test]
async fn a_dev_cluster_with_the_setting_runs_a_lossy_step_without_confirmation() {
    // autoTargetDev, the setting, and the Namespace label
    let scene = Scene::new(
        LOSSY,
        &at_v02(),
        &json!({"target_revision": "2026-10-v01", "auto_target_dev": true,
                "dangerous_downgrade_lossy": true}),
        &[("api", 3)],
    );
    scene
        .sim
        .set_namespace_labels(&[("thorium.sandia.gov/dev", "true")]);
    scene.settle_checks(&covered(), 3).await;
    // the Job runs with neither a backup nor a code
    let created = scene.migrations();
    assert_eq!(created.len(), 1, "{created:#?}");
    assert_eq!(Scene::flag(&created[0], "--backup"), None);
    assert_eq!(Scene::flag(&created[0], "--confirm"), None);
    // the history still records what it loses, and no block
    let entries = scene.history(LOSSY_REVERSE);
    assert!(
        entries
            .iter()
            .all(|entry| entry.outcome != StepState::Blocked)
    );
    let running = entries
        .iter()
        .find(|entry| entry.outcome == StepState::Running)
        .expect("started");
    assert_eq!(running.backup, None);
    assert_eq!(running.confirm, None);
    assert_eq!(
        running.loss.as_ref().map(|loss| loss.fields[0].objects),
        Some(3)
    );
    // nothing is refused
    let conditions = scene.sim.status().upgrade.expect("status").conditions;
    assert!(conditions.iter().all(|condition| condition.kind
        != UpgradeConditionType::LossySettingRefused
        || condition.status == "False"));
}

/// The setting on a cluster that isn't dev, with or without `autoTargetDev`, is refused and
/// reported; lossy steps still need their own confirmation and the rest of the plan runs
#[tokio::test]
async fn the_setting_on_a_cluster_that_isnt_dev_is_refused_and_changes_nothing_else() {
    for auto_target_dev in [false, true] {
        // a lossless step first, whose approval is set, then the lossy one with none; the
        // Namespace carries no label
        let scene = Scene::new(
            MIXED,
            &deployed(
                "2026-10-v02",
                &[("2026-10-v02", "tags-v2"), ("2026-10-v02", "users-v2")],
            ),
            &json!({"target_revision": "2026-10-v01", "auto_target_dev": auto_target_dev,
                    "dangerous_downgrade_lossy": true,
                    "approvals": [{"step": "tags-v2-reverse", "backup": "snap-1"}]}),
            &[("api", 3)],
        );
        scene.settle_checks(&covered(), 4).await;
        // the lossless step runs: its Job is created and finishes
        let tags = scene.sim.jobs("migration");
        assert_eq!(tags.len(), 1, "{tags:?}");
        scene.sim.finish(
            &tags[0],
            &Finish::Complete(json!({"outcome": "done", "migration": "tags-v2-reverse",
                "counts": PassCounts {read: 10, current: 7, migratable: 3, written: 3,
                                      ..PassCounts::default()},
                "groups": []})),
        );
        scene.settle_checks(&covered(), 4).await;
        // the lossy step is gated as usual, and the setting is reported as refused
        assert!(
            matches!(
                scene.step(LOSSY_REVERSE).blocked,
                Some(BlockReason::LossAcceptance { .. })
            ),
            "{:?}",
            scene.step(LOSSY_REVERSE)
        );
        assert_eq!(scene.migrations().len(), 1);
        let conditions = scene.sim.status().upgrade.expect("status").conditions;
        let refused = conditions
            .iter()
            .find(|condition| condition.kind == UpgradeConditionType::LossySettingRefused)
            .expect("the refusal is reported");
        assert_eq!(refused.status, "True");
        assert!(refused.message.contains("dangerous_downgrade_lossy"));
    }
}

/// A leftover approval whose code a run already used is refused as consumed, with a new code shown
#[tokio::test]
async fn a_leftover_approval_for_a_consumed_code_is_refused_as_consumed() {
    // the step ran under OLDCODE1, and the deployment was upgraded again since
    let mut state = deployed("2026-10-v02", &[("2026-10-v02", "users-v2")]);
    state.applied.push(entry(
        "2026-10-v01",
        LOSSY_REVERSE,
        StepState::Blocked,
        Some("OLDCODE1"),
    ));
    state.applied.push(entry(
        "2026-10-v01",
        LOSSY_REVERSE,
        StepState::Running,
        Some("OLDCODE1"),
    ));
    state.applied.push(entry(
        "2026-10-v01",
        LOSSY_REVERSE,
        StepState::Done,
        Some("OLDCODE1"),
    ));
    state
        .applied
        .push(entry("2026-10-v02", "users-v2", StepState::Done, None));
    let scene = Scene::to_v01(
        LOSSY,
        &state,
        &json!([accepting(LOSSY_REVERSE, "OLDCODE1")]),
    );
    scene.settle_checks(&covered(), 3).await;
    // the step blocks anew with a code of its own, and the old one is consumed
    let fresh = scene.code(LOSSY_REVERSE);
    assert_ne!(fresh, "OLDCODE1");
    assert_eq!(
        scene.step(LOSSY_REVERSE).blocked,
        Some(BlockReason::ConfirmationRefused {
            confirm: fresh,
            refusal: ConfirmRefusal::Consumed,
        })
    );
    assert_eq!(scene.migrations().len(), 0);
}

/// A code the history no longer holds is still refused: the migration Job finds its `consumed_code`
/// row and refuses to run, and the status says the code was consumed and shows a new one
#[tokio::test]
async fn a_job_that_refuses_a_consumed_code_blocks_the_step_with_a_new_one() {
    let scene = Scene::to_v01(LOSSY, &at_v02(), &json!([]));
    scene.settle_checks(&covered(), 3).await;
    let code = scene.code(LOSSY_REVERSE);
    // the approval repeats the current code, so the Job is created
    scene.approve(&json!([accepting(LOSSY_REVERSE, &code)]));
    scene.settle_checks(&covered(), 3).await;
    let job = scene.sim.jobs("migration").remove(0);
    // the Job finds the code consumed in Scylla, which the state's history lost
    scene.sim.finish(
        &job,
        &Finish::Failed {
            reason: "BackoffLimitExceeded".to_owned(),
            summary: Some(json!({
                "outcome": "failed_systematic",
                "error": format!("the confirmation code {code} {CONSUMED_REFUSAL}"),
            })),
            log_lines: 2,
        },
    );
    scene.rounds(2).await;
    // the step is held failed, shows the consumed refusal and a new code
    let step = scene.step(LOSSY_REVERSE);
    assert_eq!(step.state, StepState::Failed, "{step:?}");
    let fresh = scene.code(LOSSY_REVERSE);
    assert_ne!(fresh, code);
    assert!(matches!(
        step.blocked,
        Some(BlockReason::ConfirmationRefused {
            refusal: ConfirmRefusal::Consumed,
            ..
        })
    ));
    assert!(step.message.expect("message").contains(&fresh));
    // no second Job runs under the same approval
    scene.rounds(3).await;
    assert_eq!(scene.migrations().len(), 1);
    // an approval repeating the new code runs it
    scene.approve(&json!([accepting(LOSSY_REVERSE, &fresh)]));
    scene.settle_checks(&covered(), 3).await;
    assert_eq!(scene.migrations().len(), 2);
}

/// The code a blocked step shows doesn't change when only the object counts of its loss do
#[tokio::test]
async fn the_code_doesnt_change_when_only_the_counts_do() {
    let scene = Scene::to_v01(LOSSY, &at_v02(), &json!([]));
    scene.settle_checks(&covered(), 3).await;
    let code = scene.code(LOSSY_REVERSE);
    // live writes change the counts, and the check is run again
    scene.annotate("thorium.sandia.gov/refresh-check", "1");
    scene
        .settle_checks(
            &report(40, &[("contact", 40, true), ("phone", 22, true)]),
            3,
        )
        .await;
    assert_eq!(scene.code(LOSSY_REVERSE), code);
    let loss = scene.step(LOSSY_REVERSE).loss.expect("loss");
    assert_eq!(loss.fields[0].objects, 40);
    // one block, one entry
    assert_eq!(scene.history(LOSSY_REVERSE).len(), 1);
}

/// A field no backup covers keeps the step blocked under an approval with the right code, naming
/// the gap, and the step runs once the approval accepts it
#[tokio::test]
async fn an_uncovered_field_keeps_the_step_blocked_until_the_gap_is_accepted() {
    let uncovered = report(3, &[("contact", 3, true), ("phone", 2, false)]);
    let scene = Scene::to_v01(LOSSY, &at_v02(), &json!([]));
    scene.settle_checks(&uncovered, 3).await;
    let code = scene.code(LOSSY_REVERSE);
    // the status says which fields backups cover
    let loss = scene.step(LOSSY_REVERSE).loss.expect("loss");
    assert!(loss.fields[0].backed_up && !loss.fields[1].backed_up);
    // the right code, a backup, and accepted loss aren't enough
    scene.approve(&json!([accepting(LOSSY_REVERSE, &code)]));
    scene.settle_checks(&uncovered, 3).await;
    let step = scene.step(LOSSY_REVERSE);
    assert_eq!(
        step.blocked,
        Some(BlockReason::ConfirmationRefused {
            confirm: code.clone(),
            refusal: ConfirmRefusal::BackupGap,
        })
    );
    assert!(step.message.expect("message").contains("phone"));
    assert_eq!(scene.migrations().len(), 0);
    // accepting the gap runs it
    scene.approve(
        &json!([{"step": LOSSY_REVERSE, "backup": "snap-1", "accept_loss": true,
                           "confirm": code, "accept_backup_gaps": true}]),
    );
    scene.settle_checks(&uncovered, 3).await;
    assert_eq!(scene.migrations().len(), 1);
}

/// With two lossy steps on the path, an approval given ahead for the second, with the first's code,
/// doesn't match: the plan runs the first and stops at the second for its own code
#[tokio::test]
async fn an_approval_given_ahead_for_a_later_lossy_step_doesnt_match() {
    // both forward steps finished, and the target is the first revision
    let state = deployed(
        "2026-10-v03",
        &[("2026-10-v02", "users-v2"), ("2026-10-v03", "files-v3")],
    );
    let scene = Scene::to_v01(TWO_LOSSY, &state, &json!([]));
    // the checks carry no estimate, so the plan's names stand, with no backup coverage known
    let bare = {
        let mut value = report(3, &[]);
        value["loss"] = Value::Null;
        value
    };
    scene.settle_checks(&bare, 6).await;
    let first = scene.code("files-v3-reverse");
    // both steps are approved ahead, the second with the first's code copied
    let gaps = |step: &str, code: &str| {
        json!({"step": step, "backup": "snap-1", "accept_loss": true, "confirm": code,
               "accept_backup_gaps": true})
    };
    scene.approve(&json!([
        gaps("files-v3-reverse", &first),
        gaps(LOSSY_REVERSE, &first)
    ]));
    scene.settle_checks(&bare, 6).await;
    // the first runs
    let jobs = scene.sim.jobs("migration");
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    scene.sim.finish(
        &jobs[0],
        &Finish::Complete(json!({"outcome": "done", "migration": "files-v3-reverse",
            "counts": PassCounts {read: 10, current: 7, migratable: 3, written: 3,
                                  ..PassCounts::default()},
            "groups": []})),
    );
    scene.settle_checks(&bare, 6).await;
    // the second stops for a code of its own, and the copied one is a mismatch
    let second = scene.code(LOSSY_REVERSE);
    assert_ne!(second, first);
    assert_eq!(
        scene.step(LOSSY_REVERSE).blocked,
        Some(BlockReason::ConfirmationRefused {
            confirm: second,
            refusal: ConfirmRefusal::Mismatch,
        })
    );
    assert_eq!(scene.migrations().len(), 1);
}

/// A code shown for an earlier block of the step, before a step ahead of it finished, no longer
/// matches; one shown for the current block still matches after its own note and an unrelated
/// skipped entry were recorded
#[tokio::test]
async fn a_stale_code_is_refused_and_the_current_one_survives_unrelated_entries() {
    // the lossless step ahead has finished, so the lossy one blocks
    let finished = || {
        let mut state = deployed(
            "2026-10-v02",
            &[("2026-10-v02", "tags-v2"), ("2026-10-v02", "users-v2")],
        );
        state.applied.push(entry(
            "2026-10-v01",
            "tags-v2-reverse",
            StepState::Done,
            None,
        ));
        state
    };
    let scene = Scene::to_v01(MIXED, &finished(), &json!([]));
    scene.settle_checks(&covered(), 3).await;
    let shown = scene.code(LOSSY_REVERSE);
    // a step ahead of it finishing again after the block makes the code stale
    let mut state = scene.sim.state().expect("state");
    state.applied.push(entry(
        "2026-10-v01",
        "tags-v2-reverse",
        StepState::Done,
        None,
    ));
    scene.sim.set_state(&state);
    scene.approve(&json!([accepting(LOSSY_REVERSE, &shown)]));
    scene.settle_checks(&covered(), 3).await;
    let current = scene.code(LOSSY_REVERSE);
    assert_ne!(current, shown);
    assert_eq!(
        scene.step(LOSSY_REVERSE).blocked,
        Some(BlockReason::ConfirmationRefused {
            confirm: current,
            refusal: ConfirmRefusal::Mismatch,
        })
    );
    assert_eq!(scene.migrations().len(), 0);
    // in a fresh deployment the first code still matches after a note and an unrelated skip
    let other = Scene::to_v01(MIXED, &finished(), &json!([]));
    other.settle_checks(&covered(), 3).await;
    let first = other.code(LOSSY_REVERSE);
    let mut state = other.sim.state().expect("state");
    state.applied.push(entry(
        "2026-10-v01",
        LOSSY_REVERSE,
        StepState::Blocked,
        None,
    ));
    state.applied.push(entry(
        "2026-10-v01",
        "unrelated-step",
        StepState::Skipped,
        None,
    ));
    other.sim.set_state(&state);
    other.approve(&json!([accepting(LOSSY_REVERSE, &first)]));
    other.settle_checks(&covered(), 3).await;
    assert_eq!(other.migrations().len(), 1);
}

/// After a partial upgrade the reverse edge runs only the reverses of the finished steps, newest
/// first; the same upgrade again runs every forward step of the edge
#[tokio::test]
async fn the_reverse_of_a_partial_upgrade_runs_only_what_finished_newest_first() {
    // a and b finished, c didn't
    let state = deployed(
        "2026-10-v02",
        &[("2026-10-v02", "a-v2"), ("2026-10-v02", "b-v2")],
    );
    let approvals = json!([
        {"step": "a-v2-reverse", "backup": "snap-1"},
        {"step": "b-v2-reverse", "backup": "snap-1"},
        {"step": "c-v2-reverse", "backup": "snap-1"},
    ]);
    let scene = Scene::to_v01(PARTIAL, &state, &approvals);
    let plain = report(3, &[]);
    // each reverse runs and finishes in turn
    for _ in 0..2 {
        scene.settle_checks(&plain, 5).await;
        let active = scene.sim.active("migration");
        assert_eq!(active.len(), 1, "{active:?}");
        scene.sim.finish(
            &active[0],
            &Finish::Complete(json!({"outcome": "done", "migration": "x",
                "counts": PassCounts {read: 10, current: 7, migratable: 3, written: 3,
                                      ..PassCounts::default()},
                "groups": []})),
        );
        scene.rounds(2).await;
    }
    // the reverses of b then a ran, and none for c
    let order = scene
        .migrations()
        .iter()
        .filter_map(|job| Scene::flag(job, "--id"))
        .collect::<Vec<_>>();
    assert_eq!(order, ["b-v2-reverse", "a-v2-reverse"]);
    // the deployment is back at the first revision
    let state = scene.sim.state().expect("state");
    assert_eq!(state.revision, rev("2026-10-v01"));
    // upgrading again plans every forward step: their earlier entries predate the reverses
    let planned = PARTIAL.plan(&state, &rev("2026-10-v02")).expect("plan");
    let ids = planned
        .iter()
        .map(|step| step.step.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["a-v2", "b-v2", "c-v2"]);
}

/// Reversing a step whose revision wrote both forms runs no check or migration Job, and records the
/// step as done without a data pass
#[tokio::test]
async fn reversing_a_dual_write_runs_no_data_pass() {
    let state = deployed("2026-10-v02", &[("2026-10-v02", "users-dw")]);
    let scene = Scene::to_v01(DUAL, &state, &json!([]));
    scene.settle_checks(&report(3, &[]), 3).await;
    // no Job of either kind was created for it
    assert_eq!(scene.sim.created_of("migration").len(), 0);
    assert_eq!(scene.sim.created_of("migration-check").len(), 0);
    // the step is recorded, and the revision moved
    let entries = scene.history("users-dw-reverse");
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0].outcome, StepState::Skipped);
    assert!(
        entries[0]
            .message
            .as_deref()
            .is_some_and(|message| message.contains("both forms"))
    );
    assert_eq!(
        scene.sim.state().expect("state").revision,
        rev("2026-10-v01")
    );
}

/// A downgrade after a failed upgrade blocks every data-changing reverse step for its own approval,
/// even while the upgrade's approvals are still in the spec
#[tokio::test]
async fn a_downgrade_never_reuses_the_approvals_of_the_upgrade() {
    // a finished, b failed, and both approved for the upgrade
    let mut state = deployed("2026-10-v02", &[("2026-10-v02", "a-v2")]);
    state
        .applied
        .push(entry("2026-10-v02", "b-v2", StepState::Failed, None));
    let upgrade_approvals = json!([
        {"step": "a-v2", "backup": "snap-1"},
        {"step": "b-v2", "backup": "snap-1"},
    ]);
    let scene = Scene::to_v01(PARTIAL, &state, &upgrade_approvals);
    scene.settle_checks(&report(3, &[]), 3).await;
    // the reverse of a waits for an approval of its own, and creates no Job
    let step = scene.step("a-v2-reverse");
    assert_eq!(step.state, StepState::Blocked, "{step:?}");
    assert_eq!(step.blocked, Some(BlockReason::Approval));
    assert_eq!(scene.migrations().len(), 0);
    // an approval for the reverse runs it
    scene.approve(&json!([
        {"step": "a-v2", "backup": "snap-1"},
        {"step": "a-v2-reverse", "backup": "snap-2"},
    ]));
    scene.settle_checks(&report(3, &[]), 3).await;
    let created = scene.migrations();
    assert_eq!(created.len(), 1, "{created:#?}");
    assert_eq!(
        Scene::flag(&created[0], "--backup").as_deref(),
        Some("snap-2")
    );
}

/// A step whose newest history entry holds an outcome this build doesn't know is reported blocked
/// on it, runs nothing, and leaves the rest of the reconcile working
#[tokio::test]
async fn a_step_with_an_unknown_recorded_outcome_is_blocked_and_reconciling_goes_on() {
    // a newer operator recorded an outcome this build can't read for the reverse step
    let mut raw = serde_json::to_value(deployed("2026-10-v02", &[("2026-10-v02", "a-v2")]))
        .expect("state serializes");
    raw["applied"]
        .as_array_mut()
        .expect("history")
        .push(json!({"revision": "2026-10-v01", "step": "a-v2-reverse",
                     "outcome": "Quarantined"}));
    let state: UpgradeState = serde_json::from_value(raw).expect("a state with an unknown value");
    assert_eq!(
        state.applied.last().map(|entry| entry.outcome),
        Some(StepState::Unknown)
    );
    let scene = Scene::to_v01(
        PARTIAL,
        &state,
        &json!([{"step": "a-v2-reverse", "backup": "snap-1"}]),
    );
    // every reconcile reports the same block and creates no Job for the step
    scene.rounds(4).await;
    let step = scene.step("a-v2-reverse");
    assert_eq!(step.state, StepState::Blocked, "{step:?}");
    assert!(
        step.message
            .expect("message")
            .contains("outcome this build doesn't know")
    );
    assert_eq!(scene.migrations().len(), 0);
}

/// One lossless step that pauses the API while its reverse runs
const PAUSING: Catalog = Catalog {
    edges: &[
        edge(None, "2026-10-v01", &[]),
        edge(
            Some("2026-10-v01"),
            "2026-10-v02",
            &[forward("a-v2", Some(Reversibility::Backfill))],
        ),
        edge(
            Some("2026-10-v02"),
            "2026-10-v01",
            &[StepDef {
                pause: &[UpgradeComponent::Api],
                ..reverse("a-v2-reverse", "a-v2")
            }],
        ),
    ],
    floors: &[],
};

/// The spec of a cluster that carries everything the newer operator added
fn newer_spec() -> Value {
    // approvals with the newer fields, an acknowledgement, a starting revision, and the setting,
    // which this cluster isn't dev enough to honor
    json!({
        "target_revision": "2026-10-v01",
        "dangerous_downgrade_lossy": true,
        "starting_revision": "2026-10-v01",
        "drift_acknowledged": [{"backend": "scylla", "object": "samples_list.preview"}],
        "approvals": [{"step": "a-v2-reverse", "backup": "snap-1", "accept_issues": ["bad"],
                       "accept_loss": true, "confirm": "ABCDEFGH", "accept_backup_gaps": true}],
    })
}

/// Run a downgrade that pauses the API to its end, from a cluster carrying newer fields and a
/// history holding a state the previous release can't read
async fn handed_over() -> Scene {
    // a finished step, and an entry in a state only the newer operator writes
    let mut state = deployed("2026-10-v02", &[("2026-10-v02", "a-v2")]);
    state.applied.push(entry(
        "2026-10-v02",
        "elastic-rebuild",
        StepState::Reingesting,
        None,
    ));
    let scene = Scene::new(PAUSING, &state, &newer_spec(), &[("api", 3)]);
    scene.settle_checks(&report(3, &[]), 4).await;
    // the reverse runs with the API paused, then finishes
    let job = scene.sim.jobs("migration").remove(0);
    scene.sim.finish(
        &job,
        &Finish::Complete(json!({"outcome": "done", "migration": "a-v2-reverse",
            "counts": PassCounts {read: 10, current: 7, migratable: 3, written: 3,
                                  ..PassCounts::default()},
            "groups": []})),
    );
    scene.rounds(2).await;
    scene
}

/// The last step of a downgrade removes every newer field from the spec and the status, and the
/// operator then reconciles in hand-over mode without putting any of it back, so the previous
/// release's types parse the status after every reconcile
#[tokio::test]
async fn the_strip_removes_newer_fields_and_hand_over_mode_keeps_them_out() {
    let scene = handed_over().await;
    // the setting was refused, so the status held a condition the previous release doesn't know
    // until the strip; now the spec carries none of the newer fields
    let upgrade = scene.sim.cluster().spec.upgrade;
    assert!(!upgrade.dangerous_downgrade_lossy);
    assert_eq!(upgrade.drift_acknowledged.len(), 0);
    assert_eq!(upgrade.starting_revision, None);
    assert_eq!(upgrade.approvals.len(), 1);
    let approval = &upgrade.approvals[0];
    assert_eq!(
        (approval.step.as_str(), approval.backup.as_str()),
        ("a-v2-reverse", "snap-1")
    );
    assert!(!approval.accept_loss && approval.confirm.is_none());
    assert!(approval.accept_issues.is_empty() && !approval.accept_backup_gaps);
    // the spec and the status parse with the previous release's types, and nothing newer is left
    let check = |scene: &Scene| {
        let spec = serde_json::to_value(scene.sim.cluster()).expect("cluster serializes");
        serde_json::from_value::<n1::UpgradeSpec>(spec["spec"]["upgrade"].clone())
            .expect("the previous spec parses");
        let status = scene.sim.status_value();
        let parsed: n1::ThoriumClusterStatus =
            serde_json::from_value(status.clone()).expect("the previous status parses");
        let upgrade = parsed.upgrade.expect("an upgrade status");
        assert_eq!(upgrade.current.as_deref(), Some("2026-10-v01"));
        assert_eq!(upgrade.steps.len(), 0);
        for newer in [
            "conditions",
            "schema",
            "drift",
            "stale_approvals",
            "loss",
            "failure",
        ] {
            assert!(
                status["upgrade"].get(newer).is_none_or(Value::is_null),
                "{newer} is left in {status}"
            );
        }
    };
    check(&scene);
    // three more reconciles of this operator change none of it back
    for _ in 0..3 {
        scene.reconcile().await;
        check(&scene);
    }
    let upgrade = scene.sim.cluster().spec.upgrade;
    assert!(!upgrade.dangerous_downgrade_lossy && upgrade.drift_acknowledged.is_empty());
}

/// A cluster handed over to the first Helm release, or to the pre-Helm operator before it, reports
/// a phase those operators parse: they know only Provisioning, Ready, and Error, and fail to list
/// every `ThoriumCluster` when one holds another phase
#[tokio::test]
async fn the_hand_over_phase_parses_with_the_first_helm_release() {
    let scene = handed_over().await;
    // the status right after the hand-over and after a later reconcile in hand-over mode
    for _ in 0..2 {
        let status: n1::ThoriumClusterStatus =
            serde_json::from_value(scene.sim.status_value()).expect("the previous status parses");
        assert_eq!(status.phase, Some(n1::ClusterPhase::Error));
        let message = status.message.unwrap_or_default();
        assert!(message.contains("handed over"), "{message}");
        scene.reconcile().await;
    }
}

/// The last step of a downgrade writes the upgrade state the previous release's parser accepts,
/// without the entries holding an outcome only the newer operator writes
#[tokio::test]
async fn the_state_is_written_in_the_previous_layout() {
    let scene = handed_over().await;
    // the stored text parses with the previous release's state type
    let text = scene.sim.state_text().expect("a state");
    let previous: n1::UpgradeState =
        serde_json::from_str(&text).expect("the previous parser accepts the state");
    assert_eq!(previous.revision, "2026-10-v01");
    assert!(previous.schema_version <= 1);
    // the entry it couldn't read is gone, and the entries it can read stay
    assert!(
        previous
            .applied
            .iter()
            .all(|entry| entry.step != "elastic-rebuild")
    );
    assert!(
        previous
            .applied
            .iter()
            .any(|entry| entry.step == "a-v2-reverse")
    );
    // later reconciles leave it parseable
    scene.rounds(3).await;
    serde_json::from_str::<n1::UpgradeState>(&scene.sim.state_text().expect("a state"))
        .expect("still parses");
}

/// After the reverse edge finishes, the components it paused stay at zero, the status shows their
/// recorded counts, the history keeps them, and the previous operator's rollout restores them
#[tokio::test]
async fn the_paused_components_stay_at_zero_until_the_previous_operator_restores_them() {
    let scene = handed_over().await;
    // the API was paused for the step and is still at zero after several reconciles
    scene.rounds(3).await;
    assert_eq!(scene.sim.replicas("api"), Some(0));
    assert!(
        scene
            .sim
            .scaled()
            .iter()
            .all(|(_, replicas)| *replicas == 0)
    );
    // the status shows the counts it must return to, which the history keeps
    let message = scene.sim.status().message.expect("a message");
    assert!(message.contains("api (3)"), "{message}");
    let running = scene
        .history("a-v2-reverse")
        .into_iter()
        .find(|entry| entry.outcome == StepState::Running)
        .expect("started");
    assert_eq!(running.paused.get("api"), Some(&3));
    // the previous operator's rollout scales it back from the same record
    let mut cluster = scene.sim.cluster();
    cluster.status = Some(scene.sim.status());
    let client = scene.sim.fake().client();
    let meta = meta_for(cluster, &client);
    pause::restore(&meta, "api").await.expect("restored");
    assert_eq!(scene.sim.replicas("api"), Some(3));
}

/// Raising the target again ends hand-over mode: the next reconcile upgrades
#[tokio::test]
async fn raising_the_target_ends_hand_over_mode() {
    let scene = handed_over().await;
    let mut cluster = scene.sim.cluster();
    // still handed over while the target stays put
    cluster.status = Some(scene.sim.status());
    let client = scene.sim.fake().client();
    assert!(matches!(
        gate(&client, &tracked(&cluster)).await,
        Ok(Gate::Hold(_))
    ));
    // a target above the cluster's revision upgrades it
    let mut raw = serde_json::to_value(&cluster).expect("cluster serializes");
    raw["spec"]["upgrade"]["target_revision"] = json!("2026-10-v02");
    let cluster: ThoriumCluster = serde_json::from_value(raw).expect("cluster parses");
    assert!(matches!(
        gate(&client, &tracked(&cluster)).await,
        Ok(Gate::Upgrade(_))
    ));
}

/// A dev cluster that follows the latest revision, with no explicit target, leaves hand-over mode
/// when the cluster is below the latest revision, since it asks to be moved up
#[tokio::test]
async fn a_dev_cluster_following_the_latest_revision_ends_hand_over_mode() {
    // a cluster handed over to the previous operator
    let scene = handed_over().await;
    let mut cluster = scene.sim.cluster();
    cluster.status = Some(scene.sim.status());
    let client = scene.sim.fake().client();
    // the admin's values now follow the latest revision with no explicit target
    let mut raw = serde_json::to_value(&cluster).expect("cluster serializes");
    raw["spec"]["upgrade"]["target_revision"] = Value::Null;
    raw["spec"]["upgrade"]["auto_target_dev"] = json!(true);
    let cluster: ThoriumCluster = serde_json::from_value(raw).expect("cluster parses");
    // the gate runs the upgrade rather than holding the cluster
    assert!(
        matches!(
            gate(&client, &tracked(&cluster)).await,
            Ok(Gate::Upgrade(_))
        ),
        "still held in hand-over mode"
    );
}

/// One lossy step whose reverse pauses the api
const LOSSY_PAUSING: Catalog = Catalog {
    edges: &[
        edge(None, "2026-10-v01", &[]),
        edge(
            Some("2026-10-v01"),
            "2026-10-v02",
            &[forward("users-v2", Some(LOSSY_REVERT))],
        ),
        edge(
            Some("2026-10-v02"),
            "2026-10-v01",
            &[StepDef {
                pause: &[UpgradeComponent::Api],
                ..reverse(LOSSY_REVERSE, "users-v2")
            }],
        ),
    ],
    floors: &[],
};

/// Build a scene that approved the lossy step of [`LOSSY_PAUSING`] while its api pods are stuck
async fn approved_while_pods_drain() -> Scene {
    // the check settles, and the api's pods don't stop when asked to
    let scene = Scene::to_v01(LOSSY_PAUSING, &at_v02(), &json!([]));
    scene.settle_checks(&covered(), 3).await;
    let code = scene.code(LOSSY_REVERSE);
    scene.sim.stick("api");
    // the approval repeats the code and the step waits on the pods over several reconciles
    scene.approve(&json!([accepting(LOSSY_REVERSE, &code)]));
    scene.rounds(4).await;
    scene
}

/// An approved lossy step that pauses a component whose pods take several reconciles to stop
/// still gets its Job once they are gone, and its start is recorded once
#[tokio::test]
async fn a_lossy_step_with_slow_draining_pods_still_runs() {
    // approved while the api's pods are still running: no Job yet
    let scene = approved_while_pods_drain().await;
    assert!(scene.migrations().is_empty(), "{:?}", scene.migrations());
    // the pods stop and the next reconciles create the Job
    scene.sim.unstick("api");
    scene.rounds(4).await;
    let created = scene.migrations();
    let step = scene.step(LOSSY_REVERSE);
    let history = scene.history(LOSSY_REVERSE);
    assert_eq!(created.len(), 1, "step {step:?} history {history:#?}");
    // the start is recorded once, not again for each reconcile that waited
    let running = history
        .iter()
        .filter(|entry| entry.outcome == StepState::Running)
        .count();
    assert_eq!(running, 1, "{history:#?}");
}

/// An approved lossy step that waits for another writing Job still gets its Job when that Job
/// ends
#[tokio::test]
async fn a_lossy_step_waiting_for_a_writing_job_still_runs() {
    // another writing Job is running when the step is approved
    let scene = Scene::to_v01(LOSSY_PAUSING, &at_v02(), &json!([]));
    scene.settle_checks(&covered(), 3).await;
    let code = scene.code(LOSSY_REVERSE);
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
    let request = http::Request::builder()
        .method("POST")
        .uri("/apis/batch/v1/namespaces/thorium/jobs")
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&writer).expect("json"))
        .expect("a request");
    scene
        .sim
        .fake()
        .client()
        .request_text(request)
        .await
        .expect("created");
    scene.approve(&json!([accepting(LOSSY_REVERSE, &code)]));
    scene.rounds(4).await;
    assert!(scene.migrations().is_empty(), "{:?}", scene.migrations());
    // the writer ends and the step's Job is created
    let request = http::Request::builder()
        .method("DELETE")
        .uri("/apis/batch/v1/namespaces/thorium/jobs/schema-apply-other-aaaaaaaaaaaa")
        .body(Vec::new())
        .expect("a request");
    scene
        .sim
        .fake()
        .client()
        .request_text(request)
        .await
        .expect("deleted");
    scene.rounds(4).await;
    let created = scene.migrations();
    let step = scene.step(LOSSY_REVERSE);
    assert_eq!(created.len(), 1, "step {step:?}");
}
