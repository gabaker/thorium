//! Tests of what a step's approval, check, issues, and outcome look like across runs and edges, which
//! run on the scene of the migration tests

use serde_json::json;
use thorium::models::upgrades::{AppliedStep, StepState, UpgradeState};

use super::*;

/// A second partition size change after a finished re-bucket needs its own approval
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn a_second_rebucket_needs_a_new_approval() {
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
            super::super::schema_apply::ensure_at(&meta, Some("fp"))
                .await
                .expect("ensure")
        }
    };
    let refuse = |name: String| {
        sim.finish(
            &name,
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
    };
    let approved =
        chart_cluster(json!({"approvals": [{"step": "rebucket-files", "backup": "snap-1"}]}));
    // first change: refused, checked, approved, moved
    Box::pin(ensure(approved.clone())).await;
    refuse(sim.jobs("schema-apply").remove(0));
    for _ in 0..2 {
        Box::pin(ensure(approved.clone())).await;
    }
    let check = sim.active("migration-check").remove(0);
    sim.leave_result(&check, check_report("rebucket-files", 4, &[], None));
    sim.finish(
        &check,
        &Finish::Complete(json!({"outcome": "done", "result_id": format!("uid-{check}")})),
    );
    for _ in 0..4 {
        Box::pin(ensure(approved.clone())).await;
    }
    let job = sim.jobs("migration").remove(0);
    sim.finish(&job, &Finish::Complete(run_summary("done", 4, &[])));
    for _ in 0..3 {
        Box::pin(ensure(approved.clone())).await;
    }
    assert_eq!(sim.created_of("schema-apply").len(), 2);
    // the rollout restores the writers
    for name in ["api", "scaler", "baremetal-scaler", "event-handler"] {
        let client = sim.fake().client();
        let mut c = approved.clone();
        c.status = Some(sim.status());
        let meta = meta_for(c, &client);
        apply_deployment(&meta, name).await;
    }
    // the apply succeeds, and later the partition size is changed again: refused again
    let second = sim.jobs("schema-apply").pop().unwrap();
    refuse(second);
    for _ in 0..4 {
        Box::pin(ensure(approved.clone())).await;
    }
    // the old approval and the old check must not start a second move without an admin
    assert_eq!(
        sim.created_of("migration").len(),
        1,
        "a second re-bucket Job started under the first one's approval ({} Jobs)",
        sim.created_of("migration").len()
    );
}

/// Two edges: a gated step and the finalize step in the first, a gated step in the second
const EDGES: Catalog = Catalog {
    edges: &[
        edge(None, "2026-10-v01", &[]),
        edge(
            Some("2026-10-v01"),
            "2026-10-v02",
            &[gated("users-v2", &[], &[]), plain("finalize")],
        ),
        edge(
            Some("2026-10-v02"),
            "2026-10-v03",
            &[gated("tags-v2", &[], &[])],
        ),
    ],
    floors: &[],
};

/// The record moved locally along the injected catalog, recording the finalize step
struct Edges;

impl RecordOps for Edges {
    /// Plan from the state the gate read, since no record is read
    ///
    /// # Arguments
    ///
    /// * `_meta` - Thorium cluster client and metadata
    /// * `_pending` - The upgrade being run
    fn refresh(
        &self,
        _meta: &crate::k8s::clusters::ClusterMeta,
        _pending: &mut Pending,
    ) -> impl std::future::Future<
        Output = Result<Option<kube::runtime::controller::Action>, thorium::Error>,
    > {
        std::future::ready(Ok(None))
    }

    /// Record the revision, with a `Done` entry for the edge's finalize step the way the record's
    /// move does, and move the state along the injected catalog
    ///
    /// # Arguments
    ///
    /// * `meta` - Thorium cluster client and metadata
    /// * `pending` - The upgrade being run
    /// * `revision` - The revision to record
    async fn move_revision(
        &self,
        meta: &crate::k8s::clusters::ClusterMeta,
        pending: &mut Pending,
        revision: &thorium::models::upgrades::RevisionId,
    ) -> Result<super::super::record::Moved, thorium::Error> {
        if pending.stored.state.revision == *revision {
            return Ok(super::super::record::Moved::Done);
        }
        let finalized = pending
            .stored
            .state
            .applied
            .iter()
            .any(|a| a.step == "finalize" && a.revision == revision.as_str());
        if !finalized && pending.steps.iter().any(|s| s.step == "finalize") {
            pending.stored.state.record(AppliedStep {
                revision: revision.to_string(),
                step: "finalize".to_owned(),
                outcome: StepState::Done,
                at: Some(chrono::Utc::now().to_rfc3339()),
                by: Some("thoradm test".to_owned()),
                ..AppliedStep::default()
            });
        }
        super::super::catalog().advance(
            &mut pending.stored.state,
            &pending.target,
            OPERATOR_VERSION,
        );
        super::super::state::save(&meta.client, &meta.namespace, &mut pending.stored).await?;
        Ok(super::super::record::Moved::Done)
    }
}

/// Reconcile with the record moved the way the production tests move it
///
/// # Arguments
///
/// * `scene` - The scene to reconcile
async fn reconcile_edges(scene: &Scene) {
    let mut cluster = scene.cluster.clone();
    cluster.status = Some(scene.sim.status());
    let client = scene.sim.fake().client();
    let Ok(Gate::Upgrade(pending)) = gate(&client, &tracked(&cluster)).await else {
        return;
    };
    let meta = meta_for(cluster, &client);
    Box::pin(run_with(&meta, pending, &Edges))
        .await
        .expect("run");
}

/// The issues of a step of an earlier edge stay in the condition after the record moved past it
#[tokio::test]
async fn issues_of_an_earlier_edge_stay_in_the_condition() {
    CATALOG_OVERRIDE.with(|found| *found.borrow_mut() = Some(EDGES));
    let _guard = Guard;
    let cluster = chart_cluster(json!({
        "target_revision": "2026-10-v03",
        "approvals": [
            {"step": "users-v2", "backup": "snap-1"},
            {"step": "tags-v2", "backup": "snap-1"}
        ],
    }));
    let sim = Sim::new(&cluster);
    let mut state = UpgradeState::new(rev("2026-10-v01"), "1.8.1");
    state
        .schema
        .insert(RECORD_MARKER.to_owned(), "scylla".to_owned());
    sim.set_state(&state);
    sim.auto_complete("schema-status", json!({"outcome": "done"}));
    let scene = Scene {
        sim,
        cluster,
        _guard: Guard,
    };
    let report = check_report("users-v2", 3, &[], None);
    // users-v2 is checked, runs, and ends with an issue group
    for _ in 0..4 {
        reconcile_edges(&scene).await;
        scene.finish_checks(&report);
    }
    let job = scene.sim.jobs("migration").remove(0);
    scene.sim.finish(
        &job,
        &Finish::Complete(run_summary("done_with_issues", 3, &[("bad_type:x", 5)])),
    );
    for _ in 0..3 {
        reconcile_edges(&scene).await;
    }
    assert_eq!(scene.sim.state().unwrap().revision.as_str(), "2026-10-v02");
    // tags-v2 runs clean in the next edge
    for _ in 0..4 {
        reconcile_edges(&scene).await;
        scene.finish_checks(&report);
    }
    let job = scene.sim.jobs("migration").pop().expect("tags job");
    scene
        .sim
        .finish(&job, &Finish::Complete(run_summary("done", 3, &[])));
    for _ in 0..3 {
        reconcile_edges(&scene).await;
    }
    let condition = issues_condition(&scene);
    assert!(
        condition.message.contains("users-v2:"),
        "{} / {}",
        condition.status,
        condition.message
    );
}

/// A resumed run that finds nothing left to write finished a step an earlier pod did the work of, so
/// the step isn't recorded skipped
#[tokio::test]
async fn a_resumed_run_with_nothing_left_is_not_a_skipped_step() {
    let scene = Scene::new(ONE, &approved(), &[]);
    let report = check_report("users-v2", 3, &[], None);
    scene.settle_checks(&report, 3).await;
    let job = scene.sim.jobs("migration").remove(0);
    // the summary a second pod writes when the first pod wrote every object before it was killed
    let mut summary = run_summary("done", 0, &[]);
    summary["resumed"] = json!(true);
    scene.sim.finish(&job, &Finish::Complete(summary));
    scene.rounds(2).await;
    let outcomes = scene
        .history("users-v2")
        .into_iter()
        .map(|entry| entry.outcome)
        .collect::<Vec<_>>();
    assert_eq!(outcomes, vec![StepState::Running, StepState::Done]);
}
