//! Deleting the worker pods whose registrations a cancel run removed, and the counts the run's
//! history entry records
//!
//! A step that cancels in-flight work on approval leaves an applied report in its Job's result row:
//! counts and the workers whose registrations it removed. The operator reads it through a status
//! Job and deletes only those workers' pods that are previous-release Thorium pods of the local
//! cluster, outside its own and the blacklisted namespaces, with a UID precondition.

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use kube::api::{DeleteParams, Preconditions};
use serde_json::Value;
use thorium::Error;
use thorium::models::requisitions::{TOOL_INSTANCE_LABEL, TOOL_LABEL};
use thorium::models::schema::{DrainCancelReport, DrainWorker};
use tracing::{Level, event};

use super::Pending;
use super::record::{self, RUNNING_REQUEUE_SECS, Read};
use crate::k8s::clusters::ClusterMeta;

/// Read the cancel's part of an applied report, if it has one
///
/// # Arguments
///
/// * `applied` - The applied report a run Job left, as raw JSON
#[must_use]
pub(super) fn from_applied(applied: Option<&Value>) -> Option<DrainCancelReport> {
    // only a report with a drain part that parses names workers
    applied
        .and_then(|applied| applied.get("drain"))
        .filter(|drain| drain.is_object())
        .and_then(|drain| serde_json::from_value(drain.clone()).ok())
}

/// Describe a cancel run's counts for its history entry, naming no reaction or worker
///
/// # Arguments
///
/// * `report` - What the cancel run did
#[must_use]
pub(super) fn work_text(report: &DrainCancelReport) -> String {
    // what was cancelled, what became of each request, and the workers removed
    format!(
        "{} reactions cancelled; requests: {} recorded, {} kept from an earlier run, {} refused, \
         {} stale; {} workers removed",
        report.cancelled,
        report.recorded,
        report.kept,
        report.refused,
        report.stale,
        report.workers_removed
    )
}

/// What deleting the listed workers' pods came to
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct PodCounts {
    /// How many pods were deleted
    pub deleted: u64,
    /// How many workers' pods were not found, refused, failed a check, or failed to delete
    pub not_reachable: u64,
}

/// The cluster the scaler config names for the operator's kube context, which is the name the
/// scaler gives the workers it spawns there, or none when the config doesn't list the context
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
#[must_use]
fn local_cluster(meta: &ClusterMeta) -> Option<&String> {
    // an unlisted context would name itself, which a remote cluster could share
    let k8s = &meta.conf.thorium.scaler.k8s;
    k8s.clusters
        .contains_key(&meta.context)
        .then(|| k8s.cluster_name(&meta.context))
}

/// Whether a name is a Kubernetes DNS label, as a namespace is
///
/// # Arguments
///
/// * `name` - The name
#[must_use]
fn is_dns_label(name: &str) -> bool {
    // lowercase letters, digits, and dashes, starting and ending with a letter or digit
    let alphanumeric = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    (1..=63).contains(&name.len())
        && name.chars().all(|c| alphanumeric(c) || c == '-')
        && name.starts_with(alphanumeric)
        && name.ends_with(alphanumeric)
}

/// Whether a name is a Kubernetes DNS subdomain, as a pod's name is
///
/// # Arguments
///
/// * `name` - The name
#[must_use]
fn is_dns_subdomain(name: &str) -> bool {
    // dot-separated labels, at most 253 characters in all
    name.len() <= 253 && name.split('.').all(is_dns_label)
}

/// Whether the operator may delete pods in a worker's namespace: not its own, not the cluster's,
/// and not blacklisted
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `namespace` - The worker's namespace, which is its group
#[must_use]
fn open_namespace(meta: &ClusterMeta, namespace: &str) -> bool {
    // the client's default namespace is the operator's own when it runs in the cluster
    is_dns_label(namespace)
        && namespace != meta.namespace
        && namespace != meta.client.default_namespace()
        && !meta.conf.thorium.namespace_blacklist.contains(namespace)
}

/// Whether a worker's pod was spawned by a previous release's scaler for the worker's group: the
/// ownership labels of the scaler's `thorium_owned` with only its legacy pipeline and stage half,
/// and no label of a tool
///
/// # Arguments
///
/// * `pod` - The pod
/// * `namespace` - The namespace the pod was read from
#[must_use]
fn legacy_owned(pod: &Pod, namespace: &str) -> bool {
    // a pod without labels is not Thorium's
    let Some(labels) = &pod.metadata.labels else {
        return false;
    };
    // a pod of the new release names its tool instance or tool, so it is never a legacy worker
    let new_release = labels.contains_key(TOOL_INSTANCE_LABEL) || labels.contains_key(TOOL_LABEL);
    labels.contains_key("user")
        && labels.get("group").map(String::as_str) == Some(namespace)
        && labels.get("thorium").map(String::as_str) == Some("true")
        && labels.contains_key("pipeline")
        && labels.contains_key("stage")
        && !new_release
}

/// Delete one worker's pod if it passes every guard, returning whether it was deleted
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `local` - The local cluster's name
/// * `worker` - The worker
async fn delete_one(meta: &ClusterMeta, local: &str, worker: &DrainWorker) -> bool {
    // only a Kubernetes worker the local cluster spawned has a pod this operator can reach; the
    // scaler may be stored as its JSON text
    let k8s = worker.scaler.trim_matches('"').eq_ignore_ascii_case("k8s");
    if !k8s || worker.cluster != local {
        return false;
    }
    // the pod's namespace is the worker's group, outside the operator's and blacklisted ones
    if !open_namespace(meta, &worker.group) || !is_dns_subdomain(&worker.name) {
        return false;
    }
    // read the pod; a missing one, a refused read, or any failure leaves it unreached
    let api: Api<Pod> = Api::namespaced(meta.client.clone(), &worker.group);
    let Ok(Some(pod)) = api.get_opt(&worker.name).await else {
        return false;
    };
    // it must be a legacy worker pod of this group, pinned to the worker's node
    let node = pod.spec.as_ref().and_then(|spec| spec.node_name.as_deref());
    if !legacy_owned(&pod, &worker.group) || node != Some(worker.node.as_str()) {
        return false;
    }
    // delete only the pod just read, so one recreated under its name in between stays
    let Some(uid) = pod.metadata.uid.clone() else {
        return false;
    };
    let params = DeleteParams {
        preconditions: Some(Preconditions {
            uid: Some(uid),
            resource_version: None,
        }),
        ..DeleteParams::default()
    };
    api.delete(&worker.name, &params).await.is_ok()
}

/// Delete the pods of the workers a cancel run removed that pass every guard
///
/// A worker counts as not reachable unless its pod is deleted: when the local cluster is unknown,
/// it is another cluster's or scaler's, its namespace is the operator's, the cluster's, or
/// blacklisted, its pod is missing, refused, not a previous release's Thorium pod of its group, or
/// on another node, or the delete fails or finds the pod recreated.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `workers` - The workers the run listed
pub(super) async fn delete_pods(meta: &ClusterMeta, workers: &[DrainWorker]) -> PodCounts {
    // nothing deleted or skipped yet
    let mut counts = PodCounts::default();
    // without a known local cluster no worker can be tied to this cluster, so none is touched
    let Some(local) = local_cluster(meta).cloned() else {
        counts.not_reachable = workers.len() as u64;
        return counts;
    };
    // each worker's pod, one at a time
    for worker in workers {
        if delete_one(meta, &local, worker).await {
            counts.deleted += 1;
        } else {
            counts.not_reachable += 1;
        }
    }
    counts
}

/// What reading a finished cancel run's result came to
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Collected {
    /// The result isn't read yet, and what the step waits on
    Wait(String),
    /// The result was read and the listed workers' pods handled
    Done {
        /// The run's counts for its entry's work, or none when its result held no cancel report
        work: Option<String>,
        /// The pod counts for its entry's message
        note: String,
    },
}

/// Read a finished cancel run's result through a status Job and delete the pods of the workers it
/// names, returning what the step's entry records; only counts are logged
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade being run, which is asked to look again soon while the result is read
/// * `job` - The run's finished Job, whose result row holds the report
pub(super) async fn collect(
    meta: &ClusterMeta,
    pending: &mut Pending,
    job: &Job,
) -> Result<Collected, Error> {
    // ask for the run Job's result row through a status Job of its own
    let uid = job.metadata.uid.clone().unwrap_or_default();
    let job_name = job.metadata.name.clone().unwrap_or_default();
    let report = match record::read_report(meta, &format!("cancel-{uid}"), Some(&uid)).await? {
        Read::Wait(_) => {
            pending.requeue_hint = Some(RUNNING_REQUEUE_SECS);
            return Ok(Collected::Wait(format!(
                "Reading the result of the migration Job {job_name}"
            )));
        }
        Read::Failed(why) => {
            return Ok(Collected::Wait(format!(
                "Reading the result of the migration Job {job_name} failed ({why}); trying again"
            )));
        }
        Read::Report(report) => report,
    };
    // a result without the cancel's report names no worker, so no pod is touched
    let Some(cancelled) = from_applied(report.applied.as_ref()) else {
        event!(
            Level::WARN,
            job = job_name,
            "The cancel run left no worker list, so no worker pod was deleted"
        );
        return Ok(Collected::Done {
            work: None,
            note: "; the run left no worker list, so no worker pod was deleted".to_owned(),
        });
    };
    // delete the listed workers' pods that pass every guard
    let pods = delete_pods(meta, &cancelled.workers).await;
    event!(
        Level::INFO,
        job = job_name,
        deleted = pods.deleted,
        not_reachable = pods.not_reachable,
        not_listed = cancelled.workers_not_listed,
        "Deleted the worker pods the cancel run named"
    );
    Ok(Collected::Done {
        work: Some(work_text(&cancelled)),
        note: format!(
            "; worker pods: {} deleted, {} not reachable; {} workers not listed for pod deletion",
            pods.deleted, pods.not_reachable, cancelled.workers_not_listed
        ),
    })
}
