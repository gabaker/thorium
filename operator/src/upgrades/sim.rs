//! A stateful fake of the Kubernetes API for the tests that follow an upgrade across reconciles
//!
//! It keeps the upgrade state `ConfigMap`, the Jobs the operator creates and their pods, the
//! Deployments' replica counts, the results thoradm's Jobs leave, and the cluster's status, and
//! answers the requests the operator makes from them. A test decides how each Job ends.

use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use thorium::models::schema::{CatalogIdentity, RESULT_PREFIX};
use thorium::models::upgrades::UpgradeState;

use crate::k8s::clusters::tests::{FakeKube, FakeRequest, kube_status};
use crate::k8s::crds::{ThoriumCluster, ThoriumClusterStatus};

/// The path of the upgrade state `ConfigMap`
const STATE_PATH: &str = "/api/v1/namespaces/thorium/configmaps/thorium-upgrade-state";

/// The path `ConfigMaps` are created at
const CONFIG_MAPS_PATH: &str = "/api/v1/namespaces/thorium/configmaps";

/// The path Jobs are listed and created at
const JOBS_PATH: &str = "/apis/batch/v1/namespaces/thorium/jobs";

/// The path pods are listed at
const PODS_PATH: &str = "/api/v1/namespaces/thorium/pods";

/// The path Deployments are created and read at
const DEPLOYMENTS_PATH: &str = "/apis/apps/v1/namespaces/thorium/deployments";

/// The path of the cluster's status subresource
const STATUS_PATH: &str = "/apis/sandia.gov/v1/namespaces/thorium/thoriumclusters/thorium/status";

/// The path of the cluster itself
const CLUSTER_PATH: &str = "/apis/sandia.gov/v1/namespaces/thorium/thoriumclusters/thorium";

/// The path of the cluster's Namespace
const NAMESPACE_PATH: &str = "/api/v1/namespaces/thorium";

/// How a Job ends, as a test decides
#[derive(Debug, Clone)]
pub(crate) enum Finish {
    /// The Job completes, leaving this termination summary
    Complete(Value),
    /// The Job fails, with its condition's reason, the summary its pod left if it got that far,
    /// and the number of log lines its pod wrote
    Failed {
        /// The failure condition's reason
        reason: String,
        /// The termination summary, when the pod wrote one
        summary: Option<Value>,
        /// How many lines the pod logged, numbered from 1
        log_lines: usize,
    },
}

/// What the fake keeps
#[derive(Default)]
struct World {
    /// The state `ConfigMap`: its resource version and the state as JSON text
    state: Option<(u64, String)>,
    /// The Jobs, as the API returns them
    jobs: Vec<Value>,
    /// The Jobs in the order they were created, as they were posted
    created: Vec<Value>,
    /// The pods, as the API returns them
    pods: Vec<Value>,
    /// The log of each pod by name
    logs: BTreeMap<String, String>,
    /// The Deployments: name to the replicas their spec asks for and their status reports
    deployments: BTreeMap<String, (i32, i32)>,
    /// The Deployments whose pods never stop, whatever their spec asks for
    stuck: BTreeSet<String>,
    /// The results thoradm's Jobs left, by the Job's UID
    results: BTreeMap<String, Value>,
    /// The Jobs a delete leaves terminating, until the test reaps them
    terminating: BTreeSet<String>,
    /// Whether a delete leaves the Job terminating rather than removing it at once
    slow_delete: bool,
    /// The Jobs that change right before their next delete, as when one ends between the operator's
    /// list and its delete
    change_before_delete: BTreeSet<String>,
    /// The kinds whose Jobs complete the moment they are created, with their summaries
    auto: BTreeMap<String, Value>,
    /// The catalog identity status Jobs report, when it isn't the operator's
    catalog: Option<CatalogIdentity>,
    /// The record status Jobs report, when it isn't a namespace without a keyspace
    record: Option<Value>,
    /// The cluster's status
    status: Value,
    /// The cluster itself, which every status patch answers with
    cluster: Value,
    /// Every Deployment patch, in order, as `(name, replicas)`
    scaled: Vec<(String, i32)>,
    /// The labels on the cluster's Namespace
    namespace_labels: BTreeMap<String, String>,
}

/// A stateful fake of the Kubernetes API
#[derive(Clone)]
pub(crate) struct Sim {
    /// What the fake keeps
    world: Arc<Mutex<World>>,
    /// The fake kube API that answers from it
    fake: FakeKube,
}

/// Read a label selector's pairs from a request's query
///
/// # Arguments
///
/// * `request` - The request
fn selector(request: &FakeRequest) -> Vec<(String, String)> {
    // `a=b,c=d`
    request
        .query_param("labelSelector")
        .unwrap_or_default()
        .split(',')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

/// Whether an object carries every label of a selector
///
/// # Arguments
///
/// * `object` - The object as JSON
/// * `selector` - The labels it must carry
fn matches(object: &Value, selector: &[(String, String)]) -> bool {
    // every pair must be among the object's labels
    selector
        .iter()
        .all(|(key, value)| object["metadata"]["labels"][key].as_str() == Some(value.as_str()))
}

/// Build a list as the kube API returns it
///
/// # Arguments
///
/// * `kind` - The list's kind
/// * `items` - The items
fn list(kind: &str, items: &[Value]) -> Value {
    json!({"apiVersion": "v1", "kind": kind, "metadata": {}, "items": items})
}

impl World {
    /// Create a Job from its body, with a pod while it runs
    ///
    /// # Arguments
    ///
    /// * `body` - The Job as posted
    fn create_job(&mut self, body: &Value) -> Value {
        // the API adds a UID and a creation time
        let job_name = body["metadata"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let mut job = body.clone();
        job["metadata"]["uid"] = json!(format!("uid-{job_name}"));
        job["metadata"]["resourceVersion"] = json!("1");
        job["metadata"]["creationTimestamp"] = json!(chrono::Utc::now().to_rfc3339());
        job["status"] = json!({});
        self.created.push(body.clone());
        // its pod, which runs until the test ends the Job
        let labels = body["metadata"]["labels"].clone();
        let mut pod_labels = labels.as_object().cloned().unwrap_or_default();
        pod_labels.insert("job-name".to_owned(), json!(job_name));
        let pod = json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": format!("{job_name}-pod"), "namespace": "thorium", "labels": pod_labels,
                         "creationTimestamp": chrono::Utc::now().to_rfc3339()},
            "status": {"phase": "Running"},
        });
        self.pods.push(pod);
        self.jobs.push(job.clone());
        // a kind that completes at once does
        let kind = body["metadata"]["labels"]["thorium.sandia.gov/job-kind"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if let Some(summary) = self.auto.get(&kind).cloned() {
            self.end(&job_name, &Finish::Complete(summary));
        }
        job
    }

    /// End a Job: set its condition, and its pod's termination message and log
    ///
    /// # Arguments
    ///
    /// * `job_name` - The Job's name
    /// * `finish` - How it ends
    // the whole of a Job's end is kept together to read it in one place
    #[allow(clippy::too_many_lines)]
    fn end(&mut self, job_name: &str, finish: &Finish) {
        // the log of a status Job holds the report the operator reads
        let args = self
            .jobs
            .iter()
            .find(|job| job["metadata"]["name"] == job_name)
            .map_or(Value::Null, |job| {
                job["spec"]["template"]["spec"]["containers"][0]["args"].clone()
            });
        let is_status = self.jobs.iter().any(|job| {
            job["metadata"]["name"] == job_name
                && job["metadata"]["labels"]["thorium.sandia.gov/job-kind"] == "schema-status"
        });
        let pod_name = format!("{job_name}-pod");
        let (condition, reason, message, log, exit) = match finish {
            Finish::Complete(summary) => {
                let mut log = String::new();
                if is_status {
                    // `--result <uid>` asks for the check report that Job left
                    let uid = args
                        .as_array()
                        .and_then(|args| {
                            args.iter()
                                .position(|arg| arg == "--result")
                                .and_then(|at| args.get(at + 1))
                        })
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    let check = uid
                        .as_ref()
                        .and_then(|uid| self.results.get(uid))
                        .cloned()
                        .unwrap_or(Value::Null);
                    let catalog = self
                        .catalog
                        .clone()
                        .and_then(|catalog| serde_json::to_value(catalog).ok())
                        .unwrap_or_else(|| {
                            serde_json::to_value(thorium::models::schema::catalog_identity(
                                &super::catalog(),
                            ))
                            .expect("identity serializes")
                        });
                    let status = self
                        .record
                        .clone()
                        .unwrap_or_else(|| json!({"state": "no_keyspace"}));
                    let report = json!({
                        "status": status,
                        "catalog": catalog,
                        "check": check,
                    });
                    log = format!("{RESULT_PREFIX}{report}\n");
                }
                (
                    "Complete",
                    "Completed".to_owned(),
                    summary.to_string(),
                    log,
                    0,
                )
            }
            Finish::Failed {
                reason,
                summary,
                log_lines,
            } => {
                let log = (1..=*log_lines).fold(String::new(), |mut log, n| {
                    let _ = writeln!(log, "line {n}");
                    log
                });
                (
                    "Failed",
                    reason.clone(),
                    summary.as_ref().map_or(String::new(), ToString::to_string),
                    log,
                    1,
                )
            }
        };
        for job in &mut self.jobs {
            if job["metadata"]["name"] == job_name {
                bump(job);
                job["status"] = json!({"conditions": [{
                    "type": condition,
                    "status": "True",
                    "reason": reason,
                    "message": "fake",
                    "lastTransitionTime": chrono::Utc::now().to_rfc3339(),
                }]});
            }
        }
        for pod in &mut self.pods {
            if pod["metadata"]["name"] == pod_name {
                pod["status"] = json!({
                    "phase": if condition == "Complete" { "Succeeded" } else { "Failed" },
                    "containerStatuses": [{
                        "name": "thoradm",
                        "ready": false,
                        "restartCount": 0,
                        "image": "registry/thorium:1.8.1",
                        "imageID": "",
                        "state": {"terminated": {"exitCode": exit, "message": message}},
                    }],
                });
            }
        }
        self.logs.insert(pod_name, log);
    }

    /// Remove a Job and its pods
    ///
    /// # Arguments
    ///
    /// * `job_name` - The Job's name
    fn remove_job(&mut self, job_name: &str) {
        // the Job and every pod it made
        self.jobs.retain(|job| job["metadata"]["name"] != job_name);
        self.pods
            .retain(|pod| pod["metadata"]["labels"]["job-name"] != job_name);
        self.terminating.remove(job_name);
    }

    /// Answer a request
    ///
    /// # Arguments
    ///
    /// * `request` - The request
    // every route the operator uses is answered here, so they read as one table
    #[allow(clippy::too_many_lines)]
    fn handle(&mut self, request: &FakeRequest) -> Option<(u16, Value)> {
        let path = request.path.as_str();
        match (request.method.as_str(), path) {
            ("GET", STATE_PATH) => Some(match &self.state {
                Some((version, state)) => (200, state_map(*version, state)),
                None => (404, kube_status(404, "NotFound")),
            }),
            ("PUT", STATE_PATH) => {
                let version = request.body["metadata"]["resourceVersion"]
                    .as_str()
                    .and_then(|version| version.parse::<u64>().ok());
                let current = self.state.as_ref().map(|(version, _)| *version);
                // a replacement pinned to a stale version conflicts
                if version.is_some() && version != current {
                    return Some((409, kube_status(409, "Conflict")));
                }
                let next = current.unwrap_or(0) + 1;
                let text = request.body["data"]["state.json"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                self.state = Some((next, text.clone()));
                Some((200, state_map(next, &text)))
            }
            ("POST", CONFIG_MAPS_PATH) => {
                let text = request.body["data"]["state.json"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                self.state = Some((1, text.clone()));
                Some((201, state_map(1, &text)))
            }
            ("POST", JOBS_PATH) => {
                let job_name = request.body["metadata"]["name"]
                    .as_str()
                    .unwrap_or_default();
                // a Job still there with this name conflicts
                if self
                    .jobs
                    .iter()
                    .any(|job| job["metadata"]["name"] == job_name)
                {
                    return Some((409, kube_status(409, "AlreadyExists")));
                }
                Some((201, self.create_job(&request.body)))
            }
            ("GET", JOBS_PATH) => {
                let wanted = selector(request);
                let items: Vec<Value> = self
                    .jobs
                    .iter()
                    .filter(|job| matches(job, &wanted))
                    .cloned()
                    .collect();
                Some((200, list("JobList", &items)))
            }
            ("DELETE", _) if path.starts_with(&format!("{JOBS_PATH}/")) => {
                let job_name = path.rsplit('/').next().unwrap_or_default().to_owned();
                if !self
                    .jobs
                    .iter()
                    .any(|job| job["metadata"]["name"] == job_name)
                {
                    return Some((404, kube_status(404, "NotFound")));
                }
                // a Job that changes right before the delete moves past the version it was read at
                if self.change_before_delete.remove(&job_name) {
                    for job in &mut self.jobs {
                        if job["metadata"]["name"] == job_name {
                            bump(job);
                        }
                    }
                }
                // a delete pinned to a version or UID the Job no longer has conflicts
                let job = self
                    .jobs
                    .iter()
                    .find(|job| job["metadata"]["name"] == job_name)
                    .cloned()
                    .unwrap_or_default();
                let preconditions = &request.body["preconditions"];
                for field in ["resourceVersion", "uid"] {
                    if let Some(pinned) = preconditions[field].as_str()
                        && job["metadata"][field].as_str() != Some(pinned)
                    {
                        return Some((409, kube_status(409, "Conflict")));
                    }
                }
                // a slow delete leaves the Job terminating until the test reaps it
                if self.slow_delete {
                    self.terminating.insert(job_name.clone());
                    for job in &mut self.jobs {
                        if job["metadata"]["name"] == job_name {
                            job["metadata"]["deletionTimestamp"] =
                                json!(chrono::Utc::now().to_rfc3339());
                        }
                    }
                } else {
                    self.remove_job(&job_name);
                }
                Some((
                    200,
                    json!({"kind": "Status", "apiVersion": "v1", "status": "Success"}),
                ))
            }
            ("GET", PODS_PATH) => {
                let wanted = selector(request);
                let items: Vec<Value> = self
                    .pods
                    .iter()
                    .filter(|pod| matches(pod, &wanted))
                    .cloned()
                    .collect();
                Some((200, list("PodList", &items)))
            }
            ("GET", _) if path.starts_with(&format!("{PODS_PATH}/")) && path.ends_with("/log") => {
                let pod = path
                    .trim_start_matches(&format!("{PODS_PATH}/"))
                    .trim_end_matches("/log");
                Some((
                    200,
                    Value::String(self.logs.get(pod).cloned().unwrap_or_default()),
                ))
            }
            ("POST", DEPLOYMENTS_PATH) => {
                let name = request.body["metadata"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                // a Deployment that exists conflicts
                if self.deployments.contains_key(&name) {
                    return Some((409, kube_status(409, "AlreadyExists")));
                }
                let replicas = request.body["spec"]["replicas"]
                    .as_i64()
                    .and_then(|replicas| i32::try_from(replicas).ok())
                    .unwrap_or(1);
                self.deployments.insert(name.clone(), (replicas, replicas));
                self.scaled.push((name.clone(), replicas));
                Some((201, deployment(&name, replicas, replicas)))
            }
            ("GET", _) if path.starts_with(&format!("{DEPLOYMENTS_PATH}/")) => {
                let name = path.rsplit('/').next().unwrap_or_default();
                Some(match self.deployments.get(name) {
                    Some((spec, status)) => (200, deployment(name, *spec, *status)),
                    None => (404, kube_status(404, "NotFound")),
                })
            }
            ("PATCH", _) if path.starts_with(&format!("{DEPLOYMENTS_PATH}/")) => {
                let name = path.rsplit('/').next().unwrap_or_default().to_owned();
                if let Some(replicas) = request.body["spec"]["replicas"].as_i64() {
                    let replicas = i32::try_from(replicas).unwrap_or_default();
                    let stuck = self.stuck.contains(&name);
                    let entry = self
                        .deployments
                        .entry(name.clone())
                        .or_insert((replicas, replicas));
                    entry.0 = replicas;
                    // pods follow the spec at once unless they are stuck
                    if !stuck {
                        entry.1 = replicas;
                    }
                    self.scaled.push((name.clone(), replicas));
                }
                let (spec, status) = self.deployments.get(&name).copied().unwrap_or_default();
                Some((200, deployment(&name, spec, status)))
            }
            ("GET", NAMESPACE_PATH) => Some((
                200,
                json!({"apiVersion": "v1", "kind": "Namespace",
                       "metadata": {"name": "thorium", "labels": self.namespace_labels}}),
            )),
            ("PATCH", CLUSTER_PATH) => {
                json_patch::merge(&mut self.cluster, &request.body);
                Some((200, self.cluster.clone()))
            }
            ("PATCH", STATUS_PATH) => {
                json_patch::merge(&mut self.status, &request.body["status"]);
                let mut cluster = self.cluster.clone();
                cluster["status"] = self.status.clone();
                Some((200, cluster))
            }
            _ => None,
        }
    }
}

/// Move a Job to its next resource version, as every change of an object does
///
/// # Arguments
///
/// * `job` - The Job as JSON
fn bump(job: &mut Value) {
    // the version is a number held as text
    let version = job["metadata"]["resourceVersion"]
        .as_str()
        .and_then(|version| version.parse::<u64>().ok())
        .unwrap_or(1);
    job["metadata"]["resourceVersion"] = json!((version + 1).to_string());
}

/// Build the state `ConfigMap` as the API returns it
///
/// # Arguments
///
/// * `version` - Its resource version
/// * `state` - The state as JSON text
fn state_map(version: u64, state: &str) -> Value {
    // the state sits under its key
    json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {"name": "thorium-upgrade-state", "namespace": "thorium",
                     "resourceVersion": version.to_string()},
        "data": {"state.json": state},
    })
}

/// Build a Deployment as the API returns it
///
/// # Arguments
///
/// * `name` - Its name
/// * `spec` - The replicas its spec asks for
/// * `status` - The replicas its status reports
fn deployment(name: &str, spec: i32, status: i32) -> Value {
    // a Deployment with a replica count and a status
    json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {"name": name, "namespace": "thorium"},
        "spec": {"replicas": spec, "selector": {}, "template": {}},
        "status": {"replicas": status, "readyReplicas": status},
    })
}

impl Sim {
    /// Build a fake for a cluster
    ///
    /// # Arguments
    ///
    /// * `cluster` - The cluster the tests upgrade
    pub(crate) fn new(cluster: &ThoriumCluster) -> Sim {
        // answer every request from what the world keeps
        let world = Arc::new(Mutex::new(World {
            cluster: serde_json::to_value(cluster).expect("cluster serializes"),
            status: json!({}),
            ..World::default()
        }));
        let handled = world.clone();
        let fake = FakeKube::default()
            .with_handler(move |request| handled.lock().expect("world lock").handle(request));
        Sim { world, fake }
    }

    /// Get the fake kube API
    pub(crate) fn fake(&self) -> &FakeKube {
        // the one that answers from the world
        &self.fake
    }

    /// Set the recorded upgrade state
    ///
    /// # Arguments
    ///
    /// * `state` - The state the `ConfigMap` holds
    pub(crate) fn set_state(&self, state: &UpgradeState) {
        // the state as the operator stores it
        let text = serde_json::to_string(state).expect("state serializes");
        let mut world = self.world.lock().expect("world lock");
        let version = world.state.as_ref().map_or(5, |(version, _)| version + 1);
        world.state = Some((version, text));
    }

    /// Get the recorded upgrade state
    pub(crate) fn state(&self) -> Option<UpgradeState> {
        // parse the `ConfigMap`'s state
        let world = self.world.lock().expect("world lock");
        world
            .state
            .as_ref()
            .map(|(_, text)| serde_json::from_str(text).expect("state parses"))
    }

    /// Set the labels on the cluster's Namespace
    ///
    /// # Arguments
    ///
    /// * `labels` - The labels the Namespace carries
    pub(crate) fn set_namespace_labels(&self, labels: &[(&str, &str)]) {
        // the fake answers Namespace reads with them
        self.world.lock().expect("world lock").namespace_labels = labels
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
    }

    /// Replace the cluster, as an admin changing its spec does
    ///
    /// # Arguments
    ///
    /// * `cluster` - The cluster with its new spec
    pub(crate) fn set_cluster(&self, cluster: &ThoriumCluster) {
        // the spec the operator reads and patches from now on
        self.world.lock().expect("world lock").cluster =
            serde_json::to_value(cluster).expect("cluster serializes");
    }

    /// Get the cluster as its spec stands after the operator's patches
    pub(crate) fn cluster(&self) -> ThoriumCluster {
        // parse the merged cluster
        let world = self.world.lock().expect("world lock");
        serde_json::from_value(world.cluster.clone()).expect("the cluster parses")
    }

    /// Get the cluster's status as JSON, as the API server holds it after every patch
    pub(crate) fn status_value(&self) -> Value {
        // the merged status
        self.world.lock().expect("world lock").status.clone()
    }

    /// Get the state `ConfigMap`'s text as written
    pub(crate) fn state_text(&self) -> Option<String> {
        // the JSON the operator stored
        self.world
            .lock()
            .expect("world lock")
            .state
            .as_ref()
            .map(|(_, text)| text.clone())
    }

    /// Get the cluster's status as the operator reported it
    pub(crate) fn status(&self) -> ThoriumClusterStatus {
        // parse the merged status
        let world = self.world.lock().expect("world lock");
        serde_json::from_value(world.status.clone()).unwrap_or_default()
    }

    /// Make Jobs of a kind complete the moment they are created
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind label
    /// * `summary` - The termination summary they leave
    pub(crate) fn auto_complete(&self, kind: &str, summary: Value) {
        // remember the kind
        self.world
            .lock()
            .expect("world lock")
            .auto
            .insert(kind.to_owned(), summary);
    }

    /// Set the catalog status Jobs report, when it differs from the operator's
    ///
    /// # Arguments
    ///
    /// * `identity` - The identity thoradm's catalog has
    pub(crate) fn set_catalog(&self, identity: CatalogIdentity) {
        // status Jobs report it from now on
        self.world.lock().expect("world lock").catalog = Some(identity);
    }

    /// Set the record status Jobs report
    ///
    /// # Arguments
    ///
    /// * `status` - The record status as JSON
    pub(crate) fn set_record(&self, status: Value) {
        // status Jobs report it from now on
        self.world.lock().expect("world lock").record = Some(status);
    }

    /// Get the names of the Jobs of a kind that haven't ended
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind label
    pub(crate) fn active(&self, kind: &str) -> Vec<String> {
        // the Jobs of the kind with no terminal condition
        let world = self.world.lock().expect("world lock");
        world
            .jobs
            .iter()
            .filter(|job| job["metadata"]["labels"]["thorium.sandia.gov/job-kind"] == kind)
            .filter(|job| {
                job["status"]["conditions"]
                    .as_array()
                    .is_none_or(Vec::is_empty)
            })
            .filter_map(|job| job["metadata"]["name"].as_str().map(ToOwned::to_owned))
            .collect()
    }

    /// Make a delete leave the Job terminating until it is reaped
    pub(crate) fn slow_delete(&self) {
        // deletes keep the Job and its pod
        self.world.lock().expect("world lock").slow_delete = true;
    }

    /// Change a Job right before its next delete, so a delete pinned to the version the operator
    /// read conflicts
    ///
    /// # Arguments
    ///
    /// * `job` - The Job's name
    pub(crate) fn change_before_delete(&self, job: &str) {
        // the next delete of the Job finds it at a newer version
        self.world
            .lock()
            .expect("world lock")
            .change_before_delete
            .insert(job.to_owned());
    }

    /// Leave a Job terminating as a delete would, without the operator deleting it, as when an
    /// operator deleted it and restarted before recording the stop
    ///
    /// # Arguments
    ///
    /// * `job` - The Job's name
    pub(crate) fn terminate(&self, job: &str) {
        // the Job stays listed with a deletion timestamp until it is reaped
        let mut world = self.world.lock().expect("world lock");
        world.terminating.insert(job.to_owned());
        for found in &mut world.jobs {
            if found["metadata"]["name"] == job {
                found["metadata"]["deletionTimestamp"] = json!(chrono::Utc::now().to_rfc3339());
            }
        }
    }

    /// Finish a terminating Job's deletion, as the API does once its pod is gone
    ///
    /// # Arguments
    ///
    /// * `job` - The Job's name
    pub(crate) fn reap(&self, job: &str) {
        // the Job and its pods go
        self.world.lock().expect("world lock").remove_job(job);
    }

    /// Remove a Job and leave its pods, as when a Job is deleted while its pod is still shutting down
    ///
    /// # Arguments
    ///
    /// * `job` - The Job's name
    pub(crate) fn orphan(&self, job: &str) {
        // only the Job object goes, so the pod it made is still listed
        let mut world = self.world.lock().expect("world lock");
        world.jobs.retain(|found| found["metadata"]["name"] != job);
    }

    /// Get the names of the Jobs of a kind that exist, oldest first
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind label
    pub(crate) fn jobs(&self, kind: &str) -> Vec<String> {
        // the Jobs carrying the kind label
        let world = self.world.lock().expect("world lock");
        world
            .jobs
            .iter()
            .filter(|job| job["metadata"]["labels"]["thorium.sandia.gov/job-kind"] == kind)
            .filter_map(|job| job["metadata"]["name"].as_str().map(ToOwned::to_owned))
            .collect()
    }

    /// Get every Job created, as posted, in order
    pub(crate) fn created(&self) -> Vec<Value> {
        // the bodies of the creates
        self.world.lock().expect("world lock").created.clone()
    }

    /// Get the Jobs of a kind that were created, as posted
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind label
    pub(crate) fn created_of(&self, kind: &str) -> Vec<Value> {
        // keep the creates of the kind
        self.created()
            .into_iter()
            .filter(|job| job["metadata"]["labels"]["thorium.sandia.gov/job-kind"] == kind)
            .collect()
    }

    /// End a Job
    ///
    /// # Arguments
    ///
    /// * `job` - The Job's name
    /// * `finish` - How it ends
    pub(crate) fn finish(&self, job: &str, finish: &Finish) {
        // set its condition and its pod's message and log
        self.world.lock().expect("world lock").end(job, finish);
    }

    /// Leave the result a check Job wrote, which a status Job asked for the Job's UID returns
    ///
    /// # Arguments
    ///
    /// * `job` - The check Job's name
    /// * `report` - The report in its result row
    pub(crate) fn leave_result(&self, job: &str, report: Value) {
        // keyed by the Job's UID
        self.world
            .lock()
            .expect("world lock")
            .results
            .insert(format!("uid-{job}"), report);
    }

    /// Add a Deployment with a replica count
    ///
    /// # Arguments
    ///
    /// * `name` - The Deployment's name
    /// * `replicas` - Its replica count
    pub(crate) fn add_deployment(&self, name: &str, replicas: i32) {
        // the spec and the status agree
        self.world
            .lock()
            .expect("world lock")
            .deployments
            .insert(name.to_owned(), (replicas, replicas));
    }

    /// Make a Deployment's pods stay up whatever its spec asks for
    ///
    /// # Arguments
    ///
    /// * `name` - The Deployment's name
    pub(crate) fn stick(&self, name: &str) {
        // its status stops following its spec
        self.world
            .lock()
            .expect("world lock")
            .stuck
            .insert(name.to_owned());
    }

    /// Let a stuck Deployment's pods stop
    ///
    /// # Arguments
    ///
    /// * `name` - The Deployment's name
    pub(crate) fn unstick(&self, name: &str) {
        // its status catches up with its spec
        let mut world = self.world.lock().expect("world lock");
        world.stuck.remove(name);
        if let Some(entry) = world.deployments.get_mut(name) {
            entry.1 = entry.0;
        }
    }

    /// Get the replicas a Deployment's spec asks for
    ///
    /// # Arguments
    ///
    /// * `name` - The Deployment's name
    pub(crate) fn replicas(&self, name: &str) -> Option<i32> {
        // the spec's count
        self.world
            .lock()
            .expect("world lock")
            .deployments
            .get(name)
            .map(|(spec, _)| *spec)
    }

    /// Get every scale patch, in order, as `(name, replicas)`
    pub(crate) fn scaled(&self) -> Vec<(String, i32)> {
        // the patches the fake saw
        self.world.lock().expect("world lock").scaled.clone()
    }
}
