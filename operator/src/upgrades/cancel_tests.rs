//! Tests of deleting the worker pods a cancel run named: every guard a pod must pass, the UID
//! precondition, and the counts the drain's history entry records after its run

use thorium::models::requisitions::{TOOL_INSTANCE_LABEL, TOOL_LABEL};
use thorium::models::schema::{
    AppliedReport, DrainCancelReport, DrainWorker, MAX_DRAIN_WORKERS, RESULT_PREFIX, RecordStatus,
    StatusReport, catalog_identity, join_result, result_lines,
};
use thorium::models::upgrades::Catalog;

use super::*;
use crate::k8s::clusters::ClusterMeta;
use crate::upgrades::cancel::{self, PodCounts};

/// The labels a previous release's scaler put on a worker pod in the `alpha` group
const LEGACY: [(&str, &str); 5] = [
    ("user", "alice"),
    ("group", "alpha"),
    ("thorium", "true"),
    ("pipeline", "triage"),
    ("stage", "unpack"),
];

/// Build a worker the cancel run listed, spawned by the local cluster's scaler
///
/// # Arguments
///
/// * `group` - Its group, which is its pod's namespace
/// * `name` - Its name, which is its pod's name
/// * `node` - The node it ran on
fn worker(group: &str, name: &str, node: &str) -> DrainWorker {
    // the sample config names the in-cluster context's cluster `thorium`
    DrainWorker {
        group: group.to_owned(),
        name: name.to_owned(),
        scaler: "K8s".to_owned(),
        cluster: "thorium".to_owned(),
        node: node.to_owned(),
    }
}

/// Build a fake kube API and cluster metadata that reaches it, with no worker pods yet
fn pod_scene() -> (Sim, ClusterMeta) {
    // the chart's cluster in the `thorium` namespace
    let cluster = chart_cluster(json!({}));
    let sim = Sim::new(&cluster);
    let meta = meta_for(cluster, &sim.fake().client());
    (sim, meta)
}

/// Get the paths of the requests that touched a pod outside the cluster's namespace
///
/// # Arguments
///
/// * `sim` - The fake kube API
fn worker_pod_requests(sim: &Sim) -> Vec<String> {
    // every pod path in a namespace other than the cluster's
    sim.fake()
        .requests()
        .into_iter()
        .map(|request| request.path)
        .filter(|path| path.contains("/pods/") && !path.starts_with("/api/v1/namespaces/thorium/"))
        .collect()
}

/// Only a pod with the user, group, and `thorium=true` labels, a group label equal to its
/// namespace, and the previous release's pipeline and stage labels is deleted
#[tokio::test]
async fn pod_deletion_requires_ownership_and_legacy_labels() {
    // one pod lacking or breaking each label, and one with all of them
    let (sim, meta) = pod_scene();
    let broken: [&[(&str, &str)]; 7] = [
        &LEGACY[1..],
        &[LEGACY[0], LEGACY[2], LEGACY[3], LEGACY[4]],
        &[LEGACY[0], LEGACY[1], LEGACY[3], LEGACY[4]],
        &[
            LEGACY[0],
            LEGACY[1],
            ("thorium", "false"),
            LEGACY[3],
            LEGACY[4],
        ],
        &[
            LEGACY[0],
            ("group", "beta"),
            LEGACY[2],
            LEGACY[3],
            LEGACY[4],
        ],
        &LEGACY[..4],
        &[LEGACY[0], LEGACY[1], LEGACY[2], LEGACY[4]],
    ];
    let mut workers = Vec::new();
    for (index, labels) in broken.iter().enumerate() {
        let name = format!("w-{index}");
        sim.add_worker_pod("alpha", &name, labels, "node-a");
        workers.push(worker("alpha", &name, "node-a"));
    }
    sim.add_worker_pod("alpha", "w-ok", &LEGACY, "node-a");
    workers.push(worker("alpha", "w-ok", "node-a"));
    // only the fully labelled pod goes
    let counts = cancel::delete_pods(&meta, &workers).await;
    assert_eq!(
        counts,
        PodCounts {
            deleted: 1,
            not_reachable: 7
        }
    );
    assert_eq!(
        sim.deleted_pods(),
        vec![("alpha".to_owned(), "w-ok".to_owned())]
    );
}

/// A pod carrying a tool instance or tool label belongs to the new release and is never deleted,
/// even with the previous release's labels
#[tokio::test]
async fn a_pod_of_the_new_release_is_never_deleted() {
    // a new-release pod, and legacy pods that also name a tool instance or a tool
    let (sim, meta) = pod_scene();
    let new = [
        LEGACY[0],
        LEGACY[1],
        LEGACY[2],
        (TOOL_INSTANCE_LABEL, "0b8c"),
    ];
    let mixed = [
        LEGACY[0],
        LEGACY[1],
        LEGACY[2],
        LEGACY[3],
        LEGACY[4],
        (TOOL_INSTANCE_LABEL, "0b8c"),
    ];
    let tool = [
        LEGACY[0],
        LEGACY[1],
        LEGACY[2],
        LEGACY[3],
        LEGACY[4],
        (TOOL_LABEL, "unpacker"),
    ];
    sim.add_worker_pod("alpha", "w-new", &new, "node-a");
    sim.add_worker_pod("alpha", "w-mixed", &mixed, "node-a");
    sim.add_worker_pod("alpha", "w-tool", &tool, "node-a");
    let workers = ["w-new", "w-mixed", "w-tool"].map(|name| worker("alpha", name, "node-a"));
    // none of them goes
    let counts = cancel::delete_pods(&meta, &workers).await;
    assert_eq!(
        counts,
        PodCounts {
            deleted: 0,
            not_reachable: 3
        }
    );
    assert_eq!(sim.deleted_pods(), Vec::<(String, String)>::new());
    assert_eq!(sim.worker_pods().len(), 3);
}

/// A worker another cluster or another scaler spawned is counted as not reachable without its
/// pod being read
#[tokio::test]
async fn a_worker_of_another_cluster_is_skipped() {
    // legacy pods named by a remote cluster's worker and a bare metal worker
    let (sim, meta) = pod_scene();
    sim.add_worker_pod("alpha", "w-remote", &LEGACY, "node-a");
    sim.add_worker_pod("alpha", "w-metal", &LEGACY, "node-a");
    let mut remote = worker("alpha", "w-remote", "node-a");
    remote.cluster = "remote".to_owned();
    let mut metal = worker("alpha", "w-metal", "node-a");
    metal.scaler = "BareMetal".to_owned();
    // neither is touched
    let counts = cancel::delete_pods(&meta, &[remote, metal]).await;
    assert_eq!(
        counts,
        PodCounts {
            deleted: 0,
            not_reachable: 2
        }
    );
    assert_eq!(sim.deleted_pods(), Vec::<(String, String)>::new());
    assert_eq!(worker_pod_requests(&sim), Vec::<String>::new());
}

/// When the operator's context isn't a cluster the scaler config names, no pod is read or
/// deleted, even for a worker whose cluster is named after the context
#[tokio::test]
async fn no_pod_is_deleted_when_the_local_cluster_is_unknown() {
    // a context the config doesn't list, and workers naming it and the configured alias
    let (sim, mut meta) = pod_scene();
    meta.context = "elsewhere".to_owned();
    sim.add_worker_pod("alpha", "w-1", &LEGACY, "node-a");
    sim.add_worker_pod("alpha", "w-2", &LEGACY, "node-a");
    let mut named = worker("alpha", "w-1", "node-a");
    named.cluster = "elsewhere".to_owned();
    let workers = [named, worker("alpha", "w-2", "node-a")];
    // nothing is touched
    let counts = cancel::delete_pods(&meta, &workers).await;
    assert_eq!(
        counts,
        PodCounts {
            deleted: 0,
            not_reachable: 2
        }
    );
    assert_eq!(sim.deleted_pods(), Vec::<(String, String)>::new());
    assert_eq!(worker_pod_requests(&sim), Vec::<String>::new());
}

/// A pod pinned to a node other than the worker's is left alone
#[tokio::test]
async fn a_pod_on_another_node_is_skipped() {
    // a legacy pod on another node than its registration names
    let (sim, meta) = pod_scene();
    sim.add_worker_pod("alpha", "w-1", &LEGACY, "node-b");
    // it stays
    let counts = cancel::delete_pods(&meta, &[worker("alpha", "w-1", "node-a")]).await;
    assert_eq!(
        counts,
        PodCounts {
            deleted: 0,
            not_reachable: 1
        }
    );
    assert_eq!(sim.deleted_pods(), Vec::<(String, String)>::new());
    assert_eq!(sim.worker_pods().len(), 1);
}

/// A worker in the operator's or the cluster's namespace, a blacklisted namespace, or a name that
/// is no namespace is counted as not reachable without a request
#[tokio::test]
async fn a_blacklisted_or_operator_namespace_is_skipped() {
    // a default blacklist entry, one the config adds, the cluster's namespace, and a path
    let (sim, mut meta) = pod_scene();
    meta.conf
        .thorium
        .namespace_blacklist
        .insert("restricted".to_owned());
    let workers = [
        "thorium",
        "kube-system",
        "restricted",
        "alpha/pods/w-1/../..",
    ]
    .map(|group| worker(group, "w-1", "node-a"));
    for group in ["kube-system", "restricted"] {
        let labels = [LEGACY[0], ("group", group), LEGACY[2], LEGACY[3], LEGACY[4]];
        sim.add_worker_pod(group, "w-1", &labels, "node-a");
    }
    // nothing is touched
    let counts = cancel::delete_pods(&meta, &workers).await;
    assert_eq!(
        counts,
        PodCounts {
            deleted: 0,
            not_reachable: 4
        }
    );
    assert_eq!(sim.deleted_pods(), Vec::<(String, String)>::new());
    assert_eq!(worker_pod_requests(&sim), Vec::<String>::new());
}

/// A pod recreated under the same name between the read and the delete fails the UID precondition
/// and survives
#[tokio::test]
async fn a_recreated_pod_survives_the_uid_precondition() {
    // a legacy pod that is replaced right before the delete
    let (sim, meta) = pod_scene();
    sim.add_worker_pod("alpha", "w-1", &LEGACY, "node-a");
    sim.recreate_before_delete("alpha", "w-1");
    // the new pod stays
    let counts = cancel::delete_pods(&meta, &[worker("alpha", "w-1", "node-a")]).await;
    assert_eq!(
        counts,
        PodCounts {
            deleted: 0,
            not_reachable: 1
        }
    );
    assert_eq!(sim.deleted_pods(), Vec::<(String, String)>::new());
    assert_eq!(
        sim.worker_pods(),
        vec![(
            "alpha".to_owned(),
            "w-1".to_owned(),
            "w-1-recreated".to_owned()
        )]
    );
}

/// After the approved drain's run, the pods of the workers its result names are deleted; a
/// refused (403) namespace, a missing pod, and a new-release pod count as not reachable; and the
/// drain is recorded done with the run's counts in its work and the pod counts in its message,
/// naming no worker, group, or reaction
#[tokio::test]
async fn worker_pods_named_by_the_run_are_deleted_and_403_counts_as_not_reachable() {
    // an approved cancel past its grace, with its run's Job created
    let mut scene = drain_scene(&json!([]));
    quiesce(&mut scene).await;
    pass_grace(&scene).await;
    let job = scene.sim.jobs("migration").remove(0);
    // the pods: one to delete, one in a namespace the operator can't touch, one of the new release
    scene.sim.add_worker_pod("alpha", "w-1", &LEGACY, "node-a");
    let beta = [
        LEGACY[0],
        ("group", "beta"),
        LEGACY[2],
        LEGACY[3],
        LEGACY[4],
    ];
    scene.sim.add_worker_pod("beta", "w-2", &beta, "node-a");
    scene.sim.forbid_namespace("beta");
    let new = [
        LEGACY[0],
        LEGACY[1],
        LEGACY[2],
        (TOOL_INSTANCE_LABEL, "0b8c"),
    ];
    scene.sim.add_worker_pod("alpha", "w-3", &new, "node-a");
    // the run's result, bounded and rendered as the run leaves it, names them and a worker whose
    // pod is gone
    let workers = [
        ("alpha", "w-1"),
        ("beta", "w-2"),
        ("alpha", "w-3"),
        ("alpha", "w-gone"),
    ]
    .map(|(group, name)| worker(group, name, "node-a"))
    .to_vec();
    let mut applied = AppliedReport {
        drain: Some(DrainCancelReport {
            cancelled: 5,
            recorded: 3,
            refused: 1,
            stale: 1,
            kept: 0,
            workers_removed: 6,
            workers_not_listed: 2,
            workers,
        }),
        ..AppliedReport::default()
    };
    applied.bound();
    scene.sim.leave_result(
        &job,
        serde_json::to_value(&applied).expect("the report renders"),
    );
    let summary = json!({
        "outcome": "done",
        "migration": DRAIN,
        "counts": PassCounts { read: 11, migratable: 11, written: 11, ..PassCounts::default() },
        "groups": [],
    });
    scene.sim.finish(&job, &Finish::Complete(summary));
    settle(&scene, 4).await;
    // only the legacy pod in a reachable namespace went
    assert_eq!(
        scene.sim.deleted_pods(),
        vec![("alpha".to_owned(), "w-1".to_owned())]
    );
    let entries = scene.history(DRAIN);
    let done = entries.last().expect("an entry");
    assert_eq!(done.outcome, StepState::Done, "{entries:#?}");
    assert_eq!(done.job.as_deref(), Some(job.as_str()));
    assert_eq!(
        done.work.as_deref(),
        Some(
            "5 reactions cancelled; requests: 3 recorded, 0 kept from an earlier run, 1 refused, \
             1 stale; 6 workers removed"
        )
    );
    let message = done.message.clone().unwrap_or_default();
    assert!(
        message.ends_with(
            "; worker pods: 1 deleted, 3 not reachable; 2 workers not listed for pod deletion"
        ),
        "{message}"
    );
    for name in ["alpha", "beta", "w-1", "w-2", "w-3", "w-gone", "node-a"] {
        assert!(!message.contains(name), "{message}");
    }
}

/// The report a cancel run leaves with a full worker list, printed by a status Job in chunks and
/// joined by the operator, gives the pod deletion every worker and the drain's entry its counts
#[test]
fn a_cancel_runs_report_reaches_the_pod_deletion_whole() {
    // a full worker list, bounded as the run leaves it
    let workers = (0..MAX_DRAIN_WORKERS)
        .map(|index| worker("alpha", &format!("w-{index:05}"), "node-a"))
        .collect::<Vec<_>>();
    let drain = DrainCancelReport {
        cancelled: 9,
        recorded: 3,
        refused: 1,
        stale: 1,
        kept: 2,
        workers_removed: 5_003,
        workers_not_listed: 3,
        workers,
    };
    let mut applied = AppliedReport {
        drain: Some(drain.clone()),
        ..AppliedReport::default()
    };
    applied.bound();
    // the status Job's report, printed on result lines
    let status = StatusReport {
        status: RecordStatus::NoKeyspace,
        catalog: catalog_identity(&Catalog::PRODUCTION),
        job_result: None,
        check: None,
        applied: Some(serde_json::to_value(&applied).expect("the report renders")),
    };
    let lines = result_lines(&serde_json::to_string(&status).expect("the status renders"));
    assert!(lines.len() > 1, "{}", lines.len());
    let stripped = lines
        .iter()
        .map(|line| line.strip_prefix(RESULT_PREFIX).expect("the prefix"))
        .collect::<Vec<_>>();
    let joined: StatusReport =
        serde_json::from_str(&join_result(&stripped).expect("the chunks join")).expect("a report");
    // the pod deletion reads every worker, and the entry records the counts
    assert_eq!(
        cancel::from_applied(joined.applied.as_ref()),
        Some(drain.clone())
    );
    assert_eq!(
        cancel::work_text(&drain),
        "9 reactions cancelled; requests: 3 recorded, 2 kept from an earlier run, 1 refused, 1 \
         stale; 5003 workers removed"
    );
}
