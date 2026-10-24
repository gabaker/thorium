//! The Kubernetes Jobs the operator runs thoradm in
//!
//! Every thoradm Job runs the Thorium image as its unprivileged `thorium` user under the chart's
//! `thorium-jobs` service account without a token, with a restricted security context, a read-only
//! root filesystem, and bounded resources. The operator finds its Jobs by their kind and step
//! labels rather than by name, since each attempt gets a name of its own, and deletes them with
//! foreground propagation so a Job stays visible until its pods are gone.

use k8s_openapi::api::batch::v1::{Job, PodFailurePolicy};
use k8s_openapi::api::core::v1::{ContainerState, Pod, PodSpec, ResourceRequirements};
use kube::Api;
use kube::api::{DeleteParams, ListParams, LogParams, PostParams, Preconditions};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use thorium::Error;
use thorium::conf::K8sHostAliases;

use super::clusters::ClusterMeta;
use super::config_maps::TRACING_CONFIG_MAP;
use super::deployments::{mount_elastic_ca_in_pod, restrict_pod};

/// The label naming what a Job is for
pub const KIND_LABEL: &str = "thorium.sandia.gov/job-kind";

/// The label naming the upgrade step a Job runs
pub const STEP_LABEL: &str = "thorium.sandia.gov/step";

/// The label marking the resources the operator manages
const MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";

/// The value of [`MANAGED_BY_LABEL`] on everything the operator creates
const MANAGED_BY: &str = "thorium-operator";

/// The service account every thoradm Job runs under; the chart creates it without any role and
/// with token automounting off
pub const JOBS_SERVICE_ACCOUNT: &str = "thorium-jobs";

/// The name of the thoradm container in a Job's pod
const CONTAINER: &str = "thoradm";

/// Where thorium.yml is mounted in a Job's pod
const CONFIG_PATH: &str = "/conf/thorium.yml";

/// Where tracing.yml is mounted in a Job's pod
const TRACING_PATH: &str = "/conf/tracing.yml";

/// The environment variable holding the Job's UID, which thoradm keys its result row by
pub const JOB_UID_ENV: &str = "THORIUM_JOB_UID";

/// The environment variable holding the Job's name, which names the holder of the writer lease and
/// the Job in the step's history entry
pub const JOB_NAME_ENV: &str = "THORIUM_JOB_NAME";

/// The most log lines read from a failed Job's pod
pub const LOG_TAIL_LINES: i64 = 20;

/// What a Job the operator creates is for, set as its [`KIND_LABEL`] label
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    /// `thoradm schema apply`
    SchemaApply,
    /// `thoradm migrate run`
    Migration,
    /// `thoradm schema status`: read the record and the capped drift summary
    SchemaStatus,
    /// `thoradm migrate check`: classify a gated step's work, writing nothing
    MigrationCheck,
    /// `thoradm migrate record`: record the new revision at the end of a plan
    Record,
}

impl JobKind {
    /// Get the value of this kind's [`KIND_LABEL`] label, which also starts its Jobs' names
    #[must_use]
    pub fn label(self) -> &'static str {
        // name each kind the way its Jobs are labelled
        match self {
            JobKind::SchemaApply => "schema-apply",
            JobKind::Migration => "migration",
            JobKind::SchemaStatus => "schema-status",
            JobKind::MigrationCheck => "migration-check",
            JobKind::Record => "record",
        }
    }
}

/// A Job to build and create
#[derive(Debug, Clone, PartialEq)]
pub struct JobSpec {
    /// What the Job is for
    pub kind: JobKind,
    /// The upgrade step the Job runs, set as its [`STEP_LABEL`] label
    pub step: String,
    /// The Job's name, unique per attempt (see [`job_name`]), so a retry never collides with a
    /// failed Job kept for its logs; the operator finds its Jobs by label, not by name
    pub name: String,
    /// The Thorium image to run
    pub image: String,
    /// The thoradm arguments after `/app/thoradm --cluster-conf /conf/thorium.yml`
    pub args: Vec<String>,
    /// Extra environment variables for thoradm
    pub env: BTreeMap<String, String>,
    /// Annotations carrying the audit fields (revision, attempt, and what triggered the Job)
    pub annotations: BTreeMap<String, String>,
    /// How many times Kubernetes retries a failed pod for a process-class reason
    pub backoff_limit: i32,
    /// `FailJob` on thoradm's refusal and data-issue exit codes, `Ignore` on the
    /// `DisruptionTarget` condition; everything else counts against `backoff_limit`
    pub pod_failure_policy: PodFailurePolicy,
    /// How long a cancelled pod gets to finish its batch and checkpoint
    pub termination_grace_period_seconds: i64,
    /// How long the Job may run, in seconds
    pub active_deadline_seconds: i64,
    /// How long a finished Job is kept for its logs, in seconds
    pub ttl_seconds_after_finished: i32,
    /// Mount the upgrade state `ConfigMap` read-only, which only the record Job does
    pub mount_state: bool,
    /// The service account the pod runs under, which is always [`JOBS_SERVICE_ACCOUNT`]; the pod
    /// spec also turns off token automounting
    pub service_account: String,
    /// CPU and memory requests and limits
    pub resources: ResourceRequirements,
}

/// What polling a Job found
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    /// The Job is still running, or is being deleted
    Running,
    /// The Job finished successfully, with the summary from its pod's termination message (null
    /// when the pod or its message is gone)
    Succeeded(Value),
    /// The Job failed
    Failed {
        /// Why, from the Job's `Failed` condition
        reason: String,
        /// The summary thoradm wrote as its termination message, when the pod got that far
        summary: Option<Value>,
        /// The pod whose logs were read, if one was found
        pod: Option<String>,
        /// At most the last [`LOG_TAIL_LINES`] log lines of that pod
        log_tail: Vec<String>,
    },
}

/// Where a Job is, from its own status alone
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobPhase {
    /// The Job hasn't finished, or is being deleted
    Active,
    /// The Job's `Complete` condition is true
    Complete,
    /// The Job's `Failed` condition is true
    Failed {
        /// The condition's reason and message
        reason: String,
        /// When the condition became true, if the Job says
        at: Option<chrono::DateTime<chrono::Utc>>,
    },
}

/// Build a Job's per-attempt name: `<kind>-<step>-<12 hex>`
///
/// The hex is the start of a sha256 over the parts, each prefixed with its length, so any
/// change to them (another attempt, another trigger) gives another name.
///
/// # Arguments
///
/// * `kind` - What the Job is for
/// * `step` - The step the Job runs
/// * `parts` - What makes this attempt unique: the target revision, the approval's backup, the
///   accepted issue codes, the attempt counter, and anything else that triggered it
#[must_use]
pub fn job_name(kind: JobKind, step: &str, parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    // hash each part after its length so parts can't run into each other
    let mut hasher = Sha256::new();
    for part in std::iter::once(&step).chain(parts) {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    // render the digest as hex
    let digest = format!("{:x}", hasher.finalize());
    // join the kind, the step, and the start of the hash
    format!("{}-{step}-{}", kind.label(), &digest[..12])
}

/// The exit codes of thoradm that fail a Job at once, since retrying would fail the same way: 1 when
/// it refuses to run or fails outright, 2 for a usage error, 3 for a subcommand this image
/// doesn't have, 5 when issue groups failed a run, and 6 when a run failed systematically
const FAIL_JOB_EXIT_CODES: [u8; 5] = [1, 2, 3, 5, 6];

/// Build the pod failure policy every thoradm Job uses
///
/// thoradm's refusals, usage errors, missing subcommands, data-issue failures, and systematic
/// failures ([`FAIL_JOB_EXIT_CODES`]) fail the Job at once, so a run that fails on a group over
/// its limit creates exactly one pod. Exit code 4 (a transient failure) and a pod killed by a
/// signal count against the backoff limit. A pod evicted by a disruption is retried without
/// counting against it.
fn failure_policy() -> Result<PodFailurePolicy, serde_json::Error> {
    // fail at once on thoradm's own failure exit codes and ignore disruptions
    serde_json::from_value(json!({
        "rules": [
            {
                "action": "FailJob",
                "onExitCodes": {
                    "containerName": CONTAINER,
                    "operator": "In",
                    "values": FAIL_JOB_EXIT_CODES
                }
            },
            {
                "action": "Ignore",
                "onPodConditions": [{"type": "DisruptionTarget", "status": "True"}]
            }
        ]
    }))
}

/// Build the requests and limits of a thoradm Job's container
///
/// # Arguments
///
/// * `cpu` - The CPU request and limit, as Kubernetes quantities
/// * `memory` - The memory request and limit, as Kubernetes quantities
fn resources(
    cpu: (&str, &str),
    memory: (&str, &str),
) -> Result<ResourceRequirements, serde_json::Error> {
    // request and limit both CPU and memory
    serde_json::from_value(json!({
        "requests": {"cpu": cpu.0, "memory": memory.0},
        "limits": {"cpu": cpu.1, "memory": memory.1},
    }))
}

/// How many times a thoradm Job's pod is retried for a process-class failure (a crash or kill)
const BACKOFF_LIMIT: i32 = 2;

/// How long a cancelled thoradm pod gets to finish its batch and checkpoint, in seconds
const TERMINATION_GRACE_SECS: i64 = 120;

/// How long a thoradm Job may run, in seconds; a schema apply waits for new views to build
const ACTIVE_DEADLINE_SECS: i64 = 6 * 3600;

/// How long a finished thoradm Job is kept for reading its logs, in seconds
const TTL_AFTER_FINISHED_SECS: i32 = 7 * 86400;

/// Build a Job spec with the settings every thoradm Job shares
///
/// The spec runs under [`JOBS_SERVICE_ACCOUNT`], with the shared failure policy, grace period,
/// deadline, TTL, and resources, without the state `ConfigMap`; callers add their environment
/// and annotations.
///
/// # Arguments
///
/// * `kind` - What the Job is for
/// * `step` - The step the Job runs
/// * `name` - The Job's per-attempt name, from [`job_name`]
/// * `image` - The Thorium image to run
/// * `args` - The thoradm arguments after the config path
pub fn base_spec(
    kind: JobKind,
    step: &str,
    name: String,
    image: String,
    args: Vec<String>,
) -> Result<JobSpec, Error> {
    // fill in the settings every thoradm Job shares
    Ok(JobSpec {
        kind,
        step: step.to_owned(),
        name,
        image,
        args,
        env: BTreeMap::new(),
        annotations: BTreeMap::new(),
        backoff_limit: BACKOFF_LIMIT,
        pod_failure_policy: failure_policy()?,
        termination_grace_period_seconds: TERMINATION_GRACE_SECS,
        active_deadline_seconds: ACTIVE_DEADLINE_SECS,
        ttl_seconds_after_finished: TTL_AFTER_FINISHED_SECS,
        mount_state: false,
        service_account: JOBS_SERVICE_ACCOUNT.to_owned(),
        resources: match kind {
            // the passes read and write at the rate Scylla allows, so they get a core to classify
            // and convert on and room for their batches
            JobKind::Migration | JobKind::MigrationCheck => {
                resources(("1", "2"), ("512Mi", "1Gi"))?
            }
            _ => resources(("250m", "1"), ("256Mi", "1Gi"))?,
        },
    })
}

/// Get the host aliases a cluster's pods get from its k8s scaler config
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
fn host_aliases(meta: &ClusterMeta) -> Vec<K8sHostAliases> {
    // the aliases are configured per k8s cluster, so use the primary one's
    let k8s = &meta.conf.thorium.scaler.k8s;
    k8s.host_aliases(&k8s.primary_cluster)
        .map_or_else(Vec::new, |map| {
            map.iter().map(K8sHostAliases::from).collect()
        })
}

/// Build the labels a Job and its pods carry
///
/// # Arguments
///
/// * `spec` - The Job being built
fn labels(spec: &JobSpec) -> BTreeMap<String, String> {
    // label the kind and step so the operator finds the Job, and mark it as the operator's
    BTreeMap::from([
        (KIND_LABEL.to_owned(), spec.kind.label().to_owned()),
        (STEP_LABEL.to_owned(), spec.step.clone()),
        (MANAGED_BY_LABEL.to_owned(), MANAGED_BY.to_owned()),
    ])
}

/// Build the pod a thoradm Job runs
///
/// # Arguments
///
/// * `spec` - The Job being built
/// * `meta` - Thorium cluster client and metadata
fn pod_spec(spec: &JobSpec, meta: &ClusterMeta) -> Result<PodSpec, Error> {
    // run thoradm against the mounted config with the step's arguments
    let mut args = vec!["--cluster-conf".to_owned(), CONFIG_PATH.to_owned()];
    args.extend(spec.args.iter().cloned());
    // pass the Job's UID from its pod label and its name, then the step's own environment
    let mut env = vec![
        json!({
            "name": JOB_UID_ENV,
            "valueFrom": {"fieldRef": {"fieldPath": "metadata.labels['batch.kubernetes.io/controller-uid']"}}
        }),
        json!({"name": JOB_NAME_ENV, "value": spec.name}),
    ];
    env.extend(
        spec.env
            .iter()
            .map(|(name, value)| json!({"name": name, "value": value})),
    );
    // mount thorium.yml, tracing.yml, and a scratch /tmp, since the root filesystem is read-only
    let mut mounts = vec![
        json!({"name": "config", "mountPath": CONFIG_PATH, "subPath": "thorium.yml", "readOnly": true}),
        json!({"name": "tracing", "mountPath": TRACING_PATH, "subPath": "tracing.yml", "readOnly": true}),
        json!({"name": "tmp", "mountPath": "/tmp"}),
    ];
    let mut volumes = vec![
        json!({"name": "config", "secret": {"secretName": "thorium"}}),
        json!({"name": "tracing", "configMap": {"name": TRACING_CONFIG_MAP}}),
        json!({"name": "tmp", "emptyDir": {}}),
    ];
    // only the record Job reads the upgrade state ConfigMap
    if spec.mount_state {
        mounts.push(json!({"name": "state", "mountPath": "/state", "readOnly": true}));
        volumes.push(json!({
            "name": "state",
            "configMap": {"name": thorium::models::upgrades::STATE_CONFIG_MAP}
        }));
    }
    // build the pod: no token, never restarted in place, and bounded
    let mut pod: PodSpec = serde_json::from_value(json!({
        "serviceAccountName": spec.service_account,
        "automountServiceAccountToken": false,
        "restartPolicy": "Never",
        "terminationGracePeriodSeconds": spec.termination_grace_period_seconds,
        "containers": [{
            "name": CONTAINER,
            "image": spec.image,
            "imagePullPolicy": meta.cluster.spec.image_pull_policy,
            "command": ["/app/thoradm"],
            "args": args,
            "env": env,
            "resources": spec.resources,
            "volumeMounts": mounts,
        }],
        "volumes": volumes,
        "hostAliases": host_aliases(meta),
        "imagePullSecrets": meta.cluster.image_pull_secrets(),
    }))?;
    // give thoradm the CA to validate an external Elastic with
    mount_elastic_ca_in_pod(&mut pod, &meta.cluster)?;
    // run unprivileged, then make every container's root filesystem read-only
    restrict_pod(&mut pod)?;
    for container in &mut pod.containers {
        if let Some(context) = container.security_context.as_mut() {
            context.read_only_root_filesystem = Some(true);
        }
    }
    Ok(pod)
}

/// Build the Kubernetes Job for a thoradm Job
///
/// # Arguments
///
/// * `spec` - The Job to build
/// * `meta` - Thorium cluster client and metadata
pub fn build(spec: &JobSpec, meta: &ClusterMeta) -> Result<Job, Error> {
    // build the pod the Job runs
    let pod = pod_spec(spec, meta)?;
    let labels = labels(spec);
    // wrap it in a Job that is kept for its logs and never runs forever
    let job = serde_json::from_value(json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {
            "name": spec.name,
            "namespace": meta.namespace,
            "labels": labels,
            "annotations": spec.annotations,
        },
        "spec": {
            "backoffLimit": spec.backoff_limit,
            "activeDeadlineSeconds": spec.active_deadline_seconds,
            "ttlSecondsAfterFinished": spec.ttl_seconds_after_finished,
            "podFailurePolicy": spec.pod_failure_policy,
            "template": {
                "metadata": {"labels": labels, "annotations": spec.annotations},
                "spec": pod,
            }
        }
    }))?;
    Ok(job)
}

/// Get the Jobs API for a cluster's namespace
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
fn api(meta: &ClusterMeta) -> Api<Job> {
    // Jobs live in the cluster's own namespace
    Api::namespaced(meta.client.clone(), &meta.namespace)
}

/// Create a thoradm Job
///
/// Returns false when a Job with this name already exists, which is left as it is.
///
/// # Arguments
///
/// * `spec` - The Job to create
/// * `meta` - Thorium cluster client and metadata
pub async fn create(spec: &JobSpec, meta: &ClusterMeta) -> Result<bool, Error> {
    // build the Job and create it
    let job = build(spec, meta)?;
    match api(meta).create(&PostParams::default(), &job).await {
        Ok(_) => {
            println!("Created Job {} in namespace {}", spec.name, meta.namespace);
            Ok(true)
        }
        // a Job with this name is this same attempt, so it is found by label next time
        Err(kube::Error::Api(error)) if error.code == 409 => Ok(false),
        Err(error) => Err(Error::new(format!(
            "Failed to create Job {} in namespace {}: {error}",
            spec.name, meta.namespace
        ))),
    }
}

/// Find the operator's Jobs of a kind for a step, by their labels
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `kind` - What the Jobs are for
/// * `step` - The step they run
pub async fn find(meta: &ClusterMeta, kind: JobKind, step: &str) -> Result<Vec<Job>, Error> {
    // select the Jobs by their kind and step labels
    let selector = format!("{KIND_LABEL}={},{STEP_LABEL}={step}", kind.label());
    let params = ListParams::default().labels(&selector);
    let jobs = api(meta).list(&params).await.map_err(|error| {
        Error::new(format!(
            "Failed to list {} Jobs in namespace {}: {error}",
            kind.label(),
            meta.namespace
        ))
    })?;
    // keep only Jobs carrying the labels, in case the server didn't apply the selector
    let carries = |job: &Job, label: &str, value: &str| {
        job.metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get(label))
            .is_some_and(|found| found == value)
    };
    Ok(jobs
        .items
        .into_iter()
        .filter(|job| carries(job, KIND_LABEL, kind.label()) && carries(job, STEP_LABEL, step))
        .collect())
}

/// Find the operator's Jobs of a kind for any step, by their kind label
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `kind` - What the Jobs are for
pub async fn find_kind(meta: &ClusterMeta, kind: JobKind) -> Result<Vec<Job>, Error> {
    // select the Jobs by their kind label
    let params = ListParams::default().labels(&format!("{KIND_LABEL}={}", kind.label()));
    match api(meta).list(&params).await {
        Ok(jobs) => Ok(jobs
            .items
            .into_iter()
            .filter(|job| {
                job.metadata
                    .labels
                    .as_ref()
                    .and_then(|labels| labels.get(KIND_LABEL))
                    .is_some_and(|found| found == kind.label())
            })
            .collect()),
        // a namespace's Job list never answers 404, so a missing list is an empty one
        Err(kube::Error::Api(error)) if error.code == 404 => Ok(Vec::new()),
        Err(error) => Err(Error::new(format!(
            "Failed to list {} Jobs in namespace {}: {error}",
            kind.label(),
            meta.namespace
        ))),
    }
}

/// The kinds of Job that write to the deployment's backends, of which at most one runs in a
/// namespace at a time
const WRITING_KINDS: [JobKind; 3] = [JobKind::SchemaApply, JobKind::Migration, JobKind::Record];

/// Find the writing thoradm Jobs and pods that haven't ended in a namespace
///
/// A Job counts while it is active or being deleted, and so does a pod of a writing kind that
/// hasn't terminated even when its Job is gone, since a deleted Job's pod keeps writing until it is
/// stopped. Read-only check and status Jobs don't count. Returns the names of what is still
/// active, Jobs first.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
pub async fn active_writers(meta: &ClusterMeta) -> Result<Vec<String>, Error> {
    let mut active = Vec::new();
    for kind in WRITING_KINDS {
        // a Job that hasn't finished, or is being deleted, may still have a running pod
        for job in find_kind(meta, kind).await? {
            if phase(&job) == JobPhase::Active {
                active.push(job.metadata.name.clone().unwrap_or_default());
            }
        }
        // a pod of the kind that hasn't terminated is a writer whatever became of its Job
        let params = ListParams::default().labels(&format!("{KIND_LABEL}={}", kind.label()));
        let pods = match meta.pod_api.list(&params).await {
            Ok(pods) => pods.items,
            Err(kube::Error::Api(error)) if error.code == 404 => Vec::new(),
            Err(error) => {
                return Err(Error::new(format!(
                    "Failed to list the {} pods in namespace {}: {error}",
                    kind.label(),
                    meta.namespace
                )));
            }
        };
        for pod in pods.into_iter().filter(|pod| {
            // keep only pods carrying the label, in case the server didn't apply the selector
            pod.metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get(KIND_LABEL))
                .is_some_and(|found| found == kind.label())
        }) {
            let phase = pod
                .status
                .as_ref()
                .and_then(|status| status.phase.as_deref())
                .unwrap_or("Pending");
            if !matches!(phase, "Succeeded" | "Failed") {
                let name = pod.metadata.name.clone().unwrap_or_default();
                if !active.contains(&name) {
                    active.push(name);
                }
            }
        }
    }
    Ok(active)
}

/// Find where a Job is from its status
///
/// A Job being deleted is still active, since its pods may still be running.
///
/// # Arguments
///
/// * `job` - The Job to check
#[must_use]
pub fn phase(job: &Job) -> JobPhase {
    // a Job being deleted may still have running pods
    if job.metadata.deletion_timestamp.is_some() {
        return JobPhase::Active;
    }
    // look for a terminal condition that holds
    let conditions = job
        .status
        .as_ref()
        .and_then(|status| status.conditions.as_ref());
    for condition in conditions.into_iter().flatten() {
        if condition.status != "True" {
            continue;
        }
        match condition.type_.as_str() {
            "Complete" => return JobPhase::Complete,
            "Failed" => {
                // describe the failure from the condition's reason and message
                let reason = [condition.reason.as_deref(), condition.message.as_deref()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(": ");
                let at = condition.last_transition_time.as_ref().map(|time| time.0);
                return JobPhase::Failed { reason, at };
            }
            _ => (),
        }
    }
    JobPhase::Active
}

/// List the pods of a Job
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `job` - The name of the Job
async fn pods(meta: &ClusterMeta, job: &str) -> Result<Vec<Pod>, Error> {
    // select the Job's pods by the label Kubernetes gives them
    let params = ListParams::default().labels(&format!("job-name={job}"));
    let pods = meta.pod_api.list(&params).await.map_err(|error| {
        Error::new(format!(
            "Failed to list the pods of Job {job} in namespace {}: {error}",
            meta.namespace
        ))
    })?;
    Ok(pods.items)
}

/// Find the newest pod of a Job
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `job` - The name of the Job
async fn newest_pod(meta: &ClusterMeta, job: &str) -> Result<Option<Pod>, Error> {
    // the newest pod has the latest result
    Ok(pods(meta, job)
        .await?
        .into_iter()
        .max_by_key(|pod| pod.metadata.creation_timestamp.as_ref().map(|time| time.0)))
}

/// Decide whether a Job's pod never started thoradm
///
/// A pod that hasn't left the `Pending` phase, and whose thoradm container neither runs nor
/// ran before, never started thoradm: it is still scheduling, pulling its image (or backing off
/// from a failed pull), or waiting on its config. A pod that is running or finished, or whose
/// container ran once, did start it.
///
/// # Arguments
///
/// * `pod` - The Job's pod
#[must_use]
pub fn pod_never_started(pod: &Pod) -> bool {
    // a pod without a status hasn't been scheduled yet
    let Some(status) = pod.status.as_ref() else {
        return true;
    };
    // a pod past Pending started its container
    if status
        .phase
        .as_deref()
        .is_some_and(|phase| phase != "Pending")
    {
        return false;
    }
    // a container that runs or ran, now or before a restart, started thoradm
    let never_ran = |state: Option<&ContainerState>| {
        state.is_none_or(|state| state.running.is_none() && state.terminated.is_none())
    };
    status.container_statuses.iter().flatten().all(|container| {
        never_ran(container.state.as_ref()) && never_ran(container.last_state.as_ref())
    })
}

/// Decide whether none of a Job's pods ever started thoradm, which holds when it has no pods
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `job` - The name of the Job
pub async fn never_started(meta: &ClusterMeta, job: &str) -> Result<bool, Error> {
    // every pod of the Job must never have started thoradm
    Ok(pods(meta, job).await?.iter().all(pod_never_started))
}

/// Delete a Job, leaving it until its pods are gone; a Job already gone is fine
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `name` - The name of the Job
pub async fn delete(meta: &ClusterMeta, name: &str) -> Result<(), Error> {
    // delete it in the foreground so it stays until its pods are gone
    match api(meta).delete(name, &DeleteParams::foreground()).await {
        Ok(_) => {
            println!("Deleted Job {name} in namespace {}", meta.namespace);
            Ok(())
        }
        // a Job that is already gone is the state we want
        Err(kube::Error::Api(error)) if error.code == 404 => Ok(()),
        Err(error) => Err(Error::new(format!(
            "Failed to delete Job {name} in namespace {}: {error}",
            meta.namespace
        ))),
    }
}

/// What a delete of a Job pinned to the version it was read at came to
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pinned {
    /// The Job is being deleted in the foreground, so it stays until its pods are gone
    Deleting,
    /// The Job changed since it was read, as when it ended in between, so nothing was deleted and
    /// it is looked at again
    Changed,
    /// The Job is already gone
    Gone,
}

/// Delete a Job in the foreground only if it is still the version it was read at
///
/// A Job that ended between the read and the delete would otherwise lose the termination summary
/// its pod left, so the delete names the Job's UID and resource version and a changed Job is left
/// for the caller to look at again.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `job` - The Job as it was read
pub async fn delete_unchanged(meta: &ClusterMeta, job: &Job) -> Result<Pinned, Error> {
    // pin the delete to the Job's UID and the version it was read at
    let name = job.metadata.name.clone().unwrap_or_default();
    let params = DeleteParams {
        preconditions: Some(Preconditions {
            resource_version: job.metadata.resource_version.clone(),
            uid: job.metadata.uid.clone(),
        }),
        ..DeleteParams::foreground()
    };
    // delete it in the foreground so it stays until its pods are gone
    match api(meta).delete(&name, &params).await {
        Ok(_) => {
            println!("Deleted Job {name} in namespace {}", meta.namespace);
            Ok(Pinned::Deleting)
        }
        // a Job that changed since it was read is looked at again rather than deleted
        Err(kube::Error::Api(error)) if error.code == 409 => Ok(Pinned::Changed),
        // a Job that is already gone is the state we want
        Err(kube::Error::Api(error)) if error.code == 404 => Ok(Pinned::Gone),
        Err(error) => Err(Error::new(format!(
            "Failed to delete Job {name} in namespace {}: {error}",
            meta.namespace
        ))),
    }
}

/// Get the termination message thoradm wrote in a pod, parsed as JSON
///
/// # Arguments
///
/// * `pod` - The Job's pod
fn termination_summary(pod: &Pod) -> Option<Value> {
    // find the thoradm container's last termination message
    let statuses = pod.status.as_ref()?.container_statuses.as_ref()?;
    let message = statuses
        .iter()
        .filter(|status| status.name == CONTAINER)
        .find_map(|status| status.state.as_ref()?.terminated.as_ref()?.message.clone())?;
    // keep a message that isn't JSON as a plain string
    Some(serde_json::from_str(&message).unwrap_or(Value::String(message)))
}

/// Read the last log lines of a pod, or none when they can't be read
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pod` - The name of the pod
async fn log_tail(meta: &ClusterMeta, pod: &str) -> Vec<String> {
    // read only the last lines
    let params = LogParams {
        container: Some(CONTAINER.to_owned()),
        tail_lines: Some(LOG_TAIL_LINES),
        ..LogParams::default()
    };
    match meta.pod_api.logs(pod, &params).await {
        // keep at most the cap even if the server sent more
        Ok(logs) => {
            let lines: Vec<String> = logs.lines().map(ToOwned::to_owned).collect();
            let skip = lines
                .len()
                .saturating_sub(usize::try_from(LOG_TAIL_LINES).unwrap_or(usize::MAX));
            lines.into_iter().skip(skip).collect()
        }
        // logs are only a diagnostic, so a pod whose logs are gone has none
        Err(_) => Vec::new(),
    }
}

/// Read the line a finished Job's pod printed that starts with a prefix
///
/// This is how a status Job's report reaches the operator: the whole pod log is read, since the
/// line is far larger than the termination message, and the one line with the prefix is kept.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `job` - The name of the Job
/// * `prefix` - The prefix of the line to find
pub async fn result_line(
    meta: &ClusterMeta,
    job: &str,
    prefix: &str,
) -> Result<Option<String>, Error> {
    // the newest pod has the latest output
    let Some(pod) = newest_pod(meta, job).await? else {
        return Ok(None);
    };
    let Some(name) = pod.metadata.name else {
        return Ok(None);
    };
    // read the whole log of the thoradm container
    let params = LogParams {
        container: Some(CONTAINER.to_owned()),
        ..LogParams::default()
    };
    let logs = meta.pod_api.logs(&name, &params).await.map_err(|error| {
        Error::new(format!(
            "Failed to read the log of pod {name} in namespace {}: {error}",
            meta.namespace
        ))
    })?;
    // exactly one line may carry the prefix: a second one is not the Job's own output
    let mut lines = logs.lines().filter_map(|line| line.strip_prefix(prefix));
    let found = lines.next().map(ToOwned::to_owned);
    if lines.next().is_some() {
        return Err(Error::new(format!(
            "The log of pod {name} holds more than one `{}` line, so none is trusted",
            prefix.trim_end()
        )));
    }
    Ok(found)
}

/// Poll a Job, reading its pod's termination message and, on failure, its log tail
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `job` - The Job to poll
pub async fn state(meta: &ClusterMeta, job: &Job) -> Result<JobState, Error> {
    // a Job that hasn't finished needs no pod read
    let phase = phase(job);
    if phase == JobPhase::Active {
        return Ok(JobState::Running);
    }
    // read the newest pod's termination message
    let name = job.metadata.name.clone().unwrap_or_default();
    let pod = newest_pod(meta, &name).await?;
    let summary = pod.as_ref().and_then(termination_summary);
    match phase {
        JobPhase::Failed { reason, .. } => {
            // read the failed pod's last log lines for the status
            let pod = pod.and_then(|pod| pod.metadata.name);
            let log_tail = match &pod {
                Some(pod) => log_tail(meta, pod).await,
                None => Vec::new(),
            };
            Ok(JobState::Failed {
                reason,
                summary,
                pod,
                log_tail,
            })
        }
        _ => Ok(JobState::Succeeded(summary.unwrap_or(Value::Null))),
    }
}

/// Delete every thoradm Job of a cluster, leaving each until its pods are gone
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
pub async fn delete_all(meta: &ClusterMeta) -> Result<(), Error> {
    // select every Job the operator created for thoradm
    let params =
        ListParams::default().labels(&format!("{MANAGED_BY_LABEL}={MANAGED_BY},{KIND_LABEL}"));
    // delete them in the foreground so each stays until its pods are gone
    match api(meta)
        .delete_collection(&DeleteParams::foreground(), &params)
        .await
    {
        Ok(_) => Ok(()),
        // nothing to delete is already the state we want
        Err(kube::Error::Api(error)) if error.code == 404 => Ok(()),
        Err(error) => Err(Error::new(format!(
            "Failed to delete the thoradm Jobs in namespace {}: {error}",
            meta.namespace
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::clusters::tests::{
        FakeKube, full_spec, kube_status, meta_for, namespaced_cluster,
    };

    /// Deleting a cluster's thoradm Jobs deletes every Job the operator labelled with a kind,
    /// in the foreground, and finds nothing to delete fine
    #[tokio::test]
    async fn delete_all_removes_thoradm_jobs_in_the_foreground() {
        // a namespace whose Jobs collection can be deleted
        let path = "/apis/batch/v1/namespaces/thorium/jobs";
        let empty =
            json!({"apiVersion": "batch/v1", "kind": "JobList", "metadata": {}, "items": []});
        let fake = FakeKube::default().route("DELETE", path, 200, empty);
        let meta = meta_for(namespaced_cluster(full_spec()), &fake.client());
        // delete the cluster's thoradm Jobs
        delete_all(&meta).await.expect("delete");
        // one collection delete, in the foreground, of the operator's Jobs that have a kind
        let deletes: Vec<_> = fake
            .writes()
            .into_iter()
            .filter(|request| request.method == "DELETE")
            .collect();
        assert_eq!(deletes.len(), 1, "{deletes:?}");
        assert_eq!(deletes[0].path, path);
        assert_eq!(deletes[0].body["propagationPolicy"], "Foreground");
        assert_eq!(
            deletes[0].query_param("labelSelector").as_deref(),
            Some("app.kubernetes.io/managed-by=thorium-operator,thorium.sandia.gov/job-kind")
        );
        // a namespace without a Jobs collection has nothing to delete
        let missing = FakeKube::default();
        let meta = meta_for(namespaced_cluster(full_spec()), &missing.client());
        delete_all(&meta).await.expect("nothing to delete");
    }

    /// Build a Job's pod in a phase with the thoradm container's state and last state
    ///
    /// # Arguments
    ///
    /// * `phase` - The pod's phase
    /// * `state` - The container's state, if it has a status
    /// * `last` - The container's last state, if it ran before
    fn pod(phase: &str, state: Option<Value>, last: Option<Value>) -> Pod {
        // add a container status only when there is a state to report
        let mut raw = json!({"metadata": {"name": "pod"}, "status": {"phase": phase}});
        if let Some(state) = state {
            let mut status = json!({
                "name": CONTAINER,
                "ready": false,
                "restartCount": 0,
                "image": "registry/thorium:1.8.1",
                "imageID": "",
                "state": state
            });
            // add the state the container was in before a restart, if it ran before
            if let Some(last) = last {
                status["lastState"] = last;
            }
            raw["status"]["containerStatuses"] = json!([status]);
        }
        serde_json::from_value(raw).expect("pod")
    }

    /// A pod that is pending and whose container never ran never started thoradm, while one
    /// that runs, ran, or finished did
    #[test]
    fn pod_never_started_reads_the_pod() {
        // a pod without a status or container statuses hasn't started anything
        let bare: Pod = serde_json::from_value(json!({"metadata": {"name": "pod"}})).expect("pod");
        assert!(pod_never_started(&bare));
        assert!(pod_never_started(&pod("Pending", None, None)));
        // a pending container waiting on its image or config never started thoradm
        for reason in [
            "ImagePullBackOff",
            "ErrImagePull",
            "CreateContainerConfigError",
            "ContainerCreating",
        ] {
            let waiting = json!({"waiting": {"reason": reason}});
            assert!(
                pod_never_started(&pod("Pending", Some(waiting), None)),
                "{reason}"
            );
        }
        // a running pod or container started thoradm, even before its status lists the
        // container
        assert!(!pod_never_started(&pod("Running", None, None)));
        let running = json!({"running": {"startedAt": "2026-10-21T00:00:00Z"}});
        assert!(!pod_never_started(&pod(
            "Running",
            Some(running.clone()),
            None
        )));
        assert!(!pod_never_started(&pod("Pending", Some(running), None)));
        // a container that ran before and is now backing off started thoradm
        let backoff = json!({"waiting": {"reason": "CrashLoopBackOff"}});
        let ran = json!({"terminated": {"exitCode": 137}});
        assert!(!pod_never_started(&pod(
            "Pending",
            Some(backoff),
            Some(ran.clone())
        )));
        // a finished pod started thoradm
        assert!(!pod_never_started(&pod("Failed", Some(ran.clone()), None)));
        assert!(!pod_never_started(&pod("Succeeded", Some(ran), None)));
    }

    /// A Job's pods never started thoradm only when none of them did, which holds for a Job
    /// without pods
    #[tokio::test]
    async fn never_started_needs_every_pod() {
        // a Job with one pending pod and one running pod
        let path = "/api/v1/namespaces/thorium/pods";
        let list = |pods: Vec<Pod>| json!({"apiVersion": "v1", "kind": "PodList", "metadata": {}, "items": pods});
        let waiting = json!({"waiting": {"reason": "ContainerCreating"}});
        let running = json!({"running": {"startedAt": "2026-10-21T00:00:00Z"}});
        let mixed = list(vec![
            pod("Pending", Some(waiting.clone()), None),
            pod("Running", Some(running), None),
        ]);
        let fake = FakeKube::default().route("GET", path, 200, mixed);
        let meta = meta_for(namespaced_cluster(full_spec()), &fake.client());
        assert!(!never_started(&meta, "old").await.expect("pods"));
        // the pods were listed by the Job's name label
        let selector = fake.requests()[0].query_param("labelSelector");
        assert_eq!(selector.as_deref(), Some("job-name=old"));
        // two pending pods, or none at all, never started thoradm
        fake.set_route(
            "GET",
            path,
            200,
            list(vec![
                pod("Pending", Some(waiting.clone()), None),
                pod("Pending", Some(waiting), None),
            ]),
        );
        assert!(never_started(&meta, "old").await.expect("pods"));
        fake.set_route("GET", path, 200, list(Vec::new()));
        assert!(never_started(&meta, "old").await.expect("pods"));
    }

    /// Deleting a Job deletes it in the foreground with the default grace period, a Job already
    /// gone is fine, and any other refusal is an error
    #[tokio::test]
    async fn delete_removes_a_job_in_the_foreground() {
        // a Job that can be deleted
        let path = "/apis/batch/v1/namespaces/thorium/jobs/old";
        let job = json!({"apiVersion": "batch/v1", "kind": "Job", "metadata": {"name": "old"}});
        let fake = FakeKube::default().route("DELETE", path, 200, job);
        let meta = meta_for(namespaced_cluster(full_spec()), &fake.client());
        delete(&meta, "old").await.expect("delete");
        // one delete of that Job, in the foreground
        let deletes: Vec<_> = fake
            .writes()
            .into_iter()
            .filter(|request| request.method == "DELETE")
            .collect();
        assert_eq!(deletes.len(), 1, "{deletes:?}");
        assert_eq!(deletes[0].path, path);
        assert_eq!(deletes[0].body["propagationPolicy"], "Foreground");
        assert!(
            deletes[0].body.get("gracePeriodSeconds").is_none(),
            "{deletes:?}"
        );
        // a Job that is already gone is not an error
        let missing = FakeKube::default().route("DELETE", path, 404, kube_status(404, "NotFound"));
        let meta = meta_for(namespaced_cluster(full_spec()), &missing.client());
        delete(&meta, "old").await.expect("already gone");
        // a delete the API refuses for any other reason fails
        for (code, reason) in [
            (403, "Forbidden"),
            (409, "Conflict"),
            (500, "InternalError"),
        ] {
            let refused =
                FakeKube::default().route("DELETE", path, code, kube_status(code, reason));
            let meta = meta_for(namespaced_cluster(full_spec()), &refused.client());
            assert!(delete(&meta, "old").await.is_err(), "{code}");
        }
    }

    /// A delete pinned to the version a Job was read at sends the Job's UID and resource version
    /// with foreground propagation, and tells a changed Job and a gone one apart from a delete
    #[tokio::test]
    async fn a_pinned_delete_names_the_version_it_read() {
        // a Job read at version 7
        let path = "/apis/batch/v1/namespaces/thorium/jobs/old";
        let job: Job = serde_json::from_value(json!({"apiVersion": "batch/v1", "kind": "Job",
            "metadata": {"name": "old", "uid": "uid-old", "resourceVersion": "7"}}))
        .expect("a Job");
        let answer = json!({"apiVersion": "batch/v1", "kind": "Job", "metadata": {"name": "old"}});
        let fake = FakeKube::default().route("DELETE", path, 200, answer);
        let meta = meta_for(namespaced_cluster(full_spec()), &fake.client());
        assert_eq!(
            delete_unchanged(&meta, &job).await.expect("delete"),
            Pinned::Deleting
        );
        // one delete of that Job, in the foreground, pinned to its UID and version
        let deletes: Vec<_> = fake
            .writes()
            .into_iter()
            .filter(|request| request.method == "DELETE")
            .collect();
        assert_eq!(deletes.len(), 1, "{deletes:?}");
        assert_eq!(deletes[0].path, path);
        assert_eq!(deletes[0].body["propagationPolicy"], "Foreground");
        assert_eq!(deletes[0].body["preconditions"]["uid"], "uid-old");
        assert_eq!(deletes[0].body["preconditions"]["resourceVersion"], "7");
        // a Job that changed since it was read conflicts, and one that is gone is not found
        for (code, reason, pinned) in [
            (409, "Conflict", Pinned::Changed),
            (404, "NotFound", Pinned::Gone),
        ] {
            let answered =
                FakeKube::default().route("DELETE", path, code, kube_status(code, reason));
            let meta = meta_for(namespaced_cluster(full_spec()), &answered.client());
            assert_eq!(
                delete_unchanged(&meta, &job).await.expect("answered"),
                pinned,
                "{code}"
            );
        }
        // a delete the API refuses for any other reason fails
        for (code, reason) in [(403, "Forbidden"), (500, "InternalError")] {
            let refused =
                FakeKube::default().route("DELETE", path, code, kube_status(code, reason));
            let meta = meta_for(namespaced_cluster(full_spec()), &refused.client());
            assert!(delete_unchanged(&meta, &job).await.is_err(), "{code}");
        }
    }

    /// Job names differ per attempt and trigger, start with the kind and step, and fit a name
    #[test]
    fn job_names_are_per_attempt() {
        // the same parts give the same name, and any other part another
        let first = job_name(
            JobKind::SchemaApply,
            "schema-apply",
            &["2026-10-v02", "0", "key"],
        );
        assert_eq!(
            first,
            job_name(
                JobKind::SchemaApply,
                "schema-apply",
                &["2026-10-v02", "0", "key"]
            )
        );
        assert_ne!(
            first,
            job_name(
                JobKind::SchemaApply,
                "schema-apply",
                &["2026-10-v02", "1", "key"]
            )
        );
        assert_ne!(
            first,
            job_name(
                JobKind::SchemaApply,
                "schema-apply",
                &["2026-10-v02", "0", "other"]
            )
        );
        // the name is the kind, the step, and 12 hex characters, within the 63 a Job allows
        assert!(first.starts_with("schema-apply-schema-apply-"), "{first}");
        assert_eq!(first.len(), "schema-apply-schema-apply-".len() + 12);
        assert!(first.len() <= 63);
    }

    /// A Job is active until a terminal condition holds, and while it is being deleted
    #[test]
    fn job_phase_reads_conditions() {
        // build a Job with a list of conditions
        let job = |conditions: serde_json::Value, deleting: bool| -> Job {
            let mut raw =
                json!({"metadata": {"name": "job"}, "status": {"conditions": conditions}});
            if deleting {
                raw["metadata"]["deletionTimestamp"] = json!("2026-10-21T00:00:00Z");
            }
            serde_json::from_value(raw).expect("job")
        };
        // no condition, or one that doesn't hold, is active
        assert_eq!(phase(&job(json!([]), false)), JobPhase::Active);
        assert_eq!(
            phase(&job(
                json!([{"type": "Complete", "status": "False"}]),
                false
            )),
            JobPhase::Active
        );
        // a held Complete condition is complete, unless the Job is being deleted
        let complete = json!([{"type": "Complete", "status": "True"}]);
        assert_eq!(phase(&job(complete.clone(), false)), JobPhase::Complete);
        assert_eq!(phase(&job(complete, true)), JobPhase::Active);
        // a held Failed condition carries its reason and message
        let failed = json!([{
            "type": "Failed",
            "status": "True",
            "reason": "BackoffLimitExceeded",
            "message": "Job has reached the specified backoff limit",
            "lastTransitionTime": "2026-10-21T00:00:00Z"
        }]);
        let JobPhase::Failed { reason, at } = phase(&job(failed, false)) else {
            panic!("the Job should have failed");
        };
        assert_eq!(
            reason,
            "BackoffLimitExceeded: Job has reached the specified backoff limit"
        );
        assert!(at.is_some());
    }
}
