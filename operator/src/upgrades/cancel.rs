//! Deleting the worker pods whose registrations a cancel run removed, and the counts the run's
//! history entry records
//!
//! A step that cancels in-flight work on approval leaves an applied report in its Job's result row:
//! counts and the workers whose registrations it removed. The operator reads it through a status
//! Job and deletes only those workers' pods that are previous-release Thorium pods of the local
//! cluster, in a namespace the scaler created, outside its own, the default blacklisted, and the
//! configured blacklisted namespaces, with a UID precondition.
//!
//! The worker list, the local cluster, and the configured blacklist all come from the cluster's own
//! config and run, which its owner controls. So a worker's namespace also has to carry the label
//! the scaler puts on every group namespace it creates: the scaler's account can create Namespaces
//! but not relabel them, and the operator only reads them, so a cluster's owner can't steer the
//! deletes into a namespace Thorium didn't create, and the default blacklist always applies.
//!
//! The scaler's label names no deployment: every deployment's scaler sets the same value. So the
//! operator deletes pods only when the cluster being reconciled is the only `ThoriumCluster` on
//! its kube context. When it sees another one, or can't list them all (as in the own-namespace
//! setup, which can't reach group pods either), every worker counts as not reachable and the
//! operator logs why, since another deployment's group namespaces carry the same label.
//!
//! A previous release's agent the operator does not stop keeps running its tool to the end, may
//! then upload its results, tags, and child samples to the upgraded release and its stage logs to
//! the failed reaction, and exits only at its next version check. So for each listed worker it did
//! not stop, the operator logs at WARN a clean-up line (a `kubectl delete pods` command, or what to
//! stop or verify by hand), and a count with a pod listing for the workers the run didn't list.
//! A printed delete selects the pod by the previous release's worker labels for its group and by
//! its name and node, so it deletes nothing unless the pod is that worker. A worker whose delete a
//! guard refused (a protected or unlabelled namespace, or another `ThoriumCluster` possibly on the
//! kube context) gets no command, only a line to verify the pod by hand, so the worker list can't
//! steer an admin into deleting a pod the operator itself wouldn't. Only names that are valid
//! Kubernetes names go into a command. The drain's entry says where the lines are and gives the
//! listing, naming no worker.

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{Namespace, Pod};
use kube::Api;
use kube::api::{DeleteParams, ListParams, Preconditions};
use serde_json::Value;
use std::collections::HashMap;
use thorium::Error;
use thorium::conf::default_namespace_blacklist;
use thorium::models::requisitions::{TOOL_INSTANCE_LABEL, TOOL_LABEL};
use thorium::models::schema::{DrainCancelReport, DrainWorker};
use tracing::{Level, event};

use super::Pending;
use super::record::{self, RUNNING_REQUEUE_SECS, Read};
use crate::k8s::clusters::ClusterMeta;
use crate::k8s::crds::ThoriumCluster;

/// The label key naming the tool that manages a Kubernetes resource
const MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";

/// The value of [`MANAGED_BY_LABEL`] on every group namespace the scaler creates
const MANAGED_BY_SCALER: &str = "thorium-scaler";

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

/// Why a guard refused to delete a listed worker's pod, which no printed command may get around
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refusal {
    /// The namespace is the operator's, the cluster's, or a blacklisted one
    Protected,
    /// The namespace lacks the scaler's label, or could not be read
    Unlabelled,
    /// Another `ThoriumCluster` may be on the operator's kube context
    SharedContext,
}

impl Refusal {
    /// Why the operator refused, for a clean-up line
    #[must_use]
    pub(super) fn why(self) -> &'static str {
        // each guard in the words of a log line
        match self {
            Refusal::Protected => "its namespace is the operator's, the cluster's, or blacklisted",
            Refusal::Unlabelled => "its namespace lacks the scaler's label or could not be read",
            Refusal::SharedContext => {
                "the operator could not confirm this is the only ThoriumCluster on its kube context"
            }
        }
    }
}

/// A listed worker the operator did not stop
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct NotStopped<'a> {
    /// The worker
    pub worker: &'a DrainWorker,
    /// The guard that refused its delete, or none when its pod just wasn't reached
    pub refusal: Option<Refusal>,
}

/// What deleting the listed workers' pods came to, with the workers whose pods weren't deleted
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct PodOutcome<'a> {
    /// How many pods were deleted and how many workers were not reachable
    pub counts: PodCounts,
    /// The listed workers the operator did not stop, in the run's order
    pub not_stopped: Vec<NotStopped<'a>>,
}

/// The label selector of the worker pods a previous release's scaler spawned: its ownership,
/// pipeline, and stage labels, and no label of a tool
#[must_use]
pub(super) fn legacy_selector() -> String {
    // the labels legacy_owned requires, and the tool labels only the new release sets
    format!("thorium=true,user,group,pipeline,stage,!{TOOL_LABEL},!{TOOL_INSTANCE_LABEL}")
}

/// The label selector of a previous release's worker pods in one group: [`legacy_selector`] with
/// the group label set to the group
///
/// # Arguments
///
/// * `group` - The worker's group, which is its pod's namespace
#[must_use]
pub(super) fn worker_selector(group: &str) -> String {
    // the legacy labels, with the group label pinned as legacy_owned requires
    format!("thorium=true,user,group={group},pipeline,stage,!{TOOL_LABEL},!{TOOL_INSTANCE_LABEL}")
}

/// Whether every character of a value is one a shell takes literally outside quotes
///
/// # Arguments
///
/// * `value` - The value
#[must_use]
fn plain_word(value: &str) -> bool {
    // letters, digits, and a few punctuation marks no shell treats specially
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '@' | '/'))
}

/// A value as one shell word: as it is when it is plain, otherwise in single quotes, or none
/// when it holds anything but printable ASCII and spaces, such as a control character no quoting
/// keeps on one line or a line separator or bidi override that would garble the printed line
///
/// # Arguments
///
/// * `value` - The value
#[must_use]
fn shell_word(value: &str) -> Option<String> {
    // a plain value needs no quotes
    if plain_word(value) {
        return Some(value.to_owned());
    }
    // a newline, line separator, or bidi override would break or garble the printed line
    if value.is_empty() || !value.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
        return None;
    }
    // single quotes keep everything literal but a single quote, which closes and reopens them
    Some(format!("'{}'", value.replace('\'', "'\\''")))
}

/// A value for a line that is not a command: as it is when it is plain, otherwise escaped and
/// in double quotes
///
/// # Arguments
///
/// * `value` - The value
#[must_use]
fn shown(value: &str) -> String {
    // the debug form escapes quotes and control characters
    if plain_word(value) {
        value.to_owned()
    } else {
        format!("{value:?}")
    }
}

/// The kube context for each cluster name the scaler config gives its Kubernetes contexts
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
#[must_use]
fn cluster_contexts(meta: &ClusterMeta) -> HashMap<String, String> {
    // a cluster's name is its alias, or its context's name without one
    let k8s = &meta.conf.thorium.scaler.k8s;
    k8s.clusters
        .keys()
        .map(|context| (k8s.cluster_name(context).clone(), context.clone()))
        .collect()
}

/// The line that tells an admin how to stop a listed worker the operator did not stop: a
/// `kubectl delete pods` command for a Kubernetes worker that selects the pod by the previous
/// release's worker labels for its group and by its name and node, with `--context` when it is
/// another cluster's; for a worker whose delete a guard refused, a line to verify the pod by hand
/// with no command; or what to stop by hand for a worker of another scaler, or one whose names no
/// command can safely carry
///
/// # Arguments
///
/// * `worker` - The worker the operator did not stop
/// * `refusal` - The guard that refused its delete, if one did
/// * `local` - The local cluster's name, if the scaler config names the operator's context
/// * `contexts` - The kube context for each cluster name the scaler config lists
#[must_use]
pub(super) fn cleanup_line(
    worker: &DrainWorker,
    refusal: Option<Refusal>,
    local: Option<&str>,
    contexts: &HashMap<String, String>,
) -> String {
    // the scaler may be stored as its JSON text
    let scaler = worker.scaler.trim_matches('"');
    // a worker of another scaler is an agent on a host, which only an admin there can stop
    if !scaler.eq_ignore_ascii_case("k8s") {
        return format!(
            "Stop the previous release's agent by hand on {} node {} of cluster {}: worker {} of \
             group {}",
            shown(scaler),
            shown(&worker.node),
            shown(&worker.cluster),
            shown(&worker.name),
            shown(&worker.group)
        );
    }
    // only valid Kubernetes names go into a command, so none can carry shell text
    if !is_dns_label(&worker.group)
        || !is_dns_subdomain(&worker.name)
        || !is_dns_subdomain(&worker.node)
    {
        return format!(
            "Stop the previous release's worker pod by hand: its namespace {}, name {}, or node {} \
             is not a valid Kubernetes name, so no command is given (cluster {})",
            shown(&worker.group),
            shown(&worker.name),
            shown(&worker.node),
            shown(&worker.cluster)
        );
    }
    // a refused delete gets no command, so the worker list can't aim one past the guard
    if let Some(refusal) = refusal {
        return format!(
            "Verify by hand that pod {} in namespace {} (cluster {}, node {}) is a previous \
             release's worker before stopping it: the operator refused to delete it because {}, \
             so no command is given",
            worker.name,
            worker.group,
            shown(&worker.cluster),
            worker.node,
            refusal.why()
        );
    }
    // the command deletes nothing unless the pod is the legacy worker of its group on its node
    let delete = format!(
        "kubectl delete pods -n {} -l '{}' --field-selector metadata.name={},spec.nodeName={}",
        worker.group,
        worker_selector(&worker.group),
        worker.name,
        worker.node
    );
    // a worker of the local cluster needs no context
    if local == Some(worker.cluster.as_str()) {
        return delete;
    }
    // another cluster's worker names its context, or the cluster's name when the config has none
    let context = contexts.get(&worker.cluster).unwrap_or(&worker.cluster);
    match shell_word(context) {
        Some(context) => format!("{delete} --context {context}"),
        None => format!(
            "Stop the previous release's worker pod by hand: {delete} in the kube context of \
             cluster {}",
            shown(&worker.cluster)
        ),
    }
}

/// The line that tells an admin how many removed workers the run didn't list, and how to list
/// the previous release's worker pods that are left, or none when it listed every one
///
/// # Arguments
///
/// * `not_listed` - How many removed workers were past the run's list limit
#[must_use]
pub(super) fn not_listed_line(not_listed: u64) -> Option<String> {
    // every removed worker was listed, so each has a line of its own
    if not_listed == 0 {
        return None;
    }
    Some(format!(
        "{not_listed} removed workers were past the cancel run's list limit, so they have no \
         clean-up line and their pods were not deleted; list the previous release's worker pods \
         left on each cluster with kubectl get pods -A -l '{}' (adding --context for another \
         cluster)",
        legacy_selector()
    ))
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
/// and in neither the default blacklist nor the cluster's configured one
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
        && !default_namespace_blacklist().contains(namespace)
        && !meta.conf.thorium.namespace_blacklist.contains(namespace)
}

/// Whether a namespace carries the label the scaler puts on the group namespaces it creates; a
/// Namespace that is missing or can't be read doesn't
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `namespace` - The worker's namespace, which is its group
async fn scaler_namespace(meta: &ClusterMeta, namespace: &str) -> bool {
    // read the Namespace; a missing one, a refused read, or any failure fails the check
    let api: Api<Namespace> = Api::all(meta.client.clone());
    let Ok(Some(found)) = api.get_opt(namespace).await else {
        return false;
    };
    // only the scaler's managed-by label counts
    found
        .metadata
        .labels
        .as_ref()
        .and_then(|labels| labels.get(MANAGED_BY_LABEL))
        .is_some_and(|value| value == MANAGED_BY_SCALER)
}

/// Why the pod deletes are refused because the cluster being reconciled may not be the only
/// `ThoriumCluster` on the operator's kube context, or none when it is the only one
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
async fn shared_context(meta: &ClusterMeta) -> Option<&'static str> {
    // list every ThoriumCluster; the own-namespace setup can't, and any failure refuses too
    let api: Api<ThoriumCluster> = Api::all(meta.client.clone());
    let Ok(clusters) = api.list(&ListParams::default()).await else {
        return Some("The ThoriumClusters in every namespace could not be listed");
    };
    // another deployment's group namespaces carry the same scaler label as this one's
    let this = |cluster: &ThoriumCluster| {
        cluster.metadata.namespace.as_deref() == Some(meta.namespace.as_str())
            && cluster.metadata.name.as_deref() == Some(meta.name.as_str())
    };
    (!clusters.items.iter().all(this))
        .then_some("Another ThoriumCluster exists, whose group namespaces carry the same label")
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

/// Delete one worker's pod if it passes every guard, returning whether it was deleted, or the
/// guard that refused it
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `local` - The local cluster's name
/// * `worker` - The worker
/// * `scaler_namespaces` - Whether each namespace checked so far was created by the scaler
async fn delete_one(
    meta: &ClusterMeta,
    local: &str,
    worker: &DrainWorker,
    scaler_namespaces: &mut HashMap<String, bool>,
) -> Result<bool, Refusal> {
    // only a Kubernetes worker the local cluster spawned has a pod this operator can reach; the
    // scaler may be stored as its JSON text
    let k8s = worker.scaler.trim_matches('"').eq_ignore_ascii_case("k8s");
    if !k8s || worker.cluster != local {
        return Ok(false);
    }
    // the pod's namespace is the worker's group, outside the operator's and blacklisted ones
    if !open_namespace(meta, &worker.group) {
        return Err(Refusal::Protected);
    }
    // a pod name that is no Kubernetes name can't be read
    if !is_dns_subdomain(&worker.name) {
        return Ok(false);
    }
    // the namespace must be one the scaler created, read once per namespace
    let created = match scaler_namespaces.get(&worker.group) {
        Some(created) => *created,
        None => {
            let created = scaler_namespace(meta, &worker.group).await;
            scaler_namespaces.insert(worker.group.clone(), created);
            created
        }
    };
    if !created {
        return Err(Refusal::Unlabelled);
    }
    // read the pod; a missing one, a refused read, or any failure leaves it unreached
    let api: Api<Pod> = Api::namespaced(meta.client.clone(), &worker.group);
    let Ok(Some(pod)) = api.get_opt(&worker.name).await else {
        return Ok(false);
    };
    // it must be a legacy worker pod of this group, pinned to the worker's node
    let node = pod.spec.as_ref().and_then(|spec| spec.node_name.as_deref());
    if !legacy_owned(&pod, &worker.group) || node != Some(worker.node.as_str()) {
        return Ok(false);
    }
    // delete only the pod just read, so one recreated under its name in between stays
    let Some(uid) = pod.metadata.uid.clone() else {
        return Ok(false);
    };
    let params = DeleteParams {
        preconditions: Some(Preconditions {
            uid: Some(uid),
            resource_version: None,
        }),
        ..DeleteParams::default()
    };
    Ok(api.delete(&worker.name, &params).await.is_ok())
}

/// Delete the pods of the workers a cancel run removed that pass every guard
///
/// A worker counts as not reachable unless its pod is deleted: when another `ThoriumCluster` exists
/// or they can't all be listed (checked first, so its refusal holds whatever else is true), the
/// local cluster is unknown, the worker is another cluster's or
/// scaler's, its namespace is the operator's, the cluster's, blacklisted by default or by the
/// config, or lacks the scaler's label (or can't be read), its pod is missing, refused, not a
/// previous release's Thorium pod of its group, or on another node, or the delete fails or finds
/// the pod recreated.
///
/// Returns the counts and the listed workers whose pods were not deleted, each with the guard
/// that refused it, if one did.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `workers` - The workers the run listed
pub(super) async fn delete_pods<'a>(
    meta: &ClusterMeta,
    workers: &'a [DrainWorker],
) -> PodOutcome<'a> {
    // nothing deleted yet, so every worker is one the operator did not stop
    let none_stopped = |refusal: Option<Refusal>| PodOutcome {
        counts: PodCounts {
            deleted: 0,
            not_reachable: workers.len() as u64,
        },
        not_stopped: workers
            .iter()
            .map(|worker| NotStopped { worker, refusal })
            .collect(),
    };
    // with no workers listed there is nothing to delete or check
    if workers.is_empty() {
        return PodOutcome::default();
    }
    // the scaler's label ties a namespace to a deployment only when this is the only cluster, so
    // this guard runs first and its verify-by-hand lines apply even when the local cluster is unknown
    if let Some(why) = shared_context(meta).await {
        event!(
            Level::WARN,
            cluster = meta.name,
            namespace = meta.namespace,
            workers = workers.len(),
            "{why}, so no worker pod was deleted"
        );
        return none_stopped(Some(Refusal::SharedContext));
    }
    // without a known local cluster no worker can be tied to this cluster, so none is touched
    let Some(local) = local_cluster(meta).cloned() else {
        return none_stopped(None);
    };
    // each worker's pod, one at a time, reading each namespace's labels only once
    let mut outcome = PodOutcome::default();
    let mut scaler_namespaces = HashMap::new();
    for worker in workers {
        match delete_one(meta, &local, worker, &mut scaler_namespaces).await {
            Ok(true) => outcome.counts.deleted += 1,
            Ok(false) => {
                outcome.counts.not_reachable += 1;
                outcome.not_stopped.push(NotStopped {
                    worker,
                    refusal: None,
                });
            }
            Err(refusal) => {
                outcome.counts.not_reachable += 1;
                outcome.not_stopped.push(NotStopped {
                    worker,
                    refusal: Some(refusal),
                });
            }
        }
    }
    outcome
}

/// The clean-up line for each listed worker the operator did not stop, in the run's order
///
/// A worker in the operator's, the cluster's, or a blacklisted namespace gets no command even on a
/// path that never reached that guard, such as an unknown local cluster.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `outcome` - What deleting the listed workers' pods came to
#[must_use]
pub(super) fn cleanup_lines(meta: &ClusterMeta, outcome: &PodOutcome<'_>) -> Vec<String> {
    // the cluster each listed worker's command is aimed at
    let local = local_cluster(meta).cloned();
    let contexts = cluster_contexts(meta);
    // one line for each worker, refusing a protected namespace whatever the path
    outcome
        .not_stopped
        .iter()
        .map(|not_stopped| {
            let worker = not_stopped.worker;
            let refusal = not_stopped
                .refusal
                .or_else(|| (!open_namespace(meta, &worker.group)).then_some(Refusal::Protected));
            cleanup_line(worker, refusal, local.as_deref(), &contexts)
        })
        .collect()
}

/// Log at WARN a clean-up line for each listed worker the operator did not stop, and one for the
/// removed workers the run didn't list, returning whether any worker may still be running
///
/// The lines name groups and pods, so they go to the operator's log only: the history entry and
/// the cluster's status name no worker, group, or reaction.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `job` - The cancel run's Job name
/// * `outcome` - What deleting the listed workers' pods came to
/// * `not_listed` - How many removed workers were past the run's list limit
fn log_cleanup(meta: &ClusterMeta, job: &str, outcome: &PodOutcome<'_>, not_listed: u64) -> bool {
    // one line for each worker the operator did not stop
    for line in cleanup_lines(meta, outcome) {
        event!(
            Level::WARN,
            job,
            "A previous-release worker the cancel run removed was not stopped and may still run \
             its tool and upload results, tags, and child samples, and stage logs to the failed \
             reaction; clean it up: {line}"
        );
    }
    // and a count with a pod listing for the workers the run didn't list
    let unlisted = not_listed_line(not_listed);
    if let Some(line) = &unlisted {
        event!(Level::WARN, job, "{line}");
    }
    !outcome.not_stopped.is_empty() || unlisted.is_some()
}

/// The line that tells an admin how to list the previous release's worker pods left on each
/// cluster, for when the run named no worker
#[must_use]
pub(super) fn listing_line() -> String {
    // the workers it removed may still run, and only the legacy labels find their pods
    format!(
        "any workers the run removed may still run their tool; list the previous release's \
         worker pods left on each cluster with kubectl get pods -A -l '{}' (adding --context for \
         another cluster)",
        legacy_selector()
    )
}

/// What a drain's entry records when the cancel run left no worker list: that no pod was
/// deleted, and where to look for workers that may still run
#[must_use]
pub(super) fn no_list_note() -> String {
    // the run may have removed workers it couldn't report
    format!(
        "; the run left no worker list, so no worker pod was deleted{}",
        cleanup_note()
    )
}

/// What a drain's entry adds when a worker may still run: where the clean-up lines are, and how
/// to list the previous release's worker pods that are left, naming no worker
#[must_use]
pub(super) fn cleanup_note() -> String {
    // the per-worker lines name groups and pods, so only the operator's log has them
    format!(
        "; workers the operator did not stop may still run their tool and upload results, tags, \
         and child samples, and stage logs to the failed reaction: stop them with the clean-up \
         lines in the operator's log, and list the previous release's worker pods left with \
         kubectl get pods -A -l '{}'",
        legacy_selector()
    )
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
/// names, returning what the step's entry records; the entry holds only counts, and the clean-up
/// lines for the workers not stopped go to the operator's log
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
            "The cancel run left no worker list, so no worker pod was deleted; {}",
            listing_line()
        );
        return Ok(Collected::Done {
            work: None,
            note: no_list_note(),
        });
    };
    // delete the listed workers' pods that pass every guard
    let outcome = delete_pods(meta, &cancelled.workers).await;
    let pods = outcome.counts;
    event!(
        Level::INFO,
        job = job_name,
        deleted = pods.deleted,
        not_reachable = pods.not_reachable,
        not_listed = cancelled.workers_not_listed,
        "Deleted the worker pods the cancel run named"
    );
    // tell the admin how to stop every worker the operator did not, without naming any here
    let left = log_cleanup(meta, &job_name, &outcome, cancelled.workers_not_listed);
    Ok(Collected::Done {
        work: Some(work_text(&cancelled)),
        note: format!(
            "; worker pods: {} deleted, {} not reachable; {} workers not listed for pod \
             deletion{}",
            pods.deleted,
            pods.not_reachable,
            cancelled.workers_not_listed,
            if left { cleanup_note() } else { String::new() }
        ),
    })
}
