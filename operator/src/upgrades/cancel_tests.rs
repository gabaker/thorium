//! Tests of deleting the worker pods a cancel run named: every guard a pod must pass, the UID
//! precondition, the counts the drain's history entry records after its run, and the clean-up
//! lines for the workers the operator did not stop

use std::collections::HashMap;
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

/// Build a fake kube API and cluster metadata that reaches it, with the scaler's `alpha` group
/// namespace and no worker pods yet
fn pod_scene() -> (Sim, ClusterMeta) {
    // the chart's cluster in the `thorium` namespace, and the group namespace the scaler created
    let cluster = chart_cluster(json!({}));
    let sim = Sim::new(&cluster);
    sim.add_scaler_namespace("alpha");
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
    let counts = cancel::delete_pods(&meta, &workers).await.counts;
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
    let counts = cancel::delete_pods(&meta, &workers).await.counts;
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
    let counts = cancel::delete_pods(&meta, &[remote, metal]).await.counts;
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
    let counts = cancel::delete_pods(&meta, &workers).await.counts;
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
    let counts = cancel::delete_pods(&meta, &[worker("alpha", "w-1", "node-a")])
        .await
        .counts;
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
    let counts = cancel::delete_pods(&meta, &workers).await.counts;
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

/// A worker whose namespace lacks the scaler's label, carries another value for it, or can't be
/// found is counted as not reachable without its pod being read
#[tokio::test]
async fn a_namespace_the_scaler_did_not_create_is_skipped() {
    // legacy pods in an unlabelled namespace, one another tool manages, and one that is missing
    let (sim, meta) = pod_scene();
    sim.add_group_namespace("plain", &[]);
    sim.add_group_namespace("other", &[("app.kubernetes.io/managed-by", "Helm")]);
    for group in ["plain", "other", "missing"] {
        let labels = [LEGACY[0], ("group", group), LEGACY[2], LEGACY[3], LEGACY[4]];
        sim.add_worker_pod(group, "w-1", &labels, "node-a");
    }
    let workers = ["plain", "other", "missing"].map(|group| worker(group, "w-1", "node-a"));
    // none of their pods is read or deleted
    let counts = cancel::delete_pods(&meta, &workers).await.counts;
    assert_eq!(
        counts,
        PodCounts {
            deleted: 0,
            not_reachable: 3
        }
    );
    assert_eq!(sim.deleted_pods(), Vec::<(String, String)>::new());
    assert_eq!(worker_pod_requests(&sim), Vec::<String>::new());
}

/// A `ThoriumCluster` in a tenant namespace whose own config empties the namespace blacklist
/// can't steer the pod deletes into a namespace the scaler didn't create or into a default
/// blacklisted one, even one carrying the scaler's label; only its scaler-created group's pod goes
#[tokio::test]
async fn a_clusters_own_config_cannot_steer_pod_deletes_outside_its_groups() {
    // the tenant's cluster, its config naming the operator's context and an empty blacklist
    let mut config = crate::k8s::clusters::tests::sample_config();
    config["thorium"]["namespace_blacklist"] = json!([]);
    let mut spec = crate::k8s::clusters::tests::full_spec();
    spec["config"] = config;
    spec["config_secrets"] = json!([]);
    let mut cluster = crate::k8s::clusters::tests::cluster_from_spec(spec);
    cluster.metadata.namespace = Some("tenant-a".to_owned());
    let sim = Sim::new(&cluster);
    let client = sim.fake().client();
    let mut meta = meta_for(cluster.clone(), &client);
    // the config the operator resolves from the tenant's spec, as it does for every reconcile
    meta.conf =
        crate::k8s::clusters::resolve_config(&cluster, &kube::Api::namespaced(client, "tenant-a"))
            .await
            .expect("the tenant's config resolves");
    assert!(meta.conf.thorium.namespace_blacklist.is_empty());
    // legacy worker pods in a namespace Thorium didn't create, in kube-system (labelled as the
    // scaler labels its namespaces), and in the tenant's own scaler-created group
    sim.add_group_namespace("prod-analysts", &[]);
    sim.add_scaler_namespace("kube-system");
    sim.add_scaler_namespace("tenant-group");
    for (namespace, name) in [
        ("prod-analysts", "prod-w-1"),
        ("kube-system", "ks-w-1"),
        ("tenant-group", "tg-w-1"),
    ] {
        let labels = [
            LEGACY[0],
            ("group", namespace),
            LEGACY[2],
            LEGACY[3],
            LEGACY[4],
        ];
        sim.add_worker_pod(namespace, name, &labels, "node-a");
    }
    // the worker list a cancel run reports, which the tenant's own Job prints
    let workers = [
        worker("prod-analysts", "prod-w-1", "node-a"),
        worker("kube-system", "ks-w-1", "node-a"),
        worker("tenant-group", "tg-w-1", "node-a"),
    ];
    let counts = cancel::delete_pods(&meta, &workers).await.counts;
    // only the tenant's own group pod is read and deleted
    assert_eq!(
        counts,
        PodCounts {
            deleted: 1,
            not_reachable: 2
        }
    );
    assert_eq!(
        sim.deleted_pods(),
        vec![("tenant-group".to_owned(), "tg-w-1".to_owned())]
    );
    for path in worker_pod_requests(&sim) {
        assert!(
            path.starts_with("/api/v1/namespaces/tenant-group/"),
            "{path}"
        );
    }
}

/// With a second `ThoriumCluster` on the operator's kube context, whose deployment's group
/// namespaces carry the same scaler label, no worker pod is read or deleted and each counts as not
/// reachable
#[tokio::test]
async fn a_second_thorium_cluster_means_no_pod_is_deleted() {
    // the chart's cluster with a legacy pod in its scaler-created group, beside a second cluster
    let (sim, meta) = pod_scene();
    sim.add_worker_pod("alpha", "w-1", &LEGACY, "node-a");
    let mut other = chart_cluster(json!({}));
    other.metadata.namespace = Some("tenant-a".to_owned());
    sim.add_other_cluster(&other);
    let counts = cancel::delete_pods(&meta, &[worker("alpha", "w-1", "node-a")])
        .await
        .counts;
    // the pod stays and isn't even read
    assert_eq!(
        counts,
        PodCounts {
            deleted: 0,
            not_reachable: 1
        }
    );
    assert_eq!(sim.deleted_pods(), Vec::<(String, String)>::new());
    assert_eq!(worker_pod_requests(&sim), Vec::<String>::new());
}

/// When the `ThoriumClusters` in every namespace can't be listed, as in the own-namespace setup,
/// no worker pod is read or deleted and each counts as not reachable
#[tokio::test]
async fn unlisted_thorium_clusters_mean_no_pod_is_deleted() {
    // the chart's cluster with a legacy pod in its group, and a refused all-namespace list
    let (sim, meta) = pod_scene();
    sim.add_worker_pod("alpha", "w-1", &LEGACY, "node-a");
    sim.forbid_all_clusters();
    let counts = cancel::delete_pods(&meta, &[worker("alpha", "w-1", "node-a")])
        .await
        .counts;
    // the pod stays and isn't even read
    assert_eq!(
        counts,
        PodCounts {
            deleted: 0,
            not_reachable: 1
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
    let counts = cancel::delete_pods(&meta, &[worker("alpha", "w-1", "node-a")])
        .await
        .counts;
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
    scene.sim.add_scaler_namespace("alpha");
    scene.sim.add_scaler_namespace("beta");
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
    let counts = "; worker pods: 1 deleted, 3 not reachable; 2 workers not listed for pod deletion";
    assert!(
        message.ends_with(&format!("{counts}{}", cancel::cleanup_note())),
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

/// The contexts the formatting tests give: the cluster `remote` is the context `remote-ctx`, and
/// the cluster `odd` a context whose name holds a space and a quote
fn contexts() -> HashMap<String, String> {
    // two other clusters the scaler config would list
    HashMap::from([
        ("remote".to_owned(), "remote-ctx".to_owned()),
        ("odd".to_owned(), "lab ctx's".to_owned()),
    ])
}

/// The listed workers whose pods were not deleted are returned in the run's order, and a deleted
/// one isn't
#[tokio::test]
async fn the_workers_not_stopped_are_returned_in_order() {
    // one pod to delete, a worker whose pod is gone, and a bare-metal worker
    let (sim, meta) = pod_scene();
    sim.add_worker_pod("alpha", "w-ok", &LEGACY, "node-a");
    let mut metal = worker("alpha", "w-metal", "host-1");
    metal.scaler = "BareMetal".to_owned();
    let workers = [
        worker("alpha", "w-gone", "node-a"),
        worker("alpha", "w-ok", "node-a"),
        metal,
    ];
    // only the deleted one is left out
    let outcome = cancel::delete_pods(&meta, &workers).await;
    let names = outcome
        .not_stopped
        .iter()
        .map(|not_stopped| not_stopped.worker.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["w-gone", "w-metal"]);
    assert_eq!(
        outcome.counts,
        PodCounts {
            deleted: 1,
            not_reachable: 2
        }
    );
}

/// The command a clean-up line gives for a worker in the `alpha` group on `node-a`
///
/// # Arguments
///
/// * `name` - The worker's name, which is its pod's name
fn alpha_delete(name: &str) -> String {
    // the group's legacy worker labels, and the pod's name and node
    format!(
        "kubectl delete pods -n alpha -l \
         'thorium=true,user,group=alpha,pipeline,stage,!thorium-tool,!thorium-tool-instance' \
         --field-selector metadata.name={name},spec.nodeName=node-a"
    )
}

/// A Kubernetes worker of the local cluster gets a `kubectl delete pods` command that selects the
/// pod by its group's legacy worker labels and by its name and node
#[test]
fn a_local_k8s_worker_gets_a_delete_command() {
    // the sample config names the local cluster `thorium`
    let line = cancel::cleanup_line(
        &worker("alpha", "alpha-unpack-a41d0", "node-a"),
        None,
        Some("thorium"),
        &contexts(),
    );
    assert_eq!(line, alpha_delete("alpha-unpack-a41d0"));
}

/// A Kubernetes worker of another cluster gets the command with that cluster's kube context,
/// quoted when the context's name isn't a plain word, and the cluster's own name when the config
/// lists no context for it
#[test]
fn another_clusters_k8s_worker_gets_its_context() {
    // workers of a listed cluster, of one with an odd context name, and of an unlisted one
    let mut remote = worker("alpha", "w-1", "node-a");
    remote.cluster = "remote".to_owned();
    let mut odd = worker("alpha", "w-2", "node-a");
    odd.cluster = "odd".to_owned();
    let mut unlisted = worker("alpha", "w-3", "node-a");
    unlisted.cluster = "far".to_owned();
    assert_eq!(
        cancel::cleanup_line(&remote, None, Some("thorium"), &contexts()),
        format!("{} --context remote-ctx", alpha_delete("w-1"))
    );
    assert_eq!(
        cancel::cleanup_line(&odd, None, Some("thorium"), &contexts()),
        format!("{} --context 'lab ctx'\\''s'", alpha_delete("w-2"))
    );
    assert_eq!(
        cancel::cleanup_line(&unlisted, None, Some("thorium"), &contexts()),
        format!("{} --context far", alpha_delete("w-3"))
    );
    // with the local cluster unknown, even a worker named after it gets a context
    assert_eq!(
        cancel::cleanup_line(&worker("alpha", "w-4", "node-a"), None, None, &contexts()),
        format!("{} --context thorium", alpha_delete("w-4"))
    );
}

/// A worker whose delete a guard refused gets no command, only a line naming its namespace and pod
/// and saying to verify it by hand, whatever cluster it is on
#[test]
fn a_guard_refused_worker_gets_no_delete_command() {
    // a refused worker of the local cluster, and one of another cluster
    let mut remote = worker("kube-system", "coredns-abc", "node-a");
    remote.cluster = "remote".to_owned();
    for refusal in [
        cancel::Refusal::Protected,
        cancel::Refusal::Unlabelled,
        cancel::Refusal::SharedContext,
    ] {
        for refused in [&worker("kube-system", "coredns-abc", "node-a"), &remote] {
            let line = cancel::cleanup_line(refused, Some(refusal), Some("thorium"), &contexts());
            assert!(
                line.starts_with(
                    "Verify by hand that pod coredns-abc in namespace kube-system (cluster "
                ),
                "{line}"
            );
            assert!(line.contains(refusal.why()), "{line}");
            assert!(line.ends_with("so no command is given"), "{line}");
            assert!(!line.contains("kubectl"), "{line}");
        }
    }
}

/// Every worker the guards refused gets a verify-by-hand line from the pod deletion: a protected
/// or unlabelled namespace, and every worker when another `ThoriumCluster` exists
#[tokio::test]
async fn the_pod_deletion_reports_each_guard_that_refused() {
    // a blacklisted namespace, an unlabelled one, the scaler's group whose pod is gone
    let (sim, meta) = pod_scene();
    sim.add_group_namespace("plain", &[]);
    let workers = [
        worker("kube-system", "coredns-abc", "node-a"),
        worker("thorium", "api-0", "node-a"),
        worker("plain", "w-1", "node-a"),
        worker("alpha", "w-gone", "node-a"),
    ];
    let outcome = cancel::delete_pods(&meta, &workers).await;
    let refusals = outcome
        .not_stopped
        .iter()
        .map(|not_stopped| not_stopped.refusal)
        .collect::<Vec<_>>();
    assert_eq!(
        refusals,
        [
            Some(cancel::Refusal::Protected),
            Some(cancel::Refusal::Protected),
            Some(cancel::Refusal::Unlabelled),
            None
        ]
    );
    // with a second ThoriumCluster every worker is refused, even the scaler's group's
    let mut other = chart_cluster(json!({}));
    other.metadata.namespace = Some("tenant-a".to_owned());
    sim.add_other_cluster(&other);
    let outcome = cancel::delete_pods(&meta, &workers).await;
    assert!(
        outcome
            .not_stopped
            .iter()
            .all(|not_stopped| not_stopped.refusal == Some(cancel::Refusal::SharedContext)),
        "{outcome:?}"
    );
    assert_eq!(outcome.not_stopped.len(), workers.len());
}

/// With the local cluster unknown, a worker in a blacklisted or the cluster's namespace still
/// gets no command, while one in a group namespace gets the selecting command with its cluster's
/// context
#[tokio::test]
async fn a_protected_namespace_gets_no_command_when_the_local_cluster_is_unknown() {
    // a context the config doesn't list, so no guard past the first is reached
    let (_sim, mut meta) = pod_scene();
    meta.context = "elsewhere".to_owned();
    let workers = [
        worker("kube-system", "coredns-abc", "node-a"),
        worker("thorium", "api-0", "node-a"),
        worker("alpha", "w-1", "node-a"),
    ];
    let outcome = cancel::delete_pods(&meta, &workers).await;
    let lines = cancel::cleanup_lines(&meta, &outcome);
    assert_eq!(lines.len(), 3, "{lines:#?}");
    for line in &lines[..2] {
        assert!(line.starts_with("Verify by hand that pod "), "{line}");
        assert!(!line.contains("kubectl"), "{line}");
    }
    // the config gives the cluster `thorium` its kube context
    assert_eq!(
        lines[2],
        format!(
            "{} --context kubernetes-admin@cluster.local",
            alpha_delete("w-1")
        )
    );
}

/// With the local cluster unknown and a second `ThoriumCluster` present, every listed worker gets
/// only a verify-by-hand line and no `kubectl` command
#[tokio::test]
async fn a_second_thorium_cluster_gives_no_command_when_the_local_cluster_is_unknown() {
    // a context the config doesn't list, beside a second cluster whose groups share the label
    let (sim, mut meta) = pod_scene();
    meta.context = "elsewhere".to_owned();
    let mut other = chart_cluster(json!({}));
    other.metadata.namespace = Some("tenant-a".to_owned());
    sim.add_other_cluster(&other);
    let mut remote = worker("tenant-a-group", "w-2", "node-b");
    remote.cluster = "elsewhere".to_owned();
    let workers = [worker("alpha", "w-1", "node-a"), remote];
    // every worker is refused for the shared context and none gets a command
    let outcome = cancel::delete_pods(&meta, &workers).await;
    assert!(
        outcome
            .not_stopped
            .iter()
            .all(|not_stopped| not_stopped.refusal == Some(cancel::Refusal::SharedContext)),
        "{outcome:?}"
    );
    let lines = cancel::cleanup_lines(&meta, &outcome);
    assert_eq!(lines.len(), workers.len(), "{lines:#?}");
    for line in &lines {
        assert!(line.starts_with("Verify by hand that pod "), "{line}");
        assert!(!line.contains("kubectl"), "{line}");
    }
    assert_eq!(sim.deleted_pods(), Vec::<(String, String)>::new());
}

/// Every printed delete carries the group's legacy worker label selector and a field selector
/// for the pod's name and node, so it deletes nothing unless the pod is that worker
#[test]
fn every_printed_delete_carries_both_selectors() {
    // workers of the local cluster, of other clusters, with the local cluster unknown, with an
    // unprintable context, and refused ones
    let contexts = HashMap::from([
        ("remote".to_owned(), "remote-ctx".to_owned()),
        ("evil".to_owned(), "ctx\nrm -rf /".to_owned()),
    ]);
    let mut lines = Vec::new();
    for (group, name, node) in [
        ("alpha", "w-1", "node-a"),
        ("beta", "beta-unpack-9.x", "node-b.example"),
        ("kube-system", "coredns-abc", "node-a"),
    ] {
        for cluster in ["thorium", "remote", "far", "evil"] {
            let mut listed = worker(group, name, node);
            listed.cluster = cluster.to_owned();
            for local in [Some("thorium"), None] {
                for refusal in [None, Some(cancel::Refusal::Protected)] {
                    lines.push((
                        group,
                        name,
                        node,
                        cancel::cleanup_line(&listed, refusal, local, &contexts),
                    ));
                }
            }
        }
    }
    let mut commands = 0;
    for (group, name, node, line) in &lines {
        // a line with no delete is a by-hand one
        let Some(at) = line.find("kubectl delete") else {
            continue;
        };
        commands += 1;
        let delete = &line[at..];
        let labels = format!(
            "-l 'thorium=true,user,group={group},pipeline,stage,!thorium-tool,\
             !thorium-tool-instance'"
        );
        let fields = format!("--field-selector metadata.name={name},spec.nodeName={node}");
        assert!(
            delete.starts_with(&format!("kubectl delete pods -n {group} {labels} {fields}")),
            "{line}"
        );
    }
    assert_eq!(commands, 3 * 4 * 2, "{lines:#?}");
}

/// Bare-metal, Windows, and KVM workers, including one stored as its JSON text, get a line naming
/// the host and worker to stop by hand, never a command
#[test]
fn a_bare_metal_or_windows_worker_is_stopped_by_hand() {
    // a bare-metal worker stored as JSON text, a Windows one, and a KVM one
    let mut metal = worker("alpha", "w-metal", "host-1");
    metal.scaler = "\"BareMetal\"".to_owned();
    metal.cluster = "lab".to_owned();
    let mut windows = worker("beta", "w-win", "win-7");
    windows.scaler = "Windows".to_owned();
    let mut kvm = worker("beta", "w-kvm", "kvm-2");
    kvm.scaler = "Kvm".to_owned();
    assert_eq!(
        cancel::cleanup_line(&metal, None, Some("thorium"), &contexts()),
        "Stop the previous release's agent by hand on BareMetal node host-1 of cluster lab: \
         worker w-metal of group alpha"
    );
    assert_eq!(
        cancel::cleanup_line(&windows, None, Some("thorium"), &contexts()),
        "Stop the previous release's agent by hand on Windows node win-7 of cluster thorium: \
         worker w-win of group beta"
    );
    assert!(
        cancel::cleanup_line(&kvm, None, Some("thorium"), &contexts())
            .starts_with("Stop the previous release's agent by hand on Kvm node kvm-2 ")
    );
}

/// Removed workers past the run's list limit get a count and a pod listing with the legacy
/// scaler's labels, and none when every removed worker was listed
#[test]
fn workers_not_listed_get_a_count_and_a_pod_listing() {
    // the selector names the legacy labels and excludes the new release's tool labels
    assert_eq!(
        cancel::legacy_selector(),
        "thorium=true,user,group,pipeline,stage,!thorium-tool,!thorium-tool-instance"
    );
    assert_eq!(cancel::not_listed_line(0), None);
    let line = cancel::not_listed_line(3).expect("a line for unlisted workers");
    assert!(line.starts_with("3 removed workers were past"), "{line}");
    assert!(
        line.contains(
            "kubectl get pods -A -l \
             'thorium=true,user,group,pipeline,stage,!thorium-tool,!thorium-tool-instance'"
        ),
        "{line}"
    );
}

/// A namespace or pod name that isn't a valid Kubernetes name never reaches a command: the line
/// says to stop the worker by hand and shows the names escaped, and a context holding a newline
/// is refused the same way
#[test]
fn a_hostile_name_never_reaches_a_command() {
    // shell text in the pod name, the namespace, and a newline in another cluster's context
    let bad_pod = worker("alpha", "w-1; rm -rf / #", "node-a");
    let bad_group = worker("alpha$(reboot)", "w-2", "node-a");
    let mut bad_context = worker("alpha", "w-3", "node-a");
    bad_context.cluster = "evil".to_owned();
    let contexts = HashMap::from([("evil".to_owned(), "ctx\nrm -rf /".to_owned())]);
    let pod_line = cancel::cleanup_line(&bad_pod, None, Some("thorium"), &contexts);
    let group_line = cancel::cleanup_line(&bad_group, None, Some("thorium"), &contexts);
    let context_line = cancel::cleanup_line(&bad_context, None, Some("thorium"), &contexts);
    for line in [&pod_line, &group_line, &context_line] {
        assert!(
            line.starts_with("Stop the previous release's worker pod by hand"),
            "{line}"
        );
        assert!(!line.contains('\n'), "{line}");
    }
    assert!(pod_line.contains("\"w-1; rm -rf / #\""), "{pod_line}");
    assert!(!pod_line.contains("kubectl"), "{pod_line}");
    assert!(group_line.contains("\"alpha$(reboot)\""), "{group_line}");
    assert!(!group_line.contains("kubectl"), "{group_line}");
    // a quote in a name for a by-hand line is escaped rather than closing the quoted name
    let mut quoted = worker("alpha", "w-4", "host\"; reboot");
    quoted.scaler = "BareMetal".to_owned();
    let line = cancel::cleanup_line(&quoted, None, Some("thorium"), &contexts);
    assert!(line.contains("node \"host\\\"; reboot\""), "{line}");
    // a line separator or bidi override in a context is refused like a newline, and escaped
    for odd in ["ctx\u{2028}x", "ctx\u{202e}x"] {
        let mut garbled = worker("alpha", "w-5", "node-a");
        garbled.cluster = odd.to_owned();
        let line = cancel::cleanup_line(&garbled, None, Some("thorium"), &HashMap::new());
        assert!(!line.contains("--context"), "{line}");
        assert!(!line.contains(odd), "{line}");
        assert!(line.contains("\\u{"), "{line}");
    }
}

/// When the cancel run left no worker list, the drain's entry still says workers may run and
/// gives the listing, and every clean-up text names stage logs among the uploads
#[test]
fn a_run_without_a_worker_list_still_gives_the_clean_up_hint() {
    // the note for the no-list path ends with the general clean-up note
    let note = cancel::no_list_note();
    assert!(
        note.starts_with("; the run left no worker list, so no worker pod was deleted"),
        "{note}"
    );
    assert!(note.ends_with(&cancel::cleanup_note()), "{note}");
    assert!(
        cancel::listing_line().contains(&format!(
            "kubectl get pods -A -l '{}'",
            cancel::legacy_selector()
        )),
        "{}",
        cancel::listing_line()
    );
    assert!(
        cancel::cleanup_note().contains("stage logs to the failed reaction"),
        "{}",
        cancel::cleanup_note()
    );
}
