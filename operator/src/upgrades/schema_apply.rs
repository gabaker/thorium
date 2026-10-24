//! The schema-apply Job, which converges Scylla to the deployed image's schema before any
//! component is created or rolled out
//!
//! The Job runs `thoradm schema apply` whenever its trigger key differs from the key recorded
//! with the last successful apply: the image reference, the revision, and hashes of the settings
//! that shape stored data, of the table TTLs, and of the files password. A fresh install has no
//! recorded key, so it always runs. A retry annotation value other than the last one used runs it
//! once more, also for a recorded key. The operator records the fingerprint the Job reports, not
//! the one its own build declares, together with the key, so an operator and image built from
//! different sources run the Job once and report the difference as a `SchemaSkew` condition
//! instead of running it again.

use chrono::{DateTime, Utc};
use k8s_openapi::api::batch::v1::Job;
use kube::runtime::controller::Action;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Duration;
use thorium::Error;
use thorium::conf::Conf;
use thorium::models::schema::{SchemaObject, fingerprints};
use thorium::models::upgrades::{
    APPLY_KEY_MARKER, DEPLOYMENT_MARKER, HELD_FAILURE_MARKER, JobFailure, RETRY_VALUE_MARKER,
    RevisionId, UpgradeConditionType, UpgradeState,
};

use super::Pending;
use super::rebucket::{self, ATTEMPT_FLOOR_MARKER};
use super::state::{self, StoredState};
use crate::k8s::clusters::ClusterMeta;
use crate::k8s::crds::{self, ClusterPhase, ThoriumCluster};
use crate::k8s::jobs::{self, JobKind, JobPhase, JobSpec, JobState};

/// The annotation on a `ThoriumCluster` that runs the schema-apply Job once more: each value
/// other than the last one an attempt used starts one attempt, after a failure or over an applied
/// trigger key alike (after pulling a new image under a mutable tag)
pub const RETRY_ANNOTATION: &str = "thorium.sandia.gov/retry-schema-apply";

/// The step id the schema-apply Job runs, set as its step label
pub const STEP: &str = "schema-apply";

/// The annotation on a schema-apply Job holding the trigger key it applies for
const TRIGGER_KEY_ANNOTATION: &str = "thorium.sandia.gov/trigger-key";

/// The annotation on a schema-apply Job holding its attempt counter for its trigger key
const ATTEMPT_ANNOTATION: &str = "thorium.sandia.gov/attempt";

/// The annotation on a schema-apply Job holding the revision it applies for
const REVISION_ANNOTATION: &str = "thorium.sandia.gov/revision";

/// The annotation on a schema-apply Job holding the cluster's retry annotation value when it was
/// created, so that value forces no second retry
const RETRY_VALUE_ANNOTATION: &str = "thorium.sandia.gov/retry-value";

/// The annotation on a schema-apply Job holding the trigger key recorded with the last
/// successful apply when it was created, so a Job left from before another key was applied is
/// never taken for an apply over the current state
const APPLIED_OVER_ANNOTATION: &str = "thorium.sandia.gov/applied-over";

/// The environment variable passing the trigger key to `thoradm schema apply`, which reports
/// and records it with the fingerprint
pub const TRIGGER_KEY_ENV: &str = "THORIUM_SCHEMA_APPLY_KEY";

/// How long in seconds to wait before checking on a running schema-apply Job
const RUNNING_REQUEUE_SECS: u64 = 15;

/// How long in seconds to wait before checking on a failed schema-apply Job again; it is never
/// retried by itself, and a new retry annotation value or trigger key wakes the reconcile sooner
const HELD_REQUEUE_SECS: u64 = 300;

/// How long `thoradm schema apply` waits for Scylla to accept connections, in seconds
const WAIT_FOR_SCYLLA_SECS: &str = "300";

/// The settings that give a table its default TTL, which `schema apply` applies as config changes
///
/// The events retention is left out: nothing reads the events table any more, so a change to it
/// would run the Job for nothing.
const TTL_SETTINGS: [&str; 2] = ["thorium.retention.logs", "thorium.retention.notifications"];

/// A failed schema-apply attempt that holds the rollout, recorded in the upgrade state so it
/// keeps holding after its Job is deleted
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct HeldFailure {
    /// The trigger key the attempt was made for
    key: String,
    /// The attempt counter for that key
    attempt: u32,
    /// The failed Job's name
    job: String,
    /// Why the Job failed
    reason: String,
    /// The settings the apply refused because they changed while their data exists, as config
    /// paths such as `thorium.files.partition_size`
    #[serde(default)]
    refused: Vec<String>,
}

/// Get the failed attempt the upgrade state records as holding the rollout for a trigger key
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
/// * `key` - The current trigger key
fn held_failure(state: &UpgradeState, key: &str) -> Option<HeldFailure> {
    // a marker that doesn't parse or names another key holds nothing for this key
    state
        .schema
        .get(HELD_FAILURE_MARKER)
        .and_then(|raw| serde_json::from_str::<HeldFailure>(raw).ok())
        .filter(|held| held.key == key)
}

/// Decide whether the cluster's retry annotation asks for a new schema-apply attempt
///
/// It does when it is set to a value other than the last one an attempt used. Values are
/// compared exactly: one differing only in whitespace or case, or an empty one, is a new value.
///
/// # Arguments
///
/// * `retry` - The value of the cluster's retry annotation, if it is set
/// * `last` - The last retry annotation value an attempt used, if any
fn retry_forces(retry: Option<&str>, last: Option<&str>) -> bool {
    // only a set annotation can ask, and only with a value not used last
    retry.is_some_and(|retry| last != Some(retry))
}

/// Hash `name=value` lines in order as lowercase hex
///
/// # Arguments
///
/// * `settings` - The settings and their values, in a fixed order
fn hash_settings(settings: &[(&str, String)]) -> String {
    // hash one line per setting so values can't run together
    let mut hasher = Sha256::new();
    for (name, value) in settings {
        hasher.update(format!("{name}={value}\n").as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

/// Hash the settings that decide where stored data lives: every partition size and `earliest`
///
/// The files password also shapes stored data; [`password_hash`] covers it.
///
/// # Arguments
///
/// * `conf` - The cluster's Thorium config
fn shaping_hash(conf: &Conf) -> String {
    // list every partition size and earliest value by its key path
    let thorium = &conf.thorium;
    let tags = &thorium.tags;
    let settings = [
        (
            "files.partition_size",
            thorium.files.partition_size.to_string(),
        ),
        ("files.earliest", thorium.files.earliest.to_string()),
        (
            "repos.partition_size",
            thorium.repos.partition_size.to_string(),
        ),
        ("repos.earliest", thorium.repos.earliest.to_string()),
        (
            "results.partition_size",
            thorium.results.partition_size.to_string(),
        ),
        ("results.earliest", thorium.results.earliest.to_string()),
        (
            "entities.partition_size",
            thorium.entities.partition_size.to_string(),
        ),
        ("entities.earliest", thorium.entities.earliest.to_string()),
        (
            "associations.partition_size",
            thorium.associations.partition_size.to_string(),
        ),
        (
            "associations.earliest",
            thorium.associations.earliest.to_string(),
        ),
        (
            "tags.files.partition_size",
            tags.files.partition_size.to_string(),
        ),
        ("tags.files.earliest", tags.files.earliest.to_string()),
        (
            "tags.repos.partition_size",
            tags.repos.partition_size.to_string(),
        ),
        ("tags.repos.earliest", tags.repos.earliest.to_string()),
        (
            "tags.entities.partition_size",
            tags.entities.partition_size.to_string(),
        ),
        ("tags.entities.earliest", tags.entities.earliest.to_string()),
    ];
    hash_settings(&settings)
}

/// Hash the settings that give tables their default TTL
///
/// # Arguments
///
/// * `conf` - The cluster's Thorium config
fn ttl_hash(conf: &Conf) -> String {
    // list each TTL setting with its configured value
    let settings: Vec<(&str, String)> = TTL_SETTINGS
        .iter()
        .map(|name| (*name, format!("{:?}", conf.table_ttl(name))))
        .collect();
    hash_settings(&settings)
}

/// Hash the files password as lowercase hex
///
/// The password encrypts every stored file, so a changed one must run `schema apply`, which
/// refuses it before any component rolls out. The password guards against running with the
/// wrong config rather than protecting data, so a plain sha256 of it in the trigger key is
/// enough; the key never holds the password itself.
///
/// # Arguments
///
/// * `conf` - The cluster's Thorium config
fn password_hash(conf: &Conf) -> String {
    // hash the password alone
    format!(
        "{:x}",
        Sha256::digest(conf.thorium.files.password.as_bytes())
    )
}

/// Build the schema-apply trigger key for a cluster at a revision
///
/// The key names the image reference and the revision as they are, so an admin can tell what
/// changed, followed by the hashes of the data-shaping and TTL settings and of the files
/// password.
///
/// # Arguments
///
/// * `cluster` - The cluster being applied
/// * `conf` - The cluster's Thorium config
/// * `revision` - The revision the cluster is at or moving to
#[must_use]
pub fn trigger_key(cluster: &ThoriumCluster, conf: &Conf, revision: &RevisionId) -> String {
    // join the parts that each trigger an apply when they change
    format!(
        "image={};revision={revision};shaping={};ttl={};password={}",
        cluster.get_image(),
        shaping_hash(conf),
        ttl_hash(conf),
        password_hash(conf)
    )
}

/// Decide whether an image tag is mutable, so a new image can be pushed under it
///
/// A tag is versioned when it starts with a digit, optionally after a `v` (`1.8.1`, `v1.8.1`,
/// `1.8.1-rc1`), or when it pins a digest (`@sha256:...`). Anything else, such as an empty tag,
/// `latest`, `main`, or `dev`, is mutable.
///
/// # Arguments
///
/// * `version` - The cluster's image version or tag
fn mutable_tag(version: &str) -> bool {
    // read the tag without surrounding whitespace
    let tag = version.trim();
    // a digest pins the image whatever its tag says
    if tag.contains('@') {
        return false;
    }
    // a version starts with a digit, optionally after a v
    let number = tag.strip_prefix(['v', 'V']).unwrap_or(tag);
    !number.starts_with(|first: char| first.is_ascii_digit())
}

/// Warn while the cluster runs a mutable image tag, since a new image pushed under it changes
/// no trigger key and so never runs the schema-apply Job by itself
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
async fn report_mutable_tag(meta: &ClusterMeta) {
    // check the cluster's tag and say what to do after pulling a new image
    let version = meta.cluster.get_version();
    let holds = mutable_tag(&version);
    let message = if holds {
        format!(
            "Image tag is mutable: schema apply only runs when its trigger key changes. Set \
             {RETRY_ANNOTATION} after pulling a new image."
        )
    } else {
        format!("Image tag {version} is versioned")
    };
    // raise the condition for a mutable tag and clear it for a versioned one
    crds::set_upgrade_condition(
        &meta.client,
        &meta.status,
        UpgradeConditionType::MutableImageTag,
        holds,
        message,
    )
    .await;
}

#[cfg(test)]
thread_local! {
    /// The declared deployment fingerprint tests give this operator's build on their thread
    pub(crate) static DECLARED_OVERRIDE: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// Get the deployment fingerprint of the schema this operator's code declares
///
/// This is the deployment fingerprint of the declared schema objects, the one `thoradm schema
/// apply` reports and records for an image built from the same source; a released revision's
/// committed snapshot only matches it for a release build. It is computed once per process.
/// Tests can inject another fingerprint on their thread.
#[must_use]
pub fn declared_fingerprint() -> String {
    // the declared schema doesn't change while the operator runs
    static DECLARED: OnceLock<String> = OnceLock::new();
    // a test's injected fingerprint stands in for the declared schema
    #[cfg(test)]
    if let Some(declared) = DECLARED_OVERRIDE.with(|declared| declared.borrow().clone()) {
        return declared;
    }
    // fingerprint the declared objects the way the schema apply does
    let declared = DECLARED.get_or_init(|| {
        fingerprints(&SchemaObject::declared())
            .0
            .as_str()
            .to_owned()
    });
    declared.clone()
}

/// Get the deployment fingerprint this operator's build declares
///
/// This is compared with the fingerprint the schema-apply Job reports, which is the declared
/// schema of the image's build rather than a released revision's committed snapshot.
#[must_use]
pub fn build_fingerprint() -> String {
    // the schema this build's code declares is what an image of the same source applies
    declared_fingerprint()
}

/// Get an annotation of a Job
///
/// # Arguments
///
/// * `job` - The Job to read
/// * `key` - The annotation to get
fn annotation<'a>(job: &'a Job, key: &str) -> Option<&'a str> {
    // look the annotation up in the Job's metadata
    job.metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(key))
        .map(String::as_str)
}

/// Get a schema-apply Job's attempt counter
///
/// # Arguments
///
/// * `job` - The Job to read
fn attempt(job: &Job) -> u32 {
    // a Job without a readable counter counts as the first attempt
    annotation(job, ATTEMPT_ANNOTATION)
        .and_then(|raw| raw.parse().ok())
        .unwrap_or_default()
}

/// Get the name of a Job
///
/// # Arguments
///
/// * `job` - The Job to name
fn name(job: &Job) -> String {
    // a Job read from the API always has a name
    job.metadata.name.clone().unwrap_or_default()
}

/// Get when a Job was created, if the API says
///
/// # Arguments
///
/// * `job` - The Job to read
fn created(job: &Job) -> Option<DateTime<Utc>> {
    // the API stamps every Job it stores
    job.metadata.creation_timestamp.as_ref().map(|time| time.0)
}

/// Decide whether a failed attempt still holds the rollout
///
/// A failed attempt is never retried by itself. The next one starts only when the cluster's
/// retry annotation has a value other than the last one an attempt used and this attempt didn't
/// use it, when the recorded apply changed since the attempt was made, or when the trigger key
/// changed since: a Job for another key was created after it.
///
/// # Arguments
///
/// * `job` - The latest attempt for the current trigger key, which failed
/// * `found` - Every schema-apply Job
/// * `applied_over` - The trigger key recorded with the last successful apply, if any
/// * `retry` - The value of the cluster's retry annotation, if it asks for a new attempt
fn failure_holds(
    job: &Job,
    found: &[Job],
    applied_over: Option<&str>,
    retry: Option<&str>,
) -> bool {
    // a retry annotation value this attempt didn't use asks for one more attempt
    if retry.is_some_and(|retry| annotation(job, RETRY_VALUE_ANNOTATION) != Some(retry)) {
        return false;
    }
    // an attempt made over another recorded apply says nothing about the current state
    if annotation(job, APPLIED_OVER_ANNOTATION) != applied_over {
        return false;
    }
    // a Job for another key created after the attempt means the key changed since
    let key = annotation(job, TRIGGER_KEY_ANNOTATION);
    let at = created(job);
    !found
        .iter()
        .any(|other| annotation(other, TRIGGER_KEY_ANNOTATION) != key && created(other) > at)
}

/// Build the schema-apply Job for an attempt
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `revision` - The revision the cluster is at or moving to
/// * `key` - The trigger key the Job applies for
/// * `attempt` - The attempt counter for this key
/// * `applied_over` - The trigger key recorded with the last successful apply, if any
pub fn spec(
    meta: &ClusterMeta,
    revision: &RevisionId,
    key: &str,
    attempt: u32,
    applied_over: Option<&str>,
) -> Result<JobSpec, Error> {
    // name the attempt from what triggered it, so a retry or a new key never reuses a name
    let attempt = attempt.to_string();
    let name = jobs::job_name(
        JobKind::SchemaApply,
        STEP,
        &[revision.as_str(), "", "", &attempt, key],
    );
    // run `schema apply`, waiting for Scylla to come up
    let args = ["schema", "apply", "--wait-for-scylla", WAIT_FOR_SCYLLA_SECS]
        .map(ToOwned::to_owned)
        .to_vec();
    let mut spec = jobs::base_spec(
        JobKind::SchemaApply,
        STEP,
        name,
        meta.cluster.get_image(),
        args,
    )?;
    // pass the key to thoradm so it reports and records it with the fingerprint
    spec.env.insert(TRIGGER_KEY_ENV.to_owned(), key.to_owned());
    // record what the Job applies for, what it applies over, and the retry request it consumed
    spec.annotations = BTreeMap::from([
        (TRIGGER_KEY_ANNOTATION.to_owned(), key.to_owned()),
        (ATTEMPT_ANNOTATION.to_owned(), attempt),
        (REVISION_ANNOTATION.to_owned(), revision.to_string()),
    ]);
    if let Some(over) = applied_over {
        spec.annotations
            .insert(APPLIED_OVER_ANNOTATION.to_owned(), over.to_owned());
    }
    if let Some(retry) = retry_requested(&meta.cluster) {
        spec.annotations
            .insert(RETRY_VALUE_ANNOTATION.to_owned(), retry.to_owned());
    }
    Ok(spec)
}

/// Get the value of a cluster's retry annotation, if it is set
///
/// # Arguments
///
/// * `cluster` - The cluster being applied
fn retry_requested(cluster: &ThoriumCluster) -> Option<&str> {
    // read the annotation from the cluster's metadata
    cluster
        .metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(RETRY_ANNOTATION))
        .map(String::as_str)
}

/// Report whether the recorded fingerprint differs from this operator's
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `state` - The recorded upgrade state
/// * `own` - The deployment fingerprint this operator's build declares, if known
async fn report_skew(meta: &ClusterMeta, state: &UpgradeState, own: Option<&str>) {
    // compare the fingerprint the Job reported with this operator's
    let reported = state.schema.get(DEPLOYMENT_MARKER).map(String::as_str);
    let (holds, message) = match (reported, own) {
        (Some(reported), Some(own)) if reported != own => (
            true,
            format!(
                "The schema-apply Job reported deployment fingerprint {reported}, but this \
                 operator's build declares {own}: the operator and the Thorium image were built \
                 from different schemas. The rollout continues; deploy an operator and image of \
                 the same release to clear this"
            ),
        ),
        _ => (
            false,
            "The schema-apply Job's fingerprint matches this operator's build, or one of them \
             is unknown"
                .to_owned(),
        ),
    };
    crds::set_upgrade_condition(
        &meta.client,
        &meta.status,
        UpgradeConditionType::SchemaSkew,
        holds,
        message,
    )
    .await;
}

/// Record a successful apply: the trigger key it ran for, the fingerprint it reported, and the
/// retry annotation value it used
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `stored` - The recorded upgrade state, which is saved
/// * `key` - The trigger key the Job applied for
/// * `summary` - The summary the Job wrote as its termination message
/// * `retry` - The retry annotation value the Job used, if any
async fn record(
    meta: &ClusterMeta,
    stored: &mut StoredState,
    key: &str,
    summary: &Value,
    retry: Option<&str>,
) -> Result<(), Error> {
    // record the key, so later reconciles compare keys instead of fingerprints
    let schema = &mut stored.state.schema;
    schema.insert(APPLY_KEY_MARKER.to_owned(), key.to_owned());
    // a successful apply ends any held failure
    schema.remove(HELD_FAILURE_MARKER);
    // the retry value it used never forces another attempt
    if let Some(retry) = retry {
        schema.insert(RETRY_VALUE_MARKER.to_owned(), retry.to_owned());
    }
    // record the fingerprint the Job reported, dropping an older one it didn't replace
    match summary.get("fingerprint").and_then(Value::as_str) {
        Some(fingerprint) => {
            schema.insert(DEPLOYMENT_MARKER.to_owned(), fingerprint.to_owned());
        }
        None => {
            schema.remove(DEPLOYMENT_MARKER);
        }
    }
    println!("Recorded the schema apply for {key} in {}", meta.namespace);
    // save at the version read, like every state write
    state::save(&meta.client, &meta.namespace, stored).await
}

/// Report a running schema-apply Job and check on it again soon
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `job` - The name of the running Job
async fn running(meta: &ClusterMeta, job: &str) -> Action {
    // say what the rollout is waiting on
    let message = format!(
        "Waiting for the schema-apply Job {job} to converge Scylla before any component is \
         rolled out"
    );
    hold(meta, message).await
}

/// Report a schema-apply Job for an earlier trigger key that is still running thoradm, which
/// is never deleted mid-apply, and check on it again soon
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `job` - The name of the running Job
/// * `old_key` - The trigger key it was created for
async fn old_key_running(meta: &ClusterMeta, job: &str, old_key: &str) -> Action {
    // name the Job and its key, and say why the current key waits
    let message = format!(
        "Waiting for the schema-apply Job {job} of the earlier trigger key {old_key}: it is \
         still running thoradm, so it is not deleted. The Job for the current trigger key \
         starts once it ends, at the latest at its deadline"
    );
    hold(meta, message).await
}

/// Report a schema-apply Job for an earlier trigger key that is being deleted, and check on it
/// again soon
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `job` - The name of the Job being deleted
async fn old_key_deleting(meta: &ClusterMeta, job: &str) -> Action {
    // name the Job and say the current key waits until it is gone
    let message = format!(
        "Waiting for the schema-apply Job {job} of an earlier trigger key to be deleted. The \
         Job for the current trigger key starts once it is gone"
    );
    hold(meta, message).await
}

/// Hold the rollout in the provisioning phase with what it waits on, and check again soon
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `message` - What the rollout waits on
async fn hold(meta: &ClusterMeta, message: String) -> Action {
    // log and report what the rollout waits on
    println!("{message}");
    crds::set_status(
        &meta.client,
        &meta.status,
        ClusterPhase::Provisioning,
        Some(message),
    )
    .await;
    Action::requeue(Duration::from_secs(RUNNING_REQUEUE_SECS))
}

/// Report a failed schema-apply Job that holds the rollout until a retry is asked for or the
/// trigger key changes, and record it in the upgrade state so it keeps holding after its Job is
/// deleted
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `stored` - The recorded upgrade state, which is saved
/// * `key` - The trigger key the Job was created for
/// * `job` - The failed Job
async fn failed(
    meta: &ClusterMeta,
    stored: &mut StoredState,
    key: &str,
    job: &Job,
) -> Result<Action, Error> {
    // read why it failed and its pod's last log lines
    let job_name = name(job);
    let (reason, pod, log_tail, refused) = match jobs::state(meta, job).await? {
        JobState::Failed {
            reason,
            pod,
            log_tail,
            summary,
        } => (reason, pod, log_tail, refused_settings(summary.as_ref())),
        _ => (String::new(), None, Vec::new(), Vec::new()),
    };
    // report the failed Job in the status for reading its logs
    let failure = JobFailure {
        job: job_name.clone(),
        pod,
        log_tail,
    };
    crds::set_upgrade_failure(&meta.client, &meta.status, Some(failure)).await;
    // record the held failure and the retry value its attempt used; later reconciles hold on
    // the recorded failure without reaching here, so this saves once per failure
    let held = HeldFailure {
        key: key.to_owned(),
        attempt: attempt(job),
        job: job_name.clone(),
        reason: reason.clone(),
        refused: refused.clone(),
    };
    let raw = serde_json::to_string(&held)
        .map_err(|error| Error::new(format!("Failed to serialize a held failure: {error}")))?;
    let schema = &mut stored.state.schema;
    schema.insert(HELD_FAILURE_MARKER.to_owned(), raw);
    if let Some(retry) = annotation(job, RETRY_VALUE_ANNOTATION) {
        schema.insert(RETRY_VALUE_MARKER.to_owned(), retry.to_owned());
    }
    state::save(&meta.client, &meta.namespace, stored).await?;
    // hold the rollout with what to do
    Ok(report_failure(meta, &job_name, &reason, &refused).await)
}

/// Read the settings a failed apply refused from its termination summary
///
/// # Arguments
///
/// * `summary` - The summary the Job's pod wrote, if it got that far
fn refused_settings(summary: Option<&Value>) -> Vec<String> {
    // the summary lists them as config paths
    summary
        .and_then(|summary| summary.get("refused"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|setting| setting.as_str().map(ToOwned::to_owned))
        .collect()
}

/// Hold the rollout in the error phase for a failed schema-apply Job, saying what starts the
/// next attempt
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `job` - The name of the failed Job
/// * `reason` - Why it failed
/// * `refused` - The settings the apply refused, as config paths
async fn report_failure(meta: &ClusterMeta, job: &str, reason: &str, refused: &[String]) -> Action {
    // name the settings the apply refused to change, if it refused any
    let refusal = if refused.is_empty() {
        String::new()
    } else {
        format!(
            " It refused to apply a changed setting while data written under the old one exists: \
             {}.",
            refused.join(", ")
        )
    };
    // say why it failed, where its logs are, and what runs it again
    let message = format!(
        "The schema-apply Job {job} failed ({reason}), so no component is rolled out;{refusal} see \
         status.upgrade.failure for its logs. It is not retried by itself: set the \
         {RETRY_ANNOTATION} annotation on the ThoriumCluster to a new value to run it again, or \
         change what its trigger key covers (the image, the revision, or the data-shaping \
         settings)"
    );
    println!("{message}");
    crds::set_status(
        &meta.client,
        &meta.status,
        ClusterPhase::Error,
        Some(message),
    )
    .await;
    Action::requeue(Duration::from_secs(HELD_REQUEUE_SECS))
}

/// Clear the way for the Job of the current trigger key: delete every active Job for an earlier
/// key whose pods never started thoradm and wait for one that runs it
///
/// Returns the action to hold the rollout with while an old-key Job is active, or nothing once
/// the Job for the current key may be created. Every active Job for the current key was handled
/// before, so each active Job here is for an earlier key.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `found` - Every schema-apply Job
async fn clear_old_keys(meta: &ClusterMeta, found: &[Job]) -> Result<Option<Action>, Error> {
    // sort the active old-key Jobs into one running thoradm and those deleted or being deleted
    let mut running_old = None;
    let mut deleting = None;
    for old in found
        .iter()
        .filter(|job| jobs::phase(job) == JobPhase::Active)
    {
        // check the pods even of a Job being deleted, which may still run thoradm
        let old_name = name(old);
        if !jobs::never_started(meta, &old_name).await? {
            running_old.get_or_insert(old);
            continue;
        }
        // a Job already being deleted needs no second delete
        if old.metadata.deletion_timestamp.is_none() {
            jobs::delete(meta, &old_name).await?;
        }
        deleting.get_or_insert(old_name);
    }
    // an old-key Job running thoradm is waited for, naming its key
    if let Some(old) = running_old {
        let old_key = annotation(old, TRIGGER_KEY_ANNOTATION).unwrap_or_default();
        return Ok(Some(old_key_running(meta, &name(old), old_key).await));
    }
    // an old-key Job being deleted is waited for until it is gone, so its pod can't overlap
    // the Job for this key
    if let Some(old_name) = deleting {
        return Ok(Some(old_key_deleting(meta, &old_name).await));
    }
    // no old-key Job is active
    Ok(None)
}

/// Run the schema-apply Job when its trigger key changed, and hold the rollout until it succeeds
///
/// Returns the action to take while the rollout is held, or nothing once the schema is applied
/// for the current trigger key.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
pub async fn ensure(meta: &ClusterMeta) -> Result<Option<Action>, Error> {
    // compare against this build's fingerprint
    ensure_at(meta, Some(&build_fingerprint())).await
}

/// Run the schema-apply Job when its trigger key changed, against a build's fingerprint
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `own` - The deployment fingerprint this operator's build declares, if known
pub(super) async fn ensure_at(
    meta: &ClusterMeta,
    own: Option<&str>,
) -> Result<Option<Action>, Error> {
    // warn about a mutable image tag whether or not the Job runs
    report_mutable_tag(meta).await;
    // load the recorded state, which the upgrade gate records before any cluster is applied
    let mut stored = state::load(&meta.client, &meta.namespace)
        .await?
        .ok_or_else(|| {
            Error::new(format!(
                "Namespace {} has no recorded upgrade state to apply the schema for",
                meta.namespace
            ))
        })?;
    // apply for the revision the state records
    let revision = stored.state.revision.clone();
    ensure_with(meta, &mut stored, &revision, own).await
}

/// Run the schema-apply step of an upgrade, applying the schema for the revision it moves to
///
/// Returns the action to take while the Job runs or the apply is held, or nothing once the schema
/// is applied for the target.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade being run, whose state records the apply
pub(super) async fn run_step(
    meta: &ClusterMeta,
    pending: &mut Pending,
) -> Result<Option<Action>, Error> {
    // the step runs before any step that changes data, for the revision the upgrade reaches
    let revision = pending.target.clone();
    ensure_with(
        meta,
        &mut pending.stored,
        &revision,
        Some(&build_fingerprint()),
    )
    .await
}

/// Run the schema-apply Job when its trigger key changed, for a revision and a recorded state
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `stored` - The recorded upgrade state, which is saved as the apply is recorded
/// * `revision` - The revision the cluster is at or moving to
/// * `own` - The deployment fingerprint this operator's build declares, if known
async fn ensure_with(
    meta: &ClusterMeta,
    stored: &mut StoredState,
    revision: &RevisionId,
    own: Option<&str>,
) -> Result<Option<Action>, Error> {
    // build the key for what is deployed now
    let key = trigger_key(&meta.cluster, &meta.conf, revision);
    // a retry annotation value other than the last one used forces one more attempt
    let retry = retry_requested(&meta.cluster);
    let last_retry = stored
        .state
        .schema
        .get(RETRY_VALUE_MARKER)
        .map(String::as_str);
    let forced = retry_forces(retry, last_retry);
    // a failed attempt recorded for this key holds the rollout, even after its Job is deleted,
    // until a retry is forced
    let held = held_failure(&stored.state, &key);
    if let Some(held) = &held
        && !forced
    {
        // a refused partition size is moved by a re-bucket step, which the operator plans
        if let Some(action) = rebucket::drive(meta, stored, revision, &held.refused).await? {
            return Ok(Some(action));
        }
        return Ok(Some(
            report_failure(meta, &held.job, &held.reason, &held.refused).await,
        ));
    }
    // a successful apply for this key leaves nothing to run unless a retry is forced
    if !forced && stored.state.schema.get(APPLY_KEY_MARKER) == Some(&key) {
        report_skew(meta, &stored.state, own).await;
        crds::set_upgrade_failure(&meta.client, &meta.status, None).await;
        return Ok(None);
    }
    // find the schema-apply Jobs by label, and those created for this key
    let found = jobs::find(meta, JobKind::SchemaApply, STEP).await?;
    let for_key = || {
        found
            .iter()
            .filter(|job| annotation(job, TRIGGER_KEY_ANNOTATION) == Some(key.as_str()))
    };
    // a Job for this key that succeeded over the recorded state is recorded, even after an
    // interrupted reconcile; one left from before another key was applied is stale, and so is
    // one that didn't use a retry value forcing a new attempt
    let applied_over = stored.state.schema.get(APPLY_KEY_MARKER).cloned();
    if let Some(done) = for_key().find(|job| {
        jobs::phase(job) == JobPhase::Complete
            && annotation(job, APPLIED_OVER_ANNOTATION) == applied_over.as_deref()
            && (!forced || annotation(job, RETRY_VALUE_ANNOTATION) == retry)
    }) {
        let summary = match jobs::state(meta, done).await? {
            JobState::Succeeded(summary) => summary,
            _ => Value::Null,
        };
        let used = annotation(done, RETRY_VALUE_ANNOTATION);
        record(meta, stored, &key, &summary, used).await?;
        report_skew(meta, &stored.state, own).await;
        crds::set_upgrade_failure(&meta.client, &meta.status, None).await;
        return Ok(None);
    }
    // a running Job for this key holds the rollout until it finishes
    if let Some(active) = for_key().find(|job| jobs::phase(job) == JobPhase::Active) {
        return Ok(Some(running(meta, &name(active)).await));
    }
    // only one schema-apply Job runs at a time, so active Jobs for an earlier key go first
    if let Some(action) = clear_old_keys(meta, &found).await? {
        return Ok(Some(action));
    }
    // find the latest attempt for this key; a stale Job still counts, so the next attempt
    // never reuses its name
    let latest = for_key().max_by_key(|job| attempt(job));
    // attempts below the floor were superseded by the step that cleared their refusal
    let floor = stored
        .state
        .schema
        .get(ATTEMPT_FLOOR_MARKER)
        .and_then(|raw| raw.parse::<u32>().ok())
        .unwrap_or_default();
    let next = latest
        .map_or(0, |job| attempt(job) + 1)
        .max(held.as_ref().map_or(0, |held| held.attempt + 1))
        .max(floor);
    // a failed latest attempt holds the rollout until a retry is asked for or the key changes
    if let Some(job) = latest
        && attempt(job) >= floor
        && matches!(jobs::phase(job), JobPhase::Failed { .. })
        && failure_holds(
            job,
            &found,
            applied_over.as_deref(),
            retry.filter(|_| forced),
        )
    {
        // report the failure, then plan the re-bucket a refused partition size needs
        let action = failed(meta, stored, &key, job).await?;
        let refused = held_failure(&stored.state, &key).map_or_else(Vec::new, |held| held.refused);
        if let Some(next) = rebucket::drive(meta, stored, revision, &refused).await? {
            return Ok(Some(next));
        }
        return Ok(Some(action));
    }
    // the next attempt supersedes a held failure, of this key or another
    if stored.state.schema.remove(HELD_FAILURE_MARKER).is_some() {
        state::save(&meta.client, &meta.namespace, stored).await?;
    }
    // start the next attempt for this key, once no other writing Job runs
    let writers = jobs::active_writers(meta).await?;
    if let Some(writer) = writers
        .iter()
        .find(|writer| !writer.starts_with("schema-apply-"))
    {
        let message =
            format!("Waiting for the writing Job {writer} to end before the schema apply");
        return Ok(Some(hold(meta, message).await));
    }
    let spec = spec(meta, revision, &key, next, applied_over.as_deref())?;
    jobs::create(&spec, meta).await?;
    Ok(Some(running(meta, &spec.name).await))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::k8s::clusters::tests::{
        FakeKube, FakeRequest, full_spec, meta_for, namespaced_cluster,
    };
    use crate::upgrades::tests::{STATE_PATH, STATUS_PATH, state_config_map};
    use thorium::models::upgrades::{
        self, HELD_FAILURE_MARKER, RETRY_VALUE_MARKER, STATE_KEY, UpgradeConditionType,
    };

    /// The path Jobs are listed and created at in the test namespace
    pub(crate) const JOBS_PATH: &str = "/apis/batch/v1/namespaces/thorium/jobs";

    /// The path pods are listed at in the test namespace
    pub(crate) const PODS_PATH: &str = "/api/v1/namespaces/thorium/pods";

    /// The fingerprint the operator's build declares in these tests
    const OWN: &str = "0000000000000000000000000000000000000000000000000000000000000001";

    /// The fingerprint a skewed image's schema-apply Job reports in these tests
    const REPORTED: &str = "00000000000000000000000000000000000000000000000000000000000000ff";

    /// Build the upgrade state of a cluster at the first Helm revision
    ///
    /// # Arguments
    ///
    /// * `apply_key` - The trigger key the last successful apply recorded, if any
    pub(crate) fn state(apply_key: Option<&str>) -> UpgradeState {
        // record the revision the chart deploys and the key if there is one
        let revision = "2026-10-v02".parse().expect("revision");
        let mut state = UpgradeState::new(revision, "1.8.1");
        if let Some(key) = apply_key {
            state
                .schema
                .insert(APPLY_KEY_MARKER.to_owned(), key.to_owned());
        }
        state
    }

    /// Build a `JobList` the kube API returns
    ///
    /// # Arguments
    ///
    /// * `jobs` - The Jobs in the list
    pub(crate) fn job_list(jobs: &[serde_json::Value]) -> serde_json::Value {
        serde_json::json!({"apiVersion": "batch/v1", "kind": "JobList", "metadata": {}, "items": jobs})
    }

    /// Build a schema-apply Job as the kube API returns it
    ///
    /// # Arguments
    ///
    /// * `name` - The Job's name
    /// * `key` - The trigger key it was created for
    /// * `attempt` - Its attempt counter
    /// * `condition` - Its terminal condition and when it became true, or None while it runs
    pub(crate) fn job(
        name: &str,
        key: &str,
        attempt: u32,
        condition: Option<(&str, DateTime<Utc>)>,
    ) -> serde_json::Value {
        // label it like the operator does and add the terminal condition
        let conditions: Vec<serde_json::Value> = condition
            .into_iter()
            .map(|(kind, at)| {
                serde_json::json!({
                    "type": kind,
                    "status": "True",
                    "reason": if kind == "Failed" { "PodFailurePolicy" } else { "Completed" },
                    "message": "fake",
                    "lastTransitionTime": at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                })
            })
            .collect();
        serde_json::json!({
            "apiVersion": "batch/v1",
            "kind": "Job",
            "metadata": {
                "name": name,
                "namespace": "thorium",
                "labels": {jobs::KIND_LABEL: "schema-apply", jobs::STEP_LABEL: STEP},
                "annotations": {TRIGGER_KEY_ANNOTATION: key, ATTEMPT_ANNOTATION: attempt.to_string()}
            },
            "spec": {"template": {"spec": {"containers": []}}},
            "status": {"conditions": conditions}
        })
    }

    /// Build a `PodList` holding one finished thoradm pod with a termination message
    ///
    /// # Arguments
    ///
    /// * `message` - The termination message thoradm wrote
    pub(crate) fn finished_pods(message: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "v1",
            "kind": "PodList",
            "metadata": {},
            "items": [{
                "metadata": {"name": "schema-apply-pod", "namespace": "thorium"},
                "status": {"containerStatuses": [{
                    "name": "thoradm",
                    "ready": false,
                    "restartCount": 0,
                    "image": "registry/thorium:1.8.1",
                    "imageID": "",
                    "state": {"terminated": {"exitCode": 0, "message": message}}
                }]}
            }]
        })
    }

    /// Build a fake kube API serving a recorded state, a list of schema-apply Jobs, and the
    /// writes the schema-apply Job makes
    ///
    /// # Arguments
    ///
    /// * `cluster` - The cluster being applied
    /// * `state` - The recorded upgrade state
    /// * `jobs` - The schema-apply Jobs that exist
    pub(crate) fn fake_kube(
        cluster: &ThoriumCluster,
        state: &UpgradeState,
        jobs: &[serde_json::Value],
    ) -> FakeKube {
        // answer status patches, state reads and saves, Job lists and creates, and pod lists
        let created = job("created", "", 0, None);
        FakeKube::default()
            .route(
                "PATCH",
                STATUS_PATH,
                200,
                serde_json::to_value(cluster).expect("cluster"),
            )
            .route("GET", STATE_PATH, 200, state_config_map(state))
            .route("PUT", STATE_PATH, 200, state_config_map(state))
            .route("GET", JOBS_PATH, 200, job_list(jobs))
            .route("POST", JOBS_PATH, 201, created)
            .route("GET", PODS_PATH, 200, finished_pods("{}"))
    }

    /// Get the Jobs created through a fake kube API
    ///
    /// # Arguments
    ///
    /// * `fake` - The fake kube API
    pub(crate) fn created_jobs(fake: &FakeKube) -> Vec<serde_json::Value> {
        // keep the body of every Job creation
        fake.writes()
            .into_iter()
            .filter(|request| request.method == "POST" && request.path == JOBS_PATH)
            .map(|request| request.body)
            .collect()
    }

    /// Get the state last saved through a fake kube API
    ///
    /// # Arguments
    ///
    /// * `fake` - The fake kube API
    fn saved(fake: &FakeKube) -> Option<UpgradeState> {
        // parse the state in the last replaced ConfigMap
        let request = fake
            .writes()
            .into_iter()
            .rfind(|request| request.method == "PUT" && request.path == STATE_PATH)?;
        let raw = request.body["data"][STATE_KEY].as_str().expect("state key");
        Some(serde_json::from_str(raw).expect("state parses"))
    }

    /// Get the trigger key a cluster has at the first Helm revision with the sample config
    ///
    /// # Arguments
    ///
    /// * `cluster` - The cluster being applied
    pub(crate) fn key_of(cluster: &ThoriumCluster) -> String {
        // use the config the test cluster metadata is built with
        let meta = meta_for(cluster.clone(), &FakeKube::default().client());
        trigger_key(&meta.cluster, &meta.conf, &state(None).revision)
    }

    /// A trigger input to change: its name, the cluster and state to apply, and a config change
    type Case = (&'static str, ThoriumCluster, UpgradeState, fn(&mut Conf));

    /// Build the chart shaped test cluster
    pub(crate) fn cluster() -> ThoriumCluster {
        namespaced_cluster(full_spec())
    }

    /// Check that a built thoradm Job fails at once on thoradm's refusal and data-issue exit
    /// codes, doesn't count disruptions, and retries anything else twice
    ///
    /// # Arguments
    ///
    /// * `job` - The built Job as JSON
    /// * `kind` - The Job's kind label, for failure messages
    fn assert_failure_policy(job: &serde_json::Value, kind: &str) {
        // exit codes 1, 2, 3, 5, and 6 fail the Job and a disruption is ignored
        assert_eq!(
            job["spec"]["podFailurePolicy"],
            serde_json::json!({"rules": [
                {
                    "action": "FailJob",
                    "onExitCodes": {"containerName": "thoradm", "operator": "In", "values": [1, 2, 3, 5, 6]}
                },
                {
                    "action": "Ignore",
                    "onPodConditions": [{"type": "DisruptionTarget", "status": "True"}]
                }
            ]}),
            "{kind}"
        );
        // any other failure is retried twice
        assert_eq!(job["spec"]["backoffLimit"], 2, "{kind}");
    }

    /// The schema-apply Job, and every other thoradm Job, runs restricted under the jobs
    /// service account without a token, bounded, with the config read-only, the tracing config
    /// mounted, and a TTL
    #[tokio::test]
    async fn thoradm_jobs_run_restricted() {
        // build the schema-apply Job and a Job of every other kind
        let fake = FakeKube::default();
        let meta = meta_for(cluster(), &fake.client());
        let schema_apply = spec(&meta, &state(None).revision, "key", 0, None).expect("spec");
        let mut specs = vec![schema_apply];
        for kind in [
            JobKind::Migration,
            JobKind::SchemaStatus,
            JobKind::MigrationCheck,
            JobKind::Record,
        ] {
            let name = jobs::job_name(kind, "step", &["0"]);
            let spec = jobs::base_spec(kind, "step", name, meta.cluster.get_image(), Vec::new())
                .expect("spec");
            specs.push(spec);
        }
        for spec in specs {
            let job =
                serde_json::to_value(jobs::build(&spec, &meta).expect("build")).expect("json");
            let kind = spec.kind.label();
            let pod = &job["spec"]["template"]["spec"];
            // the jobs service account, never thorium, with no token mounted
            assert_eq!(pod["serviceAccountName"], "thorium-jobs", "{kind}");
            assert_eq!(pod["automountServiceAccountToken"], false, "{kind}");
            // the restricted pod security context as user 10001
            let context = &pod["securityContext"];
            assert_eq!(context["runAsNonRoot"], true, "{kind}");
            assert_eq!(context["runAsUser"], 10001, "{kind}");
            assert_eq!(
                context["seccompProfile"]["type"], "RuntimeDefault",
                "{kind}"
            );
            let container = &pod["containers"][0];
            let restricted = &container["securityContext"];
            assert_eq!(restricted["allowPrivilegeEscalation"], false, "{kind}");
            assert_eq!(
                restricted["capabilities"]["drop"],
                serde_json::json!(["ALL"]),
                "{kind}"
            );
            assert_eq!(restricted["readOnlyRootFilesystem"], true, "{kind}");
            // CPU and memory requests and limits
            for bound in ["requests", "limits"] {
                for resource in ["cpu", "memory"] {
                    assert!(
                        container["resources"][bound][resource].is_string(),
                        "{kind} {bound} {resource}"
                    );
                }
            }
            // the config Secret mounted read-only and the tracing config mounted
            let mounts = container["volumeMounts"].as_array().expect("mounts");
            let config = mounts
                .iter()
                .find(|mount| mount["mountPath"] == "/conf/thorium.yml")
                .expect("config mount");
            assert_eq!(config["readOnly"], true, "{kind}");
            assert!(
                mounts.iter().any(|mount| mount["name"] == "tracing"),
                "{kind}"
            );
            let volumes = pod["volumes"].as_array().expect("volumes");
            assert!(
                volumes
                    .iter()
                    .any(|volume| volume["secret"]["secretName"] == "thorium"),
                "{kind}"
            );
            assert!(
                volumes
                    .iter()
                    .any(|volume| volume["configMap"]["name"] == "tracing-conf"),
                "{kind}"
            );
            // a TTL keeps the finished Job only for a while, and it never runs forever
            assert!(
                job["spec"]["ttlSecondsAfterFinished"]
                    .as_i64()
                    .unwrap_or_default()
                    > 0,
                "{kind}"
            );
            assert!(
                job["spec"]["activeDeadlineSeconds"]
                    .as_i64()
                    .unwrap_or_default()
                    > 0,
                "{kind}"
            );
            assert_eq!(pod["restartPolicy"], "Never", "{kind}");
            // thoradm's own failures fail the Job at once and anything else is retried twice
            assert_failure_policy(&job, kind);
            // labelled by kind and step so it is found by label
            assert_eq!(job["metadata"]["labels"][jobs::KIND_LABEL], kind, "{kind}");
        }
    }

    /// The trigger key changes with the image, the revision, a partition size, and a table TTL,
    /// and with nothing else
    #[tokio::test]
    async fn trigger_key_tracks_its_inputs() {
        // the key of the test cluster at the first Helm revision
        let meta = meta_for(cluster(), &FakeKube::default().client());
        let revision = state(None).revision;
        let base = trigger_key(&meta.cluster, &meta.conf, &revision);
        // the same inputs give the same key
        assert_eq!(trigger_key(&meta.cluster, &meta.conf, &revision), base);
        // a new image reference changes it
        let mut spec = full_spec();
        spec["version"] = serde_json::json!("1.8.2");
        let newer = namespaced_cluster(spec);
        assert_ne!(trigger_key(&newer, &meta.conf, &revision), base);
        // a new revision changes it
        let next = "2026-10-v03".parse().expect("revision");
        assert_ne!(trigger_key(&meta.cluster, &meta.conf, &next), base);
        // a changed partition size changes it
        let mut conf = meta.conf.clone();
        conf.thorium.files.partition_size += 1;
        assert_ne!(trigger_key(&meta.cluster, &conf, &revision), base);
        // a changed table TTL changes it
        let mut conf = meta.conf.clone();
        conf.thorium.retention.logs += 1;
        assert_ne!(trigger_key(&meta.cluster, &conf, &revision), base);
        // a changed files password changes it
        let mut conf = meta.conf.clone();
        conf.thorium.files.password.push('x');
        assert_ne!(trigger_key(&meta.cluster, &conf, &revision), base);
        // a setting that shapes no stored data leaves it alone
        let mut conf = meta.conf.clone();
        conf.thorium.retention.results += 1;
        assert_eq!(trigger_key(&meta.cluster, &conf, &revision), base);
        // so does the events retention, whose table nothing reads, so a change would run the Job
        // for nothing
        let mut conf = meta.conf.clone();
        conf.thorium.events.retention += 1;
        assert_eq!(trigger_key(&meta.cluster, &conf, &revision), base);
    }

    /// The files password used in the password tests, made of runs that appear nowhere else in
    /// a trigger key
    const PASSWORD: &str = "Zq9!Xw7#Pk3$Vm5%Hj2&";

    /// The trigger key carries a sha256 of the files password as its own segment, never any
    /// part of the password itself, and the same password keeps the same key
    #[tokio::test]
    async fn trigger_key_hashes_the_files_password() {
        // the key of the test cluster with a known files password
        let mut meta = meta_for(cluster(), &FakeKube::default().client());
        meta.conf.thorium.files.password = PASSWORD.to_owned();
        let revision = state(None).revision;
        let key = trigger_key(&meta.cluster, &meta.conf, &revision);
        // the password's lowercase hex sha256 is its own segment
        let digest = format!("{:x}", Sha256::digest(PASSWORD.as_bytes()));
        let segment = format!("password={digest}");
        assert!(key.split(';').any(|part| part == segment), "{key}");
        // no run of four or more characters of the password shows up in the key
        let chars: Vec<char> = PASSWORD.chars().collect();
        for window in chars.windows(4) {
            let run: String = window.iter().collect();
            assert!(!key.contains(&run), "{run} in {key}");
        }
        // the same password gives the same key
        let mut same = meta.conf.clone();
        same.thorium.files.password = PASSWORD.to_owned();
        assert_eq!(trigger_key(&meta.cluster, &same, &revision), key);
    }

    /// A password-only change runs the schema-apply Job, and a failed Job for the new key holds
    /// the rollout without another attempt; an unchanged password runs nothing
    #[tokio::test]
    async fn password_change_runs_the_job() {
        // the key recorded for the test cluster's own password
        let cluster = cluster();
        let current = key_of(&cluster);
        let fake = fake_kube(&cluster, &state(Some(&current)), &[]);
        let meta = meta_for(cluster.clone(), &fake.client());
        // the unchanged password runs nothing
        let action = ensure_at(&meta, Some(OWN)).await.expect("ensure");
        assert!(action.is_none());
        assert!(created_jobs(&fake).is_empty(), "{:?}", created_jobs(&fake));
        // a changed password alone runs one Job for a new key
        let fake = fake_kube(&cluster, &state(Some(&current)), &[]);
        let mut meta = meta_for(cluster.clone(), &fake.client());
        meta.conf.thorium.files.password = PASSWORD.to_owned();
        let action = ensure_at(&meta, Some(OWN)).await.expect("ensure");
        assert!(action.is_some());
        let created = created_jobs(&fake);
        assert_eq!(created.len(), 1);
        let key = created[0]["metadata"]["annotations"][TRIGGER_KEY_ANNOTATION]
            .as_str()
            .expect("key")
            .to_owned();
        assert_ne!(key, current);
        // that Job failed, so the rollout is held in the error phase and nothing else runs
        let name = created[0]["metadata"]["name"].as_str().expect("name");
        let mut failed = job(name, &key, 0, Some(("Failed", Utc::now())));
        failed["metadata"]["annotations"][APPLIED_OVER_ANNOTATION] = serde_json::json!(current);
        let fake = fake_kube(&cluster, &state(Some(&current)), &[failed]);
        let mut meta = meta_for(cluster, &fake.client());
        meta.conf.thorium.files.password = PASSWORD.to_owned();
        let action = ensure_at(&meta, Some(OWN)).await.expect("ensure");
        assert!(action.is_some());
        assert!(created_jobs(&fake).is_empty(), "{:?}", created_jobs(&fake));
        assert_eq!(meta.status.current().phase, Some(ClusterPhase::Error));
    }

    /// A recorded key or a Job's key in an older format, which the current key starts with, is
    /// another key: the Job runs for the current key
    #[tokio::test]
    async fn older_key_format_is_another_key() {
        // the current key without its password segment, as an older operator built it
        let cluster = cluster();
        let key = key_of(&cluster);
        let older = key
            .split(";password=")
            .next()
            .expect("older key")
            .to_owned();
        assert!(key.starts_with(&older) && key != older, "{key}");
        // a recorded older key runs the Job for the current key
        let fake = fake_kube(&cluster, &state(Some(&older)), &[]);
        let meta = meta_for(cluster.clone(), &fake.client());
        let action = ensure_at(&meta, Some(OWN)).await.expect("ensure");
        assert!(action.is_some());
        let created = created_jobs(&fake);
        assert_eq!(created.len(), 1);
        assert_eq!(
            created[0]["metadata"]["annotations"][TRIGGER_KEY_ANNOTATION],
            key.as_str()
        );
        // a completed Job of the older key is not taken for an apply of the current key
        let done = job(
            "schema-apply-older",
            &older,
            0,
            Some(("Complete", Utc::now())),
        );
        let fake = fake_kube(&cluster, &state(None), &[done]);
        let meta = meta_for(cluster, &fake.client());
        let action = ensure_at(&meta, Some(OWN)).await.expect("ensure");
        assert!(action.is_some());
        assert!(saved(&fake).is_none());
        assert_eq!(created_jobs(&fake).len(), 1);
    }

    /// A fresh install creates the schema-apply Job, which carries its trigger key
    #[tokio::test]
    async fn fresh_install_runs_the_job() {
        // a namespace whose state records no apply
        let cluster = cluster();
        let fake = fake_kube(&cluster, &state(None), &[]);
        let meta = meta_for(cluster.clone(), &fake.client());
        // the rollout is held while the Job runs
        let action = ensure_at(&meta, Some(OWN)).await.expect("ensure");
        assert!(action.is_some());
        // exactly one Job was created, for the current key
        let created = created_jobs(&fake);
        assert_eq!(created.len(), 1);
        let annotations = &created[0]["metadata"]["annotations"];
        assert_eq!(annotations[TRIGGER_KEY_ANNOTATION], key_of(&cluster));
        // the Jobs were looked for by their kind and step labels
        let selectors: Vec<Option<String>> = fake
            .requests()
            .iter()
            .filter(|request| request.method == "GET" && request.path == JOBS_PATH)
            .map(|request| request.query_param("labelSelector"))
            .collect();
        assert_eq!(
            selectors[0],
            Some(format!(
                "{}=schema-apply,{}={STEP}",
                jobs::KIND_LABEL,
                jobs::STEP_LABEL
            ))
        );
        // then every kind of writing Job was looked for by its kind alone, before the new Job
        assert_eq!(
            selectors[1..],
            ["schema-apply", "migration", "record"]
                .map(|kind| Some(format!("{}={kind}", jobs::KIND_LABEL)))
        );
        // the Job runs schema apply with the key in its environment
        let container = &created[0]["spec"]["template"]["spec"]["containers"][0];
        let args = container["args"].as_array().expect("args");
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "schema" && pair[1] == "apply"),
            "{args:?}"
        );
        let env = container["env"].as_array().expect("env");
        assert!(
            env.iter()
                .any(|var| var["name"] == TRIGGER_KEY_ENV && var["value"] == key_of(&cluster))
        );
        // the status says what the rollout waits on
        let status = meta.status.current();
        assert_eq!(status.phase, Some(ClusterPhase::Provisioning));
        assert!(status.message.unwrap_or_default().contains("schema-apply"));
    }

    /// A changed trigger key runs the Job again and an unchanged one runs nothing
    #[tokio::test]
    async fn changed_trigger_key_runs_the_job() {
        // with the current key recorded nothing is listed or created
        let cluster = cluster();
        let current = key_of(&cluster);
        let fake = fake_kube(&cluster, &state(Some(&current)), &[]);
        let meta = meta_for(cluster.clone(), &fake.client());
        let action = ensure_at(&meta, Some(OWN)).await.expect("ensure");
        assert!(action.is_none());
        assert!(created_jobs(&fake).is_empty(), "{:?}", created_jobs(&fake));
        assert!(
            fake.requests()
                .iter()
                .all(|request| request.path != JOBS_PATH)
        );
        // each changed input runs the Job: a new image, a new revision, a new partition size,
        // and a new TTL
        let mut newer_spec = full_spec();
        newer_spec["version"] = serde_json::json!("1.8.2");
        let newer = namespaced_cluster(newer_spec);
        let mut older_revision = state(Some(&current));
        older_revision.revision = "2026-10-v01".parse().expect("revision");
        let cases: Vec<Case> = vec![
            ("image", newer, state(Some(&current)), |_| ()),
            ("revision", cluster.clone(), older_revision, |_| ()),
            (
                "partition size",
                cluster.clone(),
                state(Some(&current)),
                |conf| {
                    conf.thorium.results.partition_size = 60;
                },
            ),
            ("ttl", cluster.clone(), state(Some(&current)), |conf| {
                conf.thorium.retention.notifications = 3600;
            }),
        ];
        for (case, cluster, state, change) in cases {
            let fake = fake_kube(&cluster, &state, &[]);
            let mut meta = meta_for(cluster, &fake.client());
            change(&mut meta.conf);
            let action = ensure_at(&meta, Some(OWN)).await.expect("ensure");
            assert!(action.is_some(), "{case}");
            assert_eq!(created_jobs(&fake).len(), 1, "{case}");
        }
    }

    /// Set the retry annotation on a cluster
    ///
    /// # Arguments
    ///
    /// * `cluster` - The cluster to annotate
    /// * `value` - The annotation's value
    fn with_retry(cluster: &ThoriumCluster, value: &str) -> ThoriumCluster {
        // replace the cluster's annotations with the retry request
        let mut retried = cluster.clone();
        retried.metadata.annotations = Some(BTreeMap::from([(
            RETRY_ANNOTATION.to_owned(),
            value.to_owned(),
        )]));
        retried
    }

    /// A failed schema-apply Job is never retried by itself, and each new value of the retry
    /// annotation creates exactly one new attempt under a new name
    #[tokio::test]
    async fn failed_job_waits_for_the_retry_annotation() {
        // the names the operator gives the first three attempts for the current key
        let cluster = cluster();
        let key = key_of(&cluster);
        let revision = state(None).revision;
        let named = meta_for(cluster.clone(), &FakeKube::default().client());
        let first = spec(&named, &revision, &key, 0, None).expect("spec").name;
        let second = spec(&named, &revision, &key, 1, None).expect("spec").name;
        let third = spec(&named, &revision, &key, 2, None).expect("spec").name;
        // a first attempt for the current key that failed long ago is not retried
        let long_ago = Utc::now() - chrono::Duration::hours(2);
        let failed = job(&first, &key, 0, Some(("Failed", long_ago)));
        let fake = fake_kube(&cluster, &state(None), std::slice::from_ref(&failed));
        let meta = meta_for(cluster.clone(), &fake.client());
        let action = ensure_at(&meta, Some(OWN)).await.expect("ensure");
        assert!(action.is_some());
        assert!(created_jobs(&fake).is_empty(), "{:?}", created_jobs(&fake));
        // a retry annotation value creates one new attempt under another name
        let retried = with_retry(&cluster, "1");
        let fake = fake_kube(&retried, &state(None), &[failed]);
        let meta = meta_for(retried.clone(), &fake.client());
        ensure_at(&meta, Some(OWN)).await.expect("ensure");
        let created = created_jobs(&fake);
        assert_eq!(created.len(), 1);
        assert_eq!(created[0]["metadata"]["name"], second.as_str());
        let annotations = &created[0]["metadata"]["annotations"];
        assert_eq!(annotations[ATTEMPT_ANNOTATION], "1");
        assert_eq!(annotations[RETRY_VALUE_ANNOTATION], "1");
        // once that attempt failed too, the same value creates nothing more
        let mut consumed = job(&second, &key, 1, Some(("Failed", Utc::now())));
        consumed["metadata"]["annotations"][RETRY_VALUE_ANNOTATION] = serde_json::json!("1");
        let fake = fake_kube(&retried, &state(None), std::slice::from_ref(&consumed));
        let meta = meta_for(retried, &fake.client());
        for reconcile in 0..3 {
            ensure_at(&meta, Some(OWN)).await.expect("ensure");
            assert!(created_jobs(&fake).is_empty(), "{reconcile}");
        }
        // the next value creates exactly one more attempt
        let again = with_retry(&cluster, "2");
        let fake = fake_kube(&again, &state(None), &[consumed]);
        let meta = meta_for(again, &fake.client());
        ensure_at(&meta, Some(OWN)).await.expect("ensure");
        let created = created_jobs(&fake);
        assert_eq!(created.len(), 1);
        assert_eq!(created[0]["metadata"]["name"], third.as_str());
        assert_eq!(
            created[0]["metadata"]["annotations"][RETRY_VALUE_ANNOTATION],
            "2"
        );
    }

    /// A failed schema-apply Job is not recreated across further reconciles, however long ago
    /// it failed, and the status says what starts the next attempt
    #[tokio::test]
    async fn failed_job_is_not_recreated() {
        // a Job for the current key that failed at each simulated time, from now to days ago
        // and in the future
        let cluster = cluster();
        let key = key_of(&cluster);
        let ages = [0, 30, 900, 86_400, 6 * 86_400, -5 * 3600];
        for age in ages {
            let at = Utc::now() - chrono::Duration::seconds(age);
            let failed = job("schema-apply-failed", &key, 0, Some(("Failed", at)));
            let fake = fake_kube(&cluster, &state(None), &[failed]);
            let meta = meta_for(cluster.clone(), &fake.client());
            // five further reconciles create nothing and keep the rollout held
            for reconcile in 0..5 {
                let action = ensure_at(&meta, Some(OWN)).await.expect("ensure");
                assert!(action.is_some(), "{age} {reconcile}");
                assert!(
                    created_jobs(&fake).is_empty(),
                    "{age} {reconcile}: {:?}",
                    created_jobs(&fake)
                );
            }
            // the cluster is held in the error phase, naming the annotation and the key
            let status = meta.status.current();
            assert_eq!(status.phase, Some(ClusterPhase::Error), "{age}");
            let message = status.message.unwrap_or_default();
            assert!(message.contains(RETRY_ANNOTATION), "{message}");
            assert!(!message.contains("retried in"), "{message}");
            assert!(message.contains("trigger key"), "{message}");
        }
    }

    /// A changed trigger key after a failure creates one Job for the new key, also when the key
    /// comes back to one whose earlier attempt failed before another key was tried
    #[tokio::test]
    async fn new_key_after_a_failure_creates_one() {
        // a failed Job for an older image's key
        let cluster = cluster();
        let key = key_of(&cluster);
        let mut older_spec = full_spec();
        older_spec["version"] = serde_json::json!("1.8.0");
        let older = key_of(&namespaced_cluster(older_spec));
        let just_now = Utc::now() - chrono::Duration::seconds(10);
        let failed_older = job("schema-apply-older", &older, 0, Some(("Failed", just_now)));
        let fake = fake_kube(&cluster, &state(None), std::slice::from_ref(&failed_older));
        let meta = meta_for(cluster.clone(), &fake.client());
        // the current key gets exactly one Job
        ensure_at(&meta, Some(OWN)).await.expect("ensure");
        let created = created_jobs(&fake);
        assert_eq!(created.len(), 1);
        assert_eq!(
            created[0]["metadata"]["annotations"][TRIGGER_KEY_ANNOTATION],
            key.as_str()
        );
        // the current key failed first, then the older key was tried and failed after it
        let mut failed_current = job("schema-apply-current", &key, 0, Some(("Failed", just_now)));
        failed_current["metadata"]["creationTimestamp"] = serde_json::json!("2026-10-21T10:00:00Z");
        let mut failed_older = failed_older;
        failed_older["metadata"]["creationTimestamp"] = serde_json::json!("2026-10-21T10:05:00Z");
        let fake = fake_kube(&cluster, &state(None), &[failed_current, failed_older]);
        let meta = meta_for(cluster, &fake.client());
        // coming back to the current key is a key change, so it gets one new attempt
        ensure_at(&meta, Some(OWN)).await.expect("ensure");
        let created = created_jobs(&fake);
        assert_eq!(created.len(), 1);
        let annotations = &created[0]["metadata"]["annotations"];
        assert_eq!(annotations[TRIGGER_KEY_ANNOTATION], key.as_str());
        assert_eq!(annotations[ATTEMPT_ANNOTATION], "1");
    }

    /// The path the old-key Job in these tests is deleted at
    const OLD_JOB_PATH: &str = "/apis/batch/v1/namespaces/thorium/jobs/schema-apply-old";

    /// Build a `PodList` holding one thoradm pod in a phase with its container's state
    ///
    /// # Arguments
    ///
    /// * `phase` - The pod's phase
    /// * `state` - The thoradm container's state
    fn pods_in(phase: &str, state: &serde_json::Value) -> serde_json::Value {
        // one pod of the old Job with the thoradm container's status
        serde_json::json!({
            "apiVersion": "v1",
            "kind": "PodList",
            "metadata": {},
            "items": [{
                "metadata": {"name": "schema-apply-old-pod", "namespace": "thorium"},
                "status": {"phase": phase, "containerStatuses": [{
                    "name": "thoradm",
                    "ready": false,
                    "restartCount": 0,
                    "image": "registry/thorium:1.8.0",
                    "imageID": "",
                    "state": state
                }]}
            }]
        })
    }

    /// Build a fake kube API where the current key has no Job yet and an older image's key has
    /// one, with the old Job's pods and a delete route for it
    ///
    /// # Arguments
    ///
    /// * `old` - The old-key Job's terminal condition, or None while it is active
    /// * `pods` - The `PodList` the old Job's pods are listed as
    ///
    /// Returns the fake, the cluster, the current key, and the old key.
    fn old_key_fake(
        old: Option<(&str, DateTime<Utc>)>,
        pods: serde_json::Value,
    ) -> (FakeKube, ThoriumCluster, String, String) {
        // the old key is the same cluster on an older image
        let cluster = cluster();
        let key = key_of(&cluster);
        let mut older_spec = full_spec();
        older_spec["version"] = serde_json::json!("1.8.0");
        let old_key = key_of(&namespaced_cluster(older_spec));
        let old_job = job("schema-apply-old", &old_key, 0, old);
        // answer the old Job's pods and its delete
        let fake = fake_kube(&cluster, &state(None), std::slice::from_ref(&old_job)).route(
            "DELETE",
            OLD_JOB_PATH,
            200,
            old_job,
        );
        fake.set_route("GET", PODS_PATH, 200, pods);
        (fake, cluster, key, old_key)
    }

    /// Get the deletes sent through a fake kube API
    ///
    /// # Arguments
    ///
    /// * `fake` - The fake kube API
    fn deletes(fake: &FakeKube) -> Vec<FakeRequest> {
        // keep every delete request
        fake.writes()
            .into_iter()
            .filter(|request| request.method == "DELETE")
            .collect()
    }

    /// Check that the status holds the rollout until an old-key Job is deleted
    ///
    /// # Arguments
    ///
    /// * `meta` - Thorium cluster client and metadata
    /// * `job` - The name of the old-key Job being deleted
    fn assert_waiting_for_delete(meta: &ClusterMeta, job: &str) {
        // the provisioning phase names the old Job and says it is waited for until deleted
        let status = meta.status.current();
        assert_eq!(status.phase, Some(ClusterPhase::Provisioning));
        let message = status.message.unwrap_or_default();
        assert!(message.contains(job), "{message}");
        assert!(
            message.contains("of an earlier trigger key to be deleted"),
            "{message}"
        );
    }

    /// Check that the old-key Job was deleted in the foreground without creating the Job for
    /// the current key, which the next reconcile creates once the old Job is gone
    ///
    /// # Arguments
    ///
    /// * `fake` - The fake kube API
    /// * `meta` - Thorium cluster client and metadata
    /// * `key` - The current trigger key
    async fn assert_replaced_next(fake: &FakeKube, meta: &ClusterMeta, key: &str) {
        // one foreground delete of the old Job with the default grace period
        let sent = deletes(fake);
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0].path, OLD_JOB_PATH);
        assert_eq!(sent[0].body["propagationPolicy"], "Foreground");
        assert!(sent[0].body.get("gracePeriodSeconds").is_none(), "{sent:?}");
        // no Job for the current key yet, and the rollout waits for the delete
        assert!(created_jobs(fake).is_empty(), "{:?}", created_jobs(fake));
        assert_waiting_for_delete(meta, "schema-apply-old");
        // while the old Job is being deleted it is not deleted again and nothing is created
        let mut deleting = old_job();
        deleting["metadata"]["deletionTimestamp"] = serde_json::json!("2026-10-21T00:00:00Z");
        fake.set_route("GET", JOBS_PATH, 200, job_list(&[deleting]));
        ensure_at(meta, Some(OWN)).await.expect("ensure");
        assert_eq!(deletes(fake).len(), 1);
        assert!(created_jobs(fake).is_empty(), "{:?}", created_jobs(fake));
        // once the old Job is gone the next reconcile creates the Job for the current key
        fake.set_route("GET", JOBS_PATH, 200, job_list(&[]));
        ensure_at(meta, Some(OWN)).await.expect("ensure");
        let created = created_jobs(fake);
        assert_eq!(created.len(), 1);
        assert_eq!(
            created[0]["metadata"]["annotations"][TRIGGER_KEY_ANNOTATION],
            key
        );
        assert_eq!(deletes(fake).len(), 1);
    }

    /// Build the old-key Job of [`old_key_fake`] while it is active
    fn old_job() -> serde_json::Value {
        // the old key is the same cluster on an older image
        let mut older_spec = full_spec();
        older_spec["version"] = serde_json::json!("1.8.0");
        let old_key = key_of(&namespaced_cluster(older_spec));
        job("schema-apply-old", &old_key, 0, None)
    }

    /// An active Job for an old key whose pod is stuck pulling its image is deleted in the
    /// foreground, and the Job for the new key is created on a later reconcile once it is gone
    #[tokio::test]
    async fn stuck_old_key_job_is_deleted() {
        // the old Job's only pod backs off from pulling a mistyped image
        let waiting = serde_json::json!({"waiting": {"reason": "ImagePullBackOff"}});
        let (fake, cluster, key, _) = old_key_fake(None, pods_in("Pending", &waiting));
        let meta = meta_for(cluster, &fake.client());
        let action = ensure_at(&meta, Some(OWN)).await.expect("ensure");
        assert!(action.is_some());
        assert_replaced_next(&fake, &meta, &key).await;
    }

    /// An active Job for an old key that has no pods yet is deleted and replaced once it is gone
    #[tokio::test]
    async fn old_key_job_without_pods_is_deleted() {
        // the old Job has no pods
        let none =
            serde_json::json!({"apiVersion": "v1", "kind": "PodList", "metadata": {}, "items": []});
        let (fake, cluster, key, _) = old_key_fake(None, none);
        let meta = meta_for(cluster, &fake.client());
        ensure_at(&meta, Some(OWN)).await.expect("ensure");
        assert_replaced_next(&fake, &meta, &key).await;
    }

    /// Every active old-key Job whose pods never started thoradm is deleted in one reconcile,
    /// and nothing is created while they are deleted
    #[tokio::test]
    async fn every_stuck_old_key_job_is_deleted() {
        // two old-key Jobs, neither with pods
        let none =
            serde_json::json!({"apiVersion": "v1", "kind": "PodList", "metadata": {}, "items": []});
        let (fake, cluster, _, old_key) = old_key_fake(None, none);
        let first = job("schema-apply-old", &old_key, 0, None);
        let second = job("schema-apply-old-2", &old_key, 1, None);
        let second_path = format!("{JOBS_PATH}/schema-apply-old-2");
        let fake = fake.route("DELETE", &second_path, 200, second.clone());
        fake.set_route("GET", JOBS_PATH, 200, job_list(&[first, second]));
        let meta = meta_for(cluster, &fake.client());
        ensure_at(&meta, Some(OWN)).await.expect("ensure");
        // both are deleted and nothing is created
        let mut paths: Vec<String> = deletes(&fake)
            .into_iter()
            .map(|request| request.path)
            .collect();
        paths.sort();
        assert_eq!(paths, [OLD_JOB_PATH.to_owned(), second_path]);
        assert!(created_jobs(&fake).is_empty(), "{:?}", created_jobs(&fake));
        assert_waiting_for_delete(&meta, "schema-apply-old");
    }

    /// An active Job for an old key whose thoradm container is running is never deleted: the
    /// rollout waits for it and the status names the Job, its key, and that it is running
    #[tokio::test]
    async fn running_old_key_job_is_waited_for() {
        // the old Job's pod is running thoradm
        let running = serde_json::json!({"running": {"startedAt": "2026-10-21T00:00:00Z"}});
        let (fake, cluster, _, old_key) = old_key_fake(None, pods_in("Running", &running));
        let meta = meta_for(cluster, &fake.client());
        // across reconciles nothing is deleted or created and the rollout is held
        for reconcile in 0..3 {
            let action = ensure_at(&meta, Some(OWN)).await.expect("ensure");
            assert!(action.is_some(), "{reconcile}");
            assert!(deletes(&fake).is_empty(), "{:?}", deletes(&fake));
            assert!(created_jobs(&fake).is_empty(), "{:?}", created_jobs(&fake));
        }
        // the status names the old Job and its key and says it is still running
        let status = meta.status.current();
        assert_eq!(status.phase, Some(ClusterPhase::Provisioning));
        let message = status.message.unwrap_or_default();
        assert!(message.contains("schema-apply-old"), "{message}");
        assert!(message.contains(&old_key), "{message}");
        assert!(message.contains("still running"), "{message}");
    }

    /// An old-key Job already being deleted, whose pods never started thoradm, is not deleted
    /// again, and the Job for the new key waits until it is gone
    #[tokio::test]
    async fn deleting_old_key_job_is_not_deleted_again() {
        // the old Job is being deleted and has no pods left
        let none =
            serde_json::json!({"apiVersion": "v1", "kind": "PodList", "metadata": {}, "items": []});
        let (fake, cluster, key, old_key) = old_key_fake(None, none);
        let mut deleting = job("schema-apply-old", &old_key, 0, None);
        deleting["metadata"]["deletionTimestamp"] = serde_json::json!("2026-10-21T00:00:00Z");
        fake.set_route("GET", JOBS_PATH, 200, job_list(&[deleting]));
        let meta = meta_for(cluster, &fake.client());
        // nothing is deleted or created while it is being deleted
        for reconcile in 0..2 {
            ensure_at(&meta, Some(OWN)).await.expect("ensure");
            assert!(
                deletes(&fake).is_empty(),
                "{reconcile}: {:?}",
                deletes(&fake)
            );
            assert!(
                created_jobs(&fake).is_empty(),
                "{reconcile}: {:?}",
                created_jobs(&fake)
            );
        }
        assert_waiting_for_delete(&meta, "schema-apply-old");
        // once it is gone the current key gets its Job
        fake.set_route("GET", JOBS_PATH, 200, job_list(&[]));
        ensure_at(&meta, Some(OWN)).await.expect("ensure");
        let created = created_jobs(&fake);
        assert_eq!(created.len(), 1);
        assert_eq!(
            created[0]["metadata"]["annotations"][TRIGGER_KEY_ANNOTATION],
            key.as_str()
        );
    }

    /// An old-key Job being deleted whose pod still runs thoradm is reported as running, and
    /// nothing is created until its pods are gone
    #[tokio::test]
    async fn deleting_old_key_job_with_a_running_pod_is_waited_for() {
        // the old Job is being deleted while its pod runs thoradm
        let running = serde_json::json!({"running": {"startedAt": "2026-10-21T00:00:00Z"}});
        let (fake, cluster, _, old_key) = old_key_fake(None, pods_in("Running", &running));
        let mut deleting = job("schema-apply-old", &old_key, 0, None);
        deleting["metadata"]["deletionTimestamp"] = serde_json::json!("2026-10-21T00:00:00Z");
        fake.set_route("GET", JOBS_PATH, 200, job_list(&[deleting]));
        let meta = meta_for(cluster, &fake.client());
        // across reconciles its pods are checked and nothing is deleted or created
        for reconcile in 0..2 {
            ensure_at(&meta, Some(OWN)).await.expect("ensure");
            assert!(
                deletes(&fake).is_empty(),
                "{reconcile}: {:?}",
                deletes(&fake)
            );
            assert!(
                created_jobs(&fake).is_empty(),
                "{reconcile}: {:?}",
                created_jobs(&fake)
            );
        }
        // the old Job's pods were checked on each reconcile
        let pod_lists = fake
            .requests()
            .iter()
            .filter(|request| request.method == "GET" && request.path == PODS_PATH)
            .count();
        assert_eq!(pod_lists, 2);
        // the status says it is still running
        let message = meta.status.current().message.unwrap_or_default();
        assert!(message.contains("still running"), "{message}");
    }

    /// Failed and complete Jobs for an old key are left to their TTL while the Job for the new
    /// key is created
    #[tokio::test]
    async fn finished_old_key_jobs_are_left() {
        for condition in ["Failed", "Complete"] {
            // the old Job finished and its pod is gone
            let none = serde_json::json!({"apiVersion": "v1", "kind": "PodList", "metadata": {}, "items": []});
            let (fake, cluster, key, _) = old_key_fake(Some((condition, Utc::now())), none);
            let meta = meta_for(cluster, &fake.client());
            ensure_at(&meta, Some(OWN)).await.expect("ensure");
            // nothing is deleted and the current key gets its Job
            assert!(
                deletes(&fake).is_empty(),
                "{condition}: {:?}",
                deletes(&fake)
            );
            let created = created_jobs(&fake);
            assert_eq!(created.len(), 1, "{condition}");
            assert_eq!(
                created[0]["metadata"]["annotations"][TRIGGER_KEY_ANNOTATION],
                key.as_str(),
                "{condition}"
            );
        }
    }

    /// Only versioned or digest-pinned image tags count as immutable
    #[test]
    fn mutable_tag_rule() {
        // empty, latest, and other names are mutable
        for tag in [
            "", " ", "latest", "Latest", "main", "dev", "nightly", "v", "vnext",
        ] {
            assert!(mutable_tag(tag), "{tag:?}");
        }
        // versions, with or without a v, and digests are not
        for tag in [
            "1.8.1",
            "v1.8.1",
            "V2",
            "1.8.1-rc1",
            "2026-10-21",
            "latest@sha256:0123abcd",
        ] {
            assert!(!mutable_tag(tag), "{tag:?}");
        }
    }

    /// Get the mutable tag condition of a cluster's status, if it has one
    ///
    /// # Arguments
    ///
    /// * `meta` - Thorium cluster client and metadata
    fn tag_condition(meta: &ClusterMeta) -> Option<upgrades::UpgradeCondition> {
        // find the condition by its type
        meta.status
            .current()
            .upgrade?
            .conditions
            .into_iter()
            .find(|condition| condition.kind == UpgradeConditionType::MutableImageTag)
    }

    /// A cluster on the latest tag gets the mutable tag condition whether or not the Job runs,
    /// and a versioned tag clears it
    #[tokio::test]
    async fn mutable_tag_raises_a_condition() {
        // a cluster on the latest tag
        let mut latest_spec = full_spec();
        latest_spec["version"] = serde_json::json!("latest");
        let latest = namespaced_cluster(latest_spec);
        let key = key_of(&latest);
        // with nothing to run and with a Job to run, the condition holds with what to do
        for recorded in [Some(key.as_str()), None] {
            let fake = fake_kube(&latest, &state(recorded), &[]);
            let meta = meta_for(latest.clone(), &fake.client());
            ensure_at(&meta, Some(OWN)).await.expect("ensure");
            let condition = tag_condition(&meta).expect("condition");
            assert_eq!(condition.status, "True", "{recorded:?}");
            assert_eq!(
                condition.message,
                "Image tag is mutable: schema apply only runs when its trigger key changes. Set \
                 thorium.sandia.gov/retry-schema-apply after pulling a new image."
            );
        }
        // a versioned tag clears a condition left in the status
        let mut versioned = cluster();
        versioned.status = Some(crds::ThoriumClusterStatus {
            upgrade: Some(upgrades::UpgradeStatus {
                conditions: vec![upgrades::UpgradeCondition {
                    kind: UpgradeConditionType::MutableImageTag,
                    status: "True".to_owned(),
                    message: "left over".to_owned(),
                }],
                ..Default::default()
            }),
            ..Default::default()
        });
        let key = key_of(&versioned);
        let fake = fake_kube(&versioned, &state(Some(&key)), &[]);
        let meta = meta_for(versioned, &fake.client());
        ensure_at(&meta, Some(OWN)).await.expect("ensure");
        let condition = tag_condition(&meta).expect("condition");
        assert_eq!(condition.status, "False");
        // a versioned tag that never had the condition gets none
        let fake = fake_kube(&cluster(), &state(Some(&key)), &[]);
        let meta = meta_for(cluster(), &fake.client());
        ensure_at(&meta, Some(OWN)).await.expect("ensure");
        assert_eq!(tag_condition(&meta), None);
    }

    /// A failed attempt made before another key was applied doesn't hold the current state:
    /// the key coming back gets a new attempt
    #[tokio::test]
    async fn failure_over_another_apply_is_retried() {
        // the current key failed over a fresh install, then another key was applied and its
        // Job is gone
        let cluster = cluster();
        let key = key_of(&cluster);
        let mut older_spec = full_spec();
        older_spec["version"] = serde_json::json!("1.8.0");
        let other = key_of(&namespaced_cluster(older_spec));
        let failed = job("schema-apply-failed", &key, 0, Some(("Failed", Utc::now())));
        let fake = fake_kube(&cluster, &state(Some(&other)), &[failed]);
        let meta = meta_for(cluster, &fake.client());
        // the current key gets its next attempt, applied over the other key
        ensure_at(&meta, Some(OWN)).await.expect("ensure");
        let created = created_jobs(&fake);
        assert_eq!(created.len(), 1);
        let annotations = &created[0]["metadata"]["annotations"];
        assert_eq!(annotations[ATTEMPT_ANNOTATION], "1");
        assert_eq!(annotations[APPLIED_OVER_ANNOTATION], other.as_str());
    }

    /// A Job name that already exists when the Job is created is the same attempt, which holds
    /// the rollout like a running Job instead of failing the reconcile
    #[tokio::test]
    async fn existing_job_name_is_not_an_error() {
        // the first attempt's name already exists, though the Job list doesn't show it yet
        let cluster = cluster();
        let named = meta_for(cluster.clone(), &FakeKube::default().client());
        let first = spec(&named, &state(None).revision, &key_of(&cluster), 0, None).expect("spec");
        let fake =
            fake_kube(&cluster, &state(None), &[]).unique_names(JOBS_PATH, &[first.name.as_str()]);
        let meta = meta_for(cluster, &fake.client());
        // creating that Job again is answered with a conflict and reported as not created
        assert!(!jobs::create(&first, &meta).await.expect("create"));
        // the reconcile tries to create it, gets the conflict, and holds the rollout
        let action = ensure_at(&meta, Some(OWN)).await.expect("ensure");
        assert!(action.is_some());
        assert_eq!(created_jobs(&fake).len(), 2);
        let status = meta.status.current();
        assert_eq!(status.phase, Some(ClusterPhase::Provisioning));
        assert!(
            status.message.unwrap_or_default().contains(&first.name),
            "{:?}",
            meta.status.current().message
        );
    }

    /// A failed schema-apply Job is reported in the status and holds the rollout, and the
    /// failure is cleared once an apply succeeds
    #[tokio::test]
    async fn failed_job_is_reported_and_holds() {
        // a Job for the current key that just failed
        let cluster = cluster();
        let key = key_of(&cluster);
        let now = Utc::now();
        let failed = job(
            "schema-apply-schema-apply-0",
            &key,
            0,
            Some(("Failed", now)),
        );
        let fake = fake_kube(&cluster, &state(None), &[failed]);
        let meta = meta_for(cluster.clone(), &fake.client());
        // the rollout is held
        let action = ensure_at(&meta, Some(OWN)).await.expect("ensure");
        assert!(action.is_some());
        // the status names the failed Job, its pod, and the retry annotation
        let status = meta.status.current();
        assert_eq!(status.phase, Some(ClusterPhase::Error));
        let message = status.message.unwrap_or_default();
        assert!(message.contains("schema-apply-schema-apply-0"), "{message}");
        assert!(message.contains(RETRY_ANNOTATION), "{message}");
        let failure = status
            .upgrade
            .and_then(|upgrade| upgrade.failure)
            .expect("failure");
        assert_eq!(failure.job, "schema-apply-schema-apply-0");
        assert_eq!(failure.pod.as_deref(), Some("schema-apply-pod"));
        // the retry completed, so the apply is recorded and the failure cleared
        let done = job("schema-apply-retry", &key, 1, Some(("Complete", now)));
        fake.set_route("GET", JOBS_PATH, 200, job_list(&[done]));
        let action = ensure_at(&meta, Some(OWN)).await.expect("record");
        assert!(action.is_none());
        let upgrade = meta.status.current().upgrade.expect("upgrade status");
        assert_eq!(upgrade.failure, None);
        // a failure left in the status is cleared when the recorded key is current
        let mut stale = cluster;
        stale.status = Some(crds::ThoriumClusterStatus {
            upgrade: Some(upgrades::UpgradeStatus {
                failure: Some(failure),
                ..Default::default()
            }),
            ..Default::default()
        });
        let fake = fake_kube(&stale, &state(Some(&key)), &[]);
        let meta = meta_for(stale, &fake.client());
        let action = ensure_at(&meta, Some(OWN)).await.expect("current");
        assert!(action.is_none());
        let upgrade = meta.status.current().upgrade.expect("upgrade status");
        assert_eq!(upgrade.failure, None);
    }

    /// The failure reason the held failures in these tests record
    const HELD_REASON: &str = "PodFailurePolicy: fake";

    /// Build the upgrade state of a cluster whose failed schema-apply attempt holds the rollout
    ///
    /// # Arguments
    ///
    /// * `key` - The trigger key the attempt was made for
    /// * `attempt` - The attempt counter
    /// * `job` - The failed Job's name
    fn held_state(key: &str, attempt: u32, job: &str) -> UpgradeState {
        // record the held failure as the operator does, with no applied key
        let mut state = state(None);
        let held =
            serde_json::json!({"key": key, "attempt": attempt, "job": job, "reason": HELD_REASON});
        state
            .schema
            .insert(HELD_FAILURE_MARKER.to_owned(), held.to_string());
        state
    }

    /// Get the held failure a saved state records, as JSON
    ///
    /// # Arguments
    ///
    /// * `state` - The saved state
    fn held_of(state: &UpgradeState) -> Option<serde_json::Value> {
        // parse the held failure marker
        state
            .schema
            .get(HELD_FAILURE_MARKER)
            .map(|raw| serde_json::from_str(raw).expect("held failure parses"))
    }

    /// Give a cluster a failure in its status, like one reported earlier
    ///
    /// # Arguments
    ///
    /// * `cluster` - The cluster to give the failure
    /// * `failure` - The failure to report
    fn with_failure(cluster: &ThoriumCluster, failure: &JobFailure) -> ThoriumCluster {
        // set the upgrade failure in the cluster's status
        let mut failed = cluster.clone();
        failed.status = Some(crds::ThoriumClusterStatus {
            upgrade: Some(upgrades::UpgradeStatus {
                failure: Some(failure.clone()),
                ..Default::default()
            }),
            ..Default::default()
        });
        failed
    }

    /// Holding a failed attempt records it in the state with the retry value it used, and a
    /// reconcile reading that state saves nothing more
    #[tokio::test]
    async fn held_failure_is_recorded() {
        // the third attempt for the current key failed after the retry value r1 asked for it
        let cluster = with_retry(&cluster(), "r1");
        let key = key_of(&cluster);
        let mut failed = job("schema-apply-third", &key, 2, Some(("Failed", Utc::now())));
        failed["metadata"]["annotations"][RETRY_VALUE_ANNOTATION] = serde_json::json!("r1");
        let fake = fake_kube(&cluster, &state(None), std::slice::from_ref(&failed));
        let meta = meta_for(cluster.clone(), &fake.client());
        ensure_at(&meta, Some(OWN)).await.expect("ensure");
        // the state records the key, the attempt, the Job, why it failed, and the retry value
        let recorded = saved(&fake).expect("state saved");
        let held = held_of(&recorded).expect("held failure");
        assert_eq!(held["key"], key.as_str());
        assert_eq!(held["attempt"], 2);
        assert_eq!(held["job"], "schema-apply-third");
        assert!(
            held["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("PodFailurePolicy"),
            "{held}"
        );
        assert_eq!(
            recorded.schema.get(RETRY_VALUE_MARKER).map(String::as_str),
            Some("r1")
        );
        // a reconcile reading that state saves nothing and creates nothing
        let fake = fake_kube(&cluster, &recorded, &[failed]);
        let meta = meta_for(cluster, &fake.client());
        ensure_at(&meta, Some(OWN)).await.expect("ensure");
        assert!(saved(&fake).is_none());
        assert!(created_jobs(&fake).is_empty(), "{:?}", created_jobs(&fake));
        assert_eq!(meta.status.current().phase, Some(ClusterPhase::Error));
    }

    /// A held failure keeps holding the rollout with its failure in the status after its Job is
    /// deleted or while its TTL deletes it, and only a new retry value or trigger key creates
    /// the next attempt
    #[tokio::test]
    async fn held_failure_outlives_its_job() {
        // the first attempt for the current key failed and is held, and its Job is gone
        let base = cluster();
        let key = key_of(&base);
        let failure = JobFailure {
            job: "schema-apply-first".to_owned(),
            pod: Some("schema-apply-first-pod".to_owned()),
            log_tail: vec!["refused".to_owned()],
        };
        let cluster = with_failure(&base, &failure);
        let held = held_state(&key, 0, "schema-apply-first");
        let fake = fake_kube(&cluster, &held, &[]);
        let meta = meta_for(cluster.clone(), &fake.client());
        // reconciles create nothing and keep the failure and the error phase
        for reconcile in 0..5 {
            let action = ensure_at(&meta, Some(OWN)).await.expect("ensure");
            assert!(action.is_some(), "{reconcile}");
            assert!(
                created_jobs(&fake).is_empty(),
                "{reconcile}: {:?}",
                created_jobs(&fake)
            );
        }
        // the status names the failed Job and the annotation and keeps the reported failure
        let status = meta.status.current();
        assert_eq!(status.phase, Some(ClusterPhase::Error));
        let message = status.message.unwrap_or_default();
        assert!(message.contains("schema-apply-first"), "{message}");
        assert!(message.contains(RETRY_ANNOTATION), "{message}");
        assert_eq!(
            status.upgrade.and_then(|upgrade| upgrade.failure),
            Some(failure.clone())
        );
        // while the TTL deletes the failed Job the rollout stays held in the error phase
        let mut expiring = job("schema-apply-first", &key, 0, Some(("Failed", Utc::now())));
        expiring["metadata"]["deletionTimestamp"] = serde_json::json!("2026-10-21T00:00:00Z");
        fake.set_route("GET", JOBS_PATH, 200, job_list(&[expiring]));
        for reconcile in 0..3 {
            ensure_at(&meta, Some(OWN)).await.expect("ensure");
            assert!(
                created_jobs(&fake).is_empty(),
                "{reconcile}: {:?}",
                created_jobs(&fake)
            );
            assert_eq!(
                meta.status.current().phase,
                Some(ClusterPhase::Error),
                "{reconcile}"
            );
        }
        // a retry value creates the next attempt under a new name and drops the held failure
        let retried = with_retry(&cluster, "go");
        let fake = fake_kube(&retried, &held, &[]);
        let meta = meta_for(retried, &fake.client());
        ensure_at(&meta, Some(OWN)).await.expect("ensure");
        let created = created_jobs(&fake);
        assert_eq!(created.len(), 1);
        let annotations = &created[0]["metadata"]["annotations"];
        assert_eq!(annotations[ATTEMPT_ANNOTATION], "1");
        assert_eq!(annotations[RETRY_VALUE_ANNOTATION], "go");
        let recorded = saved(&fake).expect("state saved");
        assert_eq!(held_of(&recorded), None);
        // a new trigger key creates its Job
        let mut newer_spec = full_spec();
        newer_spec["version"] = serde_json::json!("1.8.2");
        let newer = namespaced_cluster(newer_spec);
        let fake = fake_kube(&newer, &held, &[]);
        let meta = meta_for(newer.clone(), &fake.client());
        ensure_at(&meta, Some(OWN)).await.expect("ensure");
        let created = created_jobs(&fake);
        assert_eq!(created.len(), 1);
        assert_eq!(
            created[0]["metadata"]["annotations"][TRIGGER_KEY_ANNOTATION],
            key_of(&newer).as_str()
        );
    }

    /// A successful attempt after a held failure drops the held failure with the apply it
    /// records
    #[tokio::test]
    async fn success_drops_the_held_failure() {
        // the retried attempt for the held key completed
        let cluster = with_retry(&cluster(), "go");
        let key = key_of(&cluster);
        let mut done = job(
            "schema-apply-second",
            &key,
            1,
            Some(("Complete", Utc::now())),
        );
        done["metadata"]["annotations"][RETRY_VALUE_ANNOTATION] = serde_json::json!("go");
        let fake = fake_kube(
            &cluster,
            &held_state(&key, 0, "schema-apply-first"),
            &[done],
        );
        let meta = meta_for(cluster, &fake.client());
        let action = ensure_at(&meta, Some(OWN)).await.expect("ensure");
        assert!(action.is_none());
        // the apply is recorded and the held failure dropped
        let recorded = saved(&fake).expect("state saved");
        assert_eq!(recorded.schema.get(APPLY_KEY_MARKER), Some(&key));
        assert_eq!(held_of(&recorded), None);
    }

    /// Build the upgrade state of a cluster with an applied key and the last retry value used
    ///
    /// # Arguments
    ///
    /// * `key` - The trigger key the last successful apply recorded
    /// * `last` - The last retry annotation value an attempt used, if any
    fn retried_state(key: &str, last: Option<&str>) -> UpgradeState {
        // record the key and the retry value
        let mut state = state(Some(key));
        if let Some(last) = last {
            state
                .schema
                .insert(RETRY_VALUE_MARKER.to_owned(), last.to_owned());
        }
        state
    }

    /// Build a schema-apply Job for the current key made over that same key's apply after a
    /// retry value asked for it
    ///
    /// # Arguments
    ///
    /// * `name` - The Job's name
    /// * `key` - The trigger key, which is also the applied key it was made over
    /// * `attempt` - Its attempt counter
    /// * `retry` - The retry value it used
    /// * `condition` - Its terminal condition, or None while it runs
    fn forced_job(
        name: &str,
        key: &str,
        attempt: u32,
        retry: &str,
        condition: Option<&str>,
    ) -> serde_json::Value {
        // annotate the Job with what it applied over and the retry value it used
        let mut forced = job(name, key, attempt, condition.map(|kind| (kind, Utc::now())));
        forced["metadata"]["annotations"][APPLIED_OVER_ANNOTATION] = serde_json::json!(key);
        forced["metadata"]["annotations"][RETRY_VALUE_ANNOTATION] = serde_json::json!(retry);
        forced
    }

    /// A new retry annotation value runs exactly one schema-apply Job over an applied key on a
    /// mutable tag, which records the value with the apply, and repeating the value runs none
    #[tokio::test]
    async fn retry_annotation_forces_one_apply() {
        // a cluster on latest whose key is applied, with a Job left from forcing pulled-0
        let mut latest_spec = full_spec();
        latest_spec["version"] = serde_json::json!("latest");
        let latest = namespaced_cluster(latest_spec);
        let key = key_of(&latest);
        let stale = forced_job(
            "schema-apply-pulled-0",
            &key,
            0,
            "pulled-0",
            Some("Complete"),
        );
        // a new image was pulled and the retry annotation set to pulled-1
        let retried = with_retry(&latest, "pulled-1");
        let before = retried_state(&key, Some("pulled-0"));
        let fake = fake_kube(&retried, &before, std::slice::from_ref(&stale));
        let meta = meta_for(retried.clone(), &fake.client());
        let action = ensure_at(&meta, Some(OWN)).await.expect("ensure");
        assert!(action.is_some());
        // exactly one Job, the next attempt for the key made over it with the new value
        let created = created_jobs(&fake);
        assert_eq!(created.len(), 1);
        let annotations = &created[0]["metadata"]["annotations"];
        assert_eq!(annotations[TRIGGER_KEY_ANNOTATION], key.as_str());
        assert_eq!(annotations[APPLIED_OVER_ANNOTATION], key.as_str());
        assert_eq!(annotations[RETRY_VALUE_ANNOTATION], "pulled-1");
        assert_eq!(annotations[ATTEMPT_ANNOTATION], "1");
        assert_eq!(
            meta.status.current().phase,
            Some(ClusterPhase::Provisioning)
        );
        // once it completed the apply and the retry value are recorded and nothing more runs
        let name = created[0]["metadata"]["name"].as_str().expect("name");
        let done = forced_job(name, &key, 1, "pulled-1", Some("Complete"));
        fake.set_route("GET", JOBS_PATH, 200, job_list(&[stale, done]));
        let action = ensure_at(&meta, Some(OWN)).await.expect("record");
        assert!(action.is_none());
        let recorded = saved(&fake).expect("state saved");
        assert_eq!(recorded.schema.get(APPLY_KEY_MARKER), Some(&key));
        assert_eq!(
            recorded.schema.get(RETRY_VALUE_MARKER).map(String::as_str),
            Some("pulled-1")
        );
        assert_eq!(created_jobs(&fake).len(), 1);
        // the same value again runs nothing and lists no Jobs, across reconciles
        let fake = fake_kube(&retried, &recorded, &[]);
        let meta = meta_for(retried, &fake.client());
        for reconcile in 0..3 {
            let action = ensure_at(&meta, Some(OWN)).await.expect("ensure");
            assert!(action.is_none(), "{reconcile}");
        }
        assert!(created_jobs(&fake).is_empty(), "{:?}", created_jobs(&fake));
        assert!(
            fake.requests()
                .iter()
                .all(|request| request.path != JOBS_PATH)
        );
    }

    /// A retry value forces an apply when it differs from the last one used in any way,
    /// whitespace, case, or emptiness included, and an unchanged or missing one never does
    #[tokio::test]
    async fn retry_value_differs_from_the_last_used() {
        // the key is applied and the last value used was a
        let cluster = cluster();
        let key = key_of(&cluster);
        let cases = [
            (Some("a"), Some("a"), 0),
            (None, Some("a"), 0),
            (Some(" a"), Some("a"), 1),
            (Some("a "), Some("a"), 1),
            (Some("A"), Some("a"), 1),
            (Some(""), Some("a"), 1),
            (Some(""), None, 1),
            (Some("a"), Some("b"), 1),
            (None, None, 0),
        ];
        for (value, last, expected) in cases {
            let annotated =
                value.map_or_else(|| cluster.clone(), |value| with_retry(&cluster, value));
            let fake = fake_kube(&annotated, &retried_state(&key, last), &[]);
            let meta = meta_for(annotated, &fake.client());
            ensure_at(&meta, Some(OWN)).await.expect("ensure");
            assert_eq!(
                created_jobs(&fake).len(),
                expected,
                "{value:?} after {last:?}"
            );
        }
    }

    /// A retry value already used doesn't retry a failed attempt that didn't use it, such as
    /// one for a new key created before the value was set again
    #[tokio::test]
    async fn used_retry_value_does_not_retry_a_failure() {
        // the value a was used before, and the attempt for a new key failed without a value
        let cluster = with_retry(&cluster(), "a");
        let key = key_of(&cluster);
        let mut older_spec = full_spec();
        older_spec["version"] = serde_json::json!("1.8.0");
        let older = key_of(&namespaced_cluster(older_spec));
        let mut failed = job("schema-apply-new", &key, 0, Some(("Failed", Utc::now())));
        failed["metadata"]["annotations"][APPLIED_OVER_ANNOTATION] = serde_json::json!(older);
        let fake = fake_kube(&cluster, &retried_state(&older, Some("a")), &[failed]);
        let meta = meta_for(cluster, &fake.client());
        // the failure holds and nothing is created
        for reconcile in 0..3 {
            let action = ensure_at(&meta, Some(OWN)).await.expect("ensure");
            assert!(action.is_some(), "{reconcile}");
            assert!(
                created_jobs(&fake).is_empty(),
                "{reconcile}: {:?}",
                created_jobs(&fake)
            );
        }
        assert_eq!(meta.status.current().phase, Some(ClusterPhase::Error));
    }

    /// A forced apply that fails over an applied key holds the rollout like any failed attempt,
    /// from the state once it is recorded, until another retry value
    #[tokio::test]
    async fn failed_forced_apply_holds() {
        // the value x forced an attempt over the applied key, and it failed
        let cluster = with_retry(&cluster(), "x");
        let key = key_of(&cluster);
        let failed = forced_job("schema-apply-forced", &key, 0, "x", Some("Failed"));
        let fake = fake_kube(&cluster, &retried_state(&key, None), &[failed]);
        let meta = meta_for(cluster.clone(), &fake.client());
        let action = ensure_at(&meta, Some(OWN)).await.expect("ensure");
        assert!(action.is_some());
        assert!(created_jobs(&fake).is_empty(), "{:?}", created_jobs(&fake));
        assert_eq!(meta.status.current().phase, Some(ClusterPhase::Error));
        // the held failure and the value are recorded
        let recorded = saved(&fake).expect("state saved");
        assert_eq!(
            held_of(&recorded).expect("held")["job"],
            "schema-apply-forced"
        );
        assert_eq!(
            recorded.schema.get(RETRY_VALUE_MARKER).map(String::as_str),
            Some("x")
        );
        // with the Job gone the recorded failure still holds the rollout
        let fake = fake_kube(&cluster, &recorded, &[]);
        let meta = meta_for(cluster, &fake.client());
        let action = ensure_at(&meta, Some(OWN)).await.expect("ensure");
        assert!(action.is_some());
        assert!(created_jobs(&fake).is_empty(), "{:?}", created_jobs(&fake));
        assert_eq!(meta.status.current().phase, Some(ClusterPhase::Error));
    }

    /// A Job reporting a fingerprint other than the operator's is recorded with its key, runs
    /// once across later reconciles and after its TTL removed it, and raises the skew condition
    #[tokio::test]
    async fn skewed_fingerprint_runs_the_job_once() {
        // the first reconcile of a fresh install creates the Job
        let cluster = cluster();
        let key = key_of(&cluster);
        let fake = fake_kube(&cluster, &state(None), &[]);
        let meta = meta_for(cluster.clone(), &fake.client());
        ensure_at(&meta, Some(OWN)).await.expect("first");
        assert_eq!(created_jobs(&fake).len(), 1);
        // the Job finished, reporting the image's own fingerprint
        let finished = job(
            "schema-apply-schema-apply-0",
            &key,
            0,
            Some(("Complete", Utc::now())),
        );
        fake.set_route("GET", JOBS_PATH, 200, job_list(&[finished]));
        let message = serde_json::json!({"outcome": "done", "fingerprint": REPORTED}).to_string();
        fake.set_route("GET", PODS_PATH, 200, finished_pods(&message));
        // the next reconcile records the reported fingerprint and the key, holding nothing
        let action = ensure_at(&meta, Some(OWN)).await.expect("record");
        assert!(action.is_none());
        let recorded = saved(&fake).expect("state saved");
        assert_eq!(recorded.schema.get(APPLY_KEY_MARKER), Some(&key));
        assert_eq!(
            recorded.schema.get(DEPLOYMENT_MARKER).map(String::as_str),
            Some(REPORTED)
        );
        // later reconciles read the recorded state and run nothing, even once the TTL removed the Job
        fake.set_route("GET", STATE_PATH, 200, state_config_map(&recorded));
        for reconcile in 0..3 {
            let action = ensure_at(&meta, Some(OWN)).await.expect("again");
            assert!(action.is_none(), "{reconcile}");
        }
        fake.set_route("GET", JOBS_PATH, 200, job_list(&[]));
        ensure_at(&meta, Some(OWN)).await.expect("after ttl");
        assert_eq!(created_jobs(&fake).len(), 1);
        // the skew condition names both fingerprints
        let conditions = meta
            .status
            .current()
            .upgrade
            .map_or_else(Vec::new, |upgrade| upgrade.conditions);
        let skew = conditions
            .iter()
            .find(|condition| condition.kind == UpgradeConditionType::SchemaSkew)
            .expect("skew condition");
        assert_eq!(skew.status, "True");
        assert!(
            skew.message.contains(REPORTED) && skew.message.contains(OWN),
            "{}",
            skew.message
        );
    }

    /// The fingerprint the operator's code declares in the tests that inject one
    const DECLARED: &str = "00000000000000000000000000000000000000000000000000000000000000de";

    /// Give this thread's operator build a declared deployment fingerprint
    ///
    /// # Arguments
    ///
    /// * `fingerprint` - The declared deployment fingerprint to inject
    fn inject_declared(fingerprint: &str) {
        // replace whatever an earlier test on this thread injected
        DECLARED_OVERRIDE.with(|declared| *declared.borrow_mut() = Some(fingerprint.to_owned()));
    }

    /// Get the schema skew condition of a cluster's status, if it has one
    ///
    /// # Arguments
    ///
    /// * `meta` - Thorium cluster client and metadata
    fn skew_condition(meta: &ClusterMeta) -> Option<upgrades::UpgradeCondition> {
        // find the condition by its type
        meta.status
            .current()
            .upgrade?
            .conditions
            .into_iter()
            .find(|condition| condition.kind == UpgradeConditionType::SchemaSkew)
    }

    /// Build the state of a successful apply for a key that recorded a reported fingerprint
    ///
    /// # Arguments
    ///
    /// * `key` - The trigger key the apply ran for
    /// * `reported` - The deployment fingerprint the Job reported
    fn applied_state(key: &str, reported: &str) -> UpgradeState {
        // record the key and the fingerprint like a recorded success does
        let mut recorded = state(Some(key));
        recorded
            .schema
            .insert(DEPLOYMENT_MARKER.to_owned(), reported.to_owned());
        recorded
    }

    /// Without an injected fingerprint, the operator's declared fingerprint is the deployment
    /// fingerprint the schema apply records for the declared schema
    #[test]
    fn declared_fingerprint_is_what_schema_apply_records() {
        // clear whatever an earlier test on this thread injected
        DECLARED_OVERRIDE.with(|declared| *declared.borrow_mut() = None);
        // the schema apply records the declared objects' deployment fingerprint
        let (deployment, _) = fingerprints(&SchemaObject::declared());
        assert_eq!(declared_fingerprint(), deployment.as_str());
        assert_eq!(build_fingerprint(), declared_fingerprint());
    }

    /// The operator compares a Job's fingerprint with the schema its own code declares, not
    /// with a released revision's committed snapshot
    #[test]
    fn build_fingerprint_is_the_declared_one() {
        // with a declared fingerprint, the build's fingerprint is that one
        inject_declared(DECLARED);
        assert_eq!(build_fingerprint(), DECLARED);
        assert_eq!(build_fingerprint(), declared_fingerprint());
    }

    /// A Job reporting the fingerprint the operator's code declares raises no skew condition
    #[tokio::test]
    async fn declared_fingerprint_raises_no_skew() {
        // the first reconcile of a fresh install creates the Job
        inject_declared(DECLARED);
        let cluster = cluster();
        let key = key_of(&cluster);
        let fake = fake_kube(&cluster, &state(None), &[]);
        let meta = meta_for(cluster, &fake.client());
        ensure(&meta).await.expect("first");
        assert_eq!(created_jobs(&fake).len(), 1);
        // the Job finished, reporting the declared fingerprint
        let finished = job(
            "schema-apply-schema-apply-0",
            &key,
            0,
            Some(("Complete", Utc::now())),
        );
        fake.set_route("GET", JOBS_PATH, 200, job_list(&[finished]));
        let message = serde_json::json!({"outcome": "done", "fingerprint": DECLARED}).to_string();
        fake.set_route("GET", PODS_PATH, 200, finished_pods(&message));
        ensure(&meta).await.expect("record");
        let recorded = saved(&fake).expect("state saved");
        assert_eq!(
            recorded.schema.get(DEPLOYMENT_MARKER).map(String::as_str),
            Some(DECLARED)
        );
        // later reconciles over the recorded state raise no skew condition
        fake.set_route("GET", STATE_PATH, 200, state_config_map(&recorded));
        ensure(&meta).await.expect("again");
        assert_eq!(skew_condition(&meta), None);
    }

    /// A Job fingerprint other than the declared one raises the skew condition naming both,
    /// and the condition clears once the recorded fingerprint agrees with the declared one
    #[tokio::test]
    async fn differing_declared_fingerprint_raises_skew_until_they_agree() {
        // a successful apply recorded a fingerprint other than the declared one
        inject_declared(DECLARED);
        let cluster = cluster();
        let key = key_of(&cluster);
        let fake = fake_kube(&cluster, &applied_state(&key, REPORTED), &[]);
        let meta = meta_for(cluster, &fake.client());
        let action = ensure(&meta).await.expect("skewed");
        assert!(action.is_none());
        // the skew condition holds and names both fingerprints
        let skew = skew_condition(&meta).expect("skew condition");
        assert_eq!(skew.status, "True");
        assert!(
            skew.message.contains(REPORTED) && skew.message.contains(DECLARED),
            "{}",
            skew.message
        );
        // a recorded fingerprint that agrees with the declared one clears the condition
        let agreeing = applied_state(&key, DECLARED);
        fake.set_route("GET", STATE_PATH, 200, state_config_map(&agreeing));
        ensure(&meta).await.expect("agreeing");
        let skew = skew_condition(&meta).expect("skew condition");
        assert_eq!(skew.status, "False");
        assert!(created_jobs(&fake).is_empty(), "{:?}", created_jobs(&fake));
    }

    /// A trigger key that comes back after another key was applied runs the Job again instead
    /// of taking the Job left from its first apply, and the new Job records the key it applied
    /// over
    #[tokio::test]
    async fn returning_key_reruns_over_a_stale_job() {
        // K1 was applied, then K2 (a changed TTL), and the config is back at K1
        let cluster = cluster();
        let k1 = key_of(&cluster);
        let meta = meta_for(cluster.clone(), &FakeKube::default().client());
        let revision = state(None).revision;
        let mut conf = meta.conf.clone();
        conf.thorium.retention.logs += 1;
        let k2 = trigger_key(&cluster, &conf, &revision);
        // the Job of K1's first apply is still kept for its logs
        let stale_name = spec(&meta, &revision, &k1, 0, None).expect("spec").name;
        let day_ago = Utc::now() - chrono::Duration::days(1);
        let stale = job(&stale_name, &k1, 0, Some(("Complete", day_ago)));
        let fake = fake_kube(&cluster, &state(Some(&k2)), std::slice::from_ref(&stale));
        let meta = meta_for(cluster.clone(), &fake.client());
        // the rollout is held for a new Job and nothing is recorded yet
        let action = ensure_at(&meta, Some(OWN)).await.expect("ensure");
        assert!(action.is_some(), "the stale K1 Job was taken for an apply");
        assert!(saved(&fake).is_none());
        // the new Job is the next attempt for K1, applied over K2, under another name
        let created = created_jobs(&fake);
        assert_eq!(created.len(), 1);
        let annotations = &created[0]["metadata"]["annotations"];
        assert_eq!(annotations[TRIGGER_KEY_ANNOTATION], k1.as_str());
        assert_eq!(annotations[APPLIED_OVER_ANNOTATION], k2.as_str());
        assert_eq!(annotations[ATTEMPT_ANNOTATION], "1");
        assert_ne!(created[0]["metadata"]["name"], stale_name.as_str());
        // once that Job completed it is recorded
        let name = created[0]["metadata"]["name"].as_str().expect("name");
        let mut done = job(name, &k1, 1, Some(("Complete", Utc::now())));
        done["metadata"]["annotations"][APPLIED_OVER_ANNOTATION] = serde_json::json!(k2);
        fake.set_route("GET", JOBS_PATH, 200, job_list(&[stale, done]));
        let action = ensure_at(&meta, Some(OWN)).await.expect("record");
        assert!(action.is_none());
        let recorded = saved(&fake).expect("state saved");
        assert_eq!(recorded.schema.get(APPLY_KEY_MARKER), Some(&k1));
        assert_eq!(created_jobs(&fake).len(), 1);
    }

    /// Rolling the image back to one applied before runs the Job again instead of taking the
    /// Job left from that image's first apply
    #[tokio::test]
    async fn image_rollback_reruns_over_a_stale_job() {
        // the newer image was applied after the older one
        let older = cluster();
        let mut newer_spec = full_spec();
        newer_spec["version"] = serde_json::json!("1.8.2");
        let newer_key = key_of(&namespaced_cluster(newer_spec));
        let older_key = key_of(&older);
        // the older image's Job, applied over a fresh install, is still kept for its logs
        let meta = meta_for(older.clone(), &FakeKube::default().client());
        let revision = state(None).revision;
        let stale_name = spec(&meta, &revision, &older_key, 0, None)
            .expect("spec")
            .name;
        let hour_ago = Utc::now() - chrono::Duration::hours(1);
        let stale = job(&stale_name, &older_key, 0, Some(("Complete", hour_ago)));
        let fake = fake_kube(&older, &state(Some(&newer_key)), &[stale]);
        let meta = meta_for(older, &fake.client());
        // rolling back runs the Job again over the newer image's apply
        let action = ensure_at(&meta, Some(OWN)).await.expect("ensure");
        assert!(
            action.is_some(),
            "the older image's Job was taken for an apply"
        );
        assert!(saved(&fake).is_none());
        let created = created_jobs(&fake);
        assert_eq!(created.len(), 1);
        let annotations = &created[0]["metadata"]["annotations"];
        assert_eq!(annotations[TRIGGER_KEY_ANNOTATION], older_key.as_str());
        assert_eq!(annotations[APPLIED_OVER_ANNOTATION], newer_key.as_str());
        assert_ne!(created[0]["metadata"]["name"], stale_name.as_str());
    }
}
