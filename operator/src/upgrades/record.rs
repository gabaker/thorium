//! The deployment's record in Scylla, which the operator reaches only through thoradm Jobs
//!
//! A short `thoradm schema status` Job reads the record, a `thoradm migrate record` Job copies the
//! operator's entries into it and moves its revision, and `thoradm schema apply` seeds it. The
//! state `ConfigMap` is a mirror the operator rebuilds from the record, with Scylla winning on
//! any difference except the entries the operator recorded since the last `migrate record` Job.
//! The operator never plans from the mirror's window of entries: the status Job returns the steps
//! the full history finished.

use chrono::{DateTime, Utc};
use k8s_openapi::api::batch::v1::Job;
use kube::runtime::controller::Action;
use std::collections::BTreeSet;
use std::time::Duration;
use thorium::Error;
use thorium::models::schema::{
    DeploymentRecord, FINALIZE_STEP, FinishedStep, RESULT_PREFIX, RecordStatus, StatusReport,
};
use thorium::models::upgrades::{
    APPLY_KEY_MARKER, AppliedStep, DEPLOYMENT_MARKER, MAX_HISTORY, RECORD_MARKER, RESET_STATE_STEP,
    RESTORED_STEP, RevisionId, StepState, UpgradeState,
};

use super::detect::{self, Detected};
use super::drift;
use super::migration::CATALOG_MARKER;
use super::state::{self, StoredState};
use super::{BASELINE_REVISION, FIRST_HELM_REVISION, OPERATOR_VERSION, Pending, catalog};
use crate::k8s::clusters::ClusterMeta;
use crate::k8s::crds::{self, ClusterPhase};
use crate::k8s::jobs::{self, JobKind, JobPhase, JobSpec, JobState};

/// The step label of the status Jobs
pub const STATUS_STEP: &str = "status";

/// The step label of the `migrate record` Jobs
pub const RECORD_STEP: &str = "record";

/// The step label of the `schema apply --seed-revision` Job
pub const SEED_STEP: &str = "seed-revision";

/// The annotation on a status Job holding why the record is read, which keeps one reader's Job
/// from another's
const PURPOSE_ANNOTATION: &str = "thorium.sandia.gov/purpose";

/// The annotation on a Job holding the revision it records
const REVISION_ANNOTATION: &str = "thorium.sandia.gov/revision";

/// The annotation on a `migrate record` Job holding the hash of the state it copies
const STATE_HASH_ANNOTATION: &str = "thorium.sandia.gov/state-hash";

/// The annotation on a `migrate record` Job marking one that seeds the record
const SEED_ANNOTATION: &str = "thorium.sandia.gov/seed";

/// The environment variable telling `schema apply --seed-revision` where the revision came from
const SEED_SOURCE_ENV: &str = "THORIUM_SEED_SOURCE";

/// Where the seeded revision came from, as the seed's history entry names it
const SEED_SOURCE: &str = "spec.upgrade.starting_revision";

/// How long `thoradm schema apply` waits for Scylla to accept connections, in seconds
const WAIT_FOR_SCYLLA_SECS: &str = "300";

/// How long a status Job may run, in seconds
const STATUS_DEADLINE_SECS: i64 = 300;

/// How long a finished status Job is kept when it isn't deleted first, in seconds
const STATUS_TTL_SECS: i32 = 3600;

/// How long in seconds to wait before checking on a running Job
pub(super) const RUNNING_REQUEUE_SECS: u64 = 15;

/// How long in seconds to wait before checking on a keyspace that is blocked for an admin
const BLOCKED_REQUEUE_SECS: u64 = 60;

/// The key in the state's schema markers holding when the record was last read for a plan
const READ_MARKER: &str = "record_read";

/// The key in the state's schema markers holding the image the last read's status Job ran, so a
/// read made by another image's thoradm (and its catalog) is never reused for planning
const READ_IMAGE_MARKER: &str = "record_read_image";

/// The key in the state's schema markers holding the steps the record's whole history finished
/// when it was last read, as JSON, so a plan within the read's validity still counts the steps
/// the state's window of entries lost
const FINISHED_MARKER: &str = "record_finished";

/// The key in the state's schema markers holding how many entries the record's full, uncapped
/// history held when it was last read, which the confirmation code of a lossy step is computed from
const HISTORY_LEN_MARKER: &str = "record_history_len";

/// How long in seconds a read of the record stays good for planning: what the record holds changes
/// only through the operator's own Jobs, so a step blocked on an admin doesn't start a status Job
/// at every recheck
const READ_VALID_SECS: i64 = 900;

/// What reading the record through a status Job found
#[derive(Debug)]
pub enum Read {
    /// The Job is still running, or was just created; check again later
    Wait(Action),
    /// The Job's report
    Report(Box<StatusReport>),
    /// The Job failed, with why; it is deleted and a later reconcile runs another
    Failed(String),
}

/// What moving the recorded revision came to
#[derive(Debug)]
pub enum Moved {
    /// The record Job is running, or was just created; check again later
    Wait(Action),
    /// The revision is recorded and the state mirrors it
    Done,
}

/// Get an annotation of a Job
///
/// # Arguments
///
/// * `job` - The Job to read
/// * `key` - The annotation's key
fn annotation<'a>(job: &'a Job, key: &str) -> Option<&'a str> {
    // read it from the Job's metadata
    job.metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(key))
        .map(String::as_str)
}

/// Get the name of a Job
///
/// # Arguments
///
/// * `job` - The Job to read
fn name(job: &Job) -> String {
    // a Job read from the API has a name
    job.metadata.name.clone().unwrap_or_default()
}

/// Build the status Job that reads the record
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `purpose` - Why the record is read
/// * `result` - The UID of a writing Job whose result row the report should carry
/// * `nonce` - What makes this Job's name its own
pub fn status_spec(
    meta: &ClusterMeta,
    purpose: &str,
    result: Option<&str>,
    nonce: &str,
) -> Result<JobSpec, Error> {
    // name the Job from why it reads and when, so a later read never meets this one
    let name = jobs::job_name(
        JobKind::SchemaStatus,
        STATUS_STEP,
        &[purpose, result.unwrap_or_default(), nonce],
    );
    // `schema status`, waiting for Scylla to come up, asking for a result row when given its Job
    let mut args = ["schema", "status", "--wait-for-scylla", "60"]
        .map(ToOwned::to_owned)
        .to_vec();
    if let Some(uid) = result {
        args.extend(["--result".to_owned(), uid.to_owned()]);
    }
    // a read that reports drift probes the live schema with the spec's acknowledgements, and any
    // other read of the record skips the probe
    if drift::probes(purpose) {
        args.extend(["--acknowledged".to_owned(), drift::acknowledgements(meta)?]);
    } else {
        args.push("--no-probe".to_owned());
    }
    let mut spec = jobs::base_spec(
        JobKind::SchemaStatus,
        STATUS_STEP,
        name,
        meta.cluster.get_image(),
        args,
    )?;
    // a read is short, so it never holds a deadline or its Job for long
    spec.active_deadline_seconds = STATUS_DEADLINE_SECS;
    spec.ttl_seconds_after_finished = STATUS_TTL_SECS;
    spec.annotations
        .insert(PURPOSE_ANNOTATION.to_owned(), purpose.to_owned());
    Ok(spec)
}

/// Build the `migrate record` Job, the only Job that mounts the state `ConfigMap`
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `revision` - The revision to record, or the revision the state holds when seeding
/// * `seed` - Whether the Job seeds the record from the state
/// * `state_hash` - The hash of the state the Job copies, which keeps its name its own
pub fn record_spec(
    meta: &ClusterMeta,
    revision: &RevisionId,
    seed: bool,
    state_hash: &str,
) -> Result<JobSpec, Error> {
    // name the Job from what it records and the state it copies
    let kind = if seed { "seed" } else { "move" };
    let name = jobs::job_name(
        JobKind::Record,
        RECORD_STEP,
        &[revision.as_str(), kind, state_hash],
    );
    // record the revision from the mounted state
    let mut args = ["migrate", "record", "--revision", revision.as_str()]
        .map(ToOwned::to_owned)
        .to_vec();
    if seed {
        args.push("--seed".to_owned());
    }
    args.extend(["--state".to_owned(), "/state".to_owned()]);
    let mut spec = jobs::base_spec(
        JobKind::Record,
        RECORD_STEP,
        name,
        meta.cluster.get_image(),
        args,
    )?;
    // only this Job reads the state, read-only
    spec.mount_state = true;
    spec.annotations.extend([
        (REVISION_ANNOTATION.to_owned(), revision.to_string()),
        (STATE_HASH_ANNOTATION.to_owned(), state_hash.to_owned()),
        (SEED_ANNOTATION.to_owned(), seed.to_string()),
    ]);
    Ok(spec)
}

/// Build the `schema apply --seed-revision` Job that records a starting revision
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `revision` - The revision the admin named in `spec.upgrade.starting_revision`
pub fn seed_spec(meta: &ClusterMeta, revision: &RevisionId) -> Result<JobSpec, Error> {
    // name the Job from the revision it seeds
    let name = jobs::job_name(JobKind::SchemaApply, SEED_STEP, &[revision.as_str()]);
    // apply the schema and seed the revision after probing the live schema against its snapshot
    let args = [
        "schema",
        "apply",
        "--wait-for-scylla",
        WAIT_FOR_SCYLLA_SECS,
        "--seed-revision",
        revision.as_str(),
    ]
    .map(ToOwned::to_owned)
    .to_vec();
    let mut spec = jobs::base_spec(
        JobKind::SchemaApply,
        SEED_STEP,
        name,
        meta.cluster.get_image(),
        args,
    )?;
    // the seed's history entry names where the revision came from
    spec.env
        .insert(SEED_SOURCE_ENV.to_owned(), SEED_SOURCE.to_owned());
    spec.annotations
        .insert(REVISION_ANNOTATION.to_owned(), revision.to_string());
    Ok(spec)
}

/// Parse a status Job's report out of its pod's log
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `job` - The finished Job
async fn parse_report(meta: &ClusterMeta, job: &Job) -> Result<StatusReport, Error> {
    // find the line the Job printed
    let job_name = name(job);
    let line = jobs::result_line(meta, &job_name, RESULT_PREFIX)
        .await?
        .ok_or_else(|| {
            Error::new(format!(
                "The status Job {job_name} finished without printing a result line"
            ))
        })?;
    // and read the report it holds
    serde_json::from_str(&line).map_err(|error| {
        Error::new(format!(
            "The status Job {job_name} printed a result that isn't a report: {error}"
        ))
    })
}

/// Whether a status Job exists for a purpose, running or finished
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `purpose` - Why the record is read
pub async fn reading(meta: &ClusterMeta, purpose: &str) -> Result<bool, Error> {
    // look for a status Job whose purpose this is
    let found = jobs::find(meta, JobKind::SchemaStatus, STATUS_STEP).await?;
    Ok(found
        .iter()
        .any(|job| annotation(job, PURPOSE_ANNOTATION) == Some(purpose)))
}

/// Read the record through a status Job, creating one when none is running
///
/// A finished Job is consumed: its report is read and the Job deleted, so the next read runs a
/// fresh one.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `purpose` - Why the record is read, which keeps one reader's Job from another's
/// * `result` - The UID of a writing Job whose result row the report should carry
pub async fn read_report(
    meta: &ClusterMeta,
    purpose: &str,
    result: Option<&str>,
) -> Result<Read, Error> {
    // find this reader's Job among the status Jobs
    let found = jobs::find(meta, JobKind::SchemaStatus, STATUS_STEP).await?;
    let mine = found
        .iter()
        .find(|job| annotation(job, PURPOSE_ANNOTATION) == Some(purpose));
    let wait = Action::requeue(Duration::from_secs(RUNNING_REQUEUE_SECS));
    let Some(job) = mine else {
        // none exists: create one, named for this moment so no earlier Job's name is reused
        let nonce = Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or_default()
            .to_string();
        let spec = status_spec(meta, purpose, result, &nonce)?;
        jobs::create(&spec, meta).await?;
        return Ok(Read::Wait(wait));
    };
    match jobs::phase(job) {
        // a running Job is checked again soon
        JobPhase::Active => Ok(Read::Wait(wait)),
        // a finished one is read, then deleted so the next read is fresh
        JobPhase::Complete => {
            let report = parse_report(meta, job).await;
            jobs::delete(meta, &name(job)).await?;
            report.map(|report| Read::Report(Box::new(report)))
        }
        // a failed one is deleted, and a later reconcile runs another
        JobPhase::Failed { reason, .. } => {
            let state = jobs::state(meta, job).await?;
            jobs::delete(meta, &name(job)).await?;
            let detail = match state {
                JobState::Failed { log_tail, .. } => log_tail.join(" | "),
                _ => String::new(),
            };
            Ok(Read::Failed(format!("{reason} {detail}").trim().to_owned()))
        }
    }
}

/// Parse an entry's time
///
/// # Arguments
///
/// * `at` - The entry's time, if it has one
fn instant(at: Option<&str>) -> Option<DateTime<Utc>> {
    // any RFC 3339 offset, read as the same instant in UTC
    at.and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&Utc))
}

/// Identify a history entry by what the record matches entries by: its revision, step, outcome, and
/// time
///
/// # Arguments
///
/// * `entry` - The entry
fn identity(entry: &AppliedStep) -> (String, String, String, Option<DateTime<Utc>>) {
    // the time is an instant, so entries spelled differently still match
    (
        entry.revision.clone(),
        entry.step.clone(),
        format!("{:?}", entry.outcome),
        instant(entry.at.as_deref()),
    )
}

/// Rebuild the state a record mirrors, keeping the entries the operator recorded since the last
/// copy
///
/// Scylla wins on the revision and every entry it holds. An entry only the state holds is kept
/// when the record's window shows it newer than the oldest entry it returned (or the window holds
/// the whole history), since the next `migrate record` Job copies it. The state's markers of a
/// schema apply are dropped when the record holds another deployment fingerprint than the apply
/// recorded.
///
/// # Arguments
///
/// * `record` - The record a status Job returned
/// * `previous` - The state the operator holds, if any
#[must_use]
pub fn mirror(record: &DeploymentRecord, previous: Option<&UpgradeState>) -> UpgradeState {
    // the record's revision and history, then the markers the operator keeps of its own
    let mut state = UpgradeState::new(record.revision.clone(), OPERATOR_VERSION);
    state
        .applied_by_version
        .clone_from(&record.applied_by_version);
    state.applied.clone_from(&record.history);
    if let Some(previous) = previous {
        state.schema.clone_from(&previous.schema);
        // an apply the record no longer holds, such as one a downgrade recorded an older revision's
        // schema over, leaves no key behind that would skip the next apply
        let marked = state.schema.get(DEPLOYMENT_MARKER).map(String::as_str);
        if marked.is_some_and(|marked| marked != record.schema.deployment.as_str()) {
            state.schema.remove(APPLY_KEY_MARKER);
            state.schema.remove(DEPLOYMENT_MARKER);
        }
        // an entry the record doesn't hold, and its window doesn't hide, waits for the next copy
        let held = record.history.iter().map(identity).collect::<BTreeSet<_>>();
        let whole =
            usize::try_from(record.history_len).is_ok_and(|len| len <= record.history.len());
        let oldest = record
            .history
            .iter()
            .filter_map(|entry| instant(entry.at.as_deref()))
            .min();
        let unsynced = previous.applied.iter().filter(|entry| {
            !held.contains(&identity(entry)) && (whole || instant(entry.at.as_deref()) >= oldest)
        });
        state.applied.extend(unsynced.cloned());
    }
    // oldest first by time, the newest entries within the mirror's size
    state
        .applied
        .sort_by_key(|entry| instant(entry.at.as_deref()));
    let excess = state.applied.len().saturating_sub(MAX_HISTORY);
    state.applied.drain(..excess);
    state
        .schema
        .insert(RECORD_MARKER.to_owned(), "scylla".to_owned());
    state
}

/// Whether a record was moved to another revision by a restore or a state reset the state hasn't
/// seen
///
/// That is a record at another revision than the state whose history holds a `restored` or
/// `reset-state` entry that the state lacks and that is newer than the state's newest entry. A
/// record that merely lags the state (an old mirror) carries no such entry, and an entry older than
/// the state's newest was already followed or undone by what the operator recorded since.
///
/// # Arguments
///
/// * `record` - The record a status Job returned
/// * `state` - The state the operator holds
#[must_use]
pub fn rewound(record: &DeploymentRecord, state: &UpgradeState) -> bool {
    // a record at the state's revision moved nothing
    if record.revision == state.revision {
        return false;
    }
    // the entries and the newest time the state holds
    let held = state.applied.iter().map(identity).collect::<BTreeSet<_>>();
    let newest = state
        .applied
        .iter()
        .filter_map(|entry| instant(entry.at.as_deref()))
        .max();
    // an entry that lowered the revision, which the state lacks and which is newer than its own
    record.history.iter().any(|entry| {
        (entry.step == RESTORED_STEP || entry.step == RESET_STATE_STEP)
            && !held.contains(&identity(entry))
            && instant(entry.at.as_deref()) > newest
    })
}

/// Make a state follow a record that a restore or a state reset moved: rebuild it from the record
/// and forget what the last reads of the record noted
///
/// The reads' markers describe the record before it moved, so the next plan reads the record again
/// instead of planning from the steps they noted as finished.
///
/// # Arguments
///
/// * `state` - The state to rebuild
/// * `record` - The record a status Job returned
pub fn follow(state: &mut UpgradeState, record: &DeploymentRecord) {
    // the record's revision and history win
    let mut rebuilt = mirror(record, Some(state));
    // what the earlier reads noted no longer holds
    for marker in [
        READ_MARKER,
        READ_IMAGE_MARKER,
        FINISHED_MARKER,
        HISTORY_LEN_MARKER,
    ] {
        rebuilt.schema.remove(marker);
    }
    *state = rebuilt;
}

/// Build the state to plan from: the mirror, with a `Done` entry for every finished step of the
/// full history the mirror's window no longer holds
///
/// # Arguments
///
/// * `record` - The record a status Job returned
/// * `mirrored` - The mirror built from it
#[must_use]
pub fn planning_state(record: &DeploymentRecord, mirrored: &UpgradeState) -> UpgradeState {
    // the record's finished set decides
    with_finished(&record.finished, mirrored)
}

/// Read the finished steps the last read of the record noted in a state
///
/// # Arguments
///
/// * `state` - The state the read noted them in
fn noted_finished(state: &UpgradeState) -> Vec<FinishedStep> {
    // a marker that is missing or doesn't parse notes none
    state
        .schema
        .get(FINISHED_MARKER)
        .and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or_default()
}

/// Add a `Done` entry for every finished step the state's window of entries doesn't hold
///
/// # Arguments
///
/// * `finished` - The steps the record's whole history finished
/// * `mirrored` - The state to plan from
fn with_finished(finished: &[FinishedStep], mirrored: &UpgradeState) -> UpgradeState {
    // a finished step the window lacks is added before every entry it holds, which is where its
    // entry sits in the full history whenever nothing newer undid it
    let mut earlier = Vec::new();
    for finished in finished {
        let held = mirrored.applied.iter().any(|entry| {
            entry.revision == finished.revision.as_str()
                && entry.step == finished.step
                && entry.outcome.finishes()
        });
        if !held {
            earlier.push(AppliedStep {
                revision: finished.revision.to_string(),
                step: finished.step.clone(),
                outcome: StepState::Done,
                message: Some("finished in the record's full history".to_owned()),
                ..AppliedStep::default()
            });
        }
    }
    let mut state = mirrored.clone();
    earlier.append(&mut state.applied);
    state.applied = earlier;
    state
}

/// Say what the operator waits on, and check again later
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `phase` - The phase to report
/// * `message` - What the operator waits on
/// * `secs` - How long to wait before checking again
async fn hold(meta: &ClusterMeta, phase: ClusterPhase, message: String, secs: u64) -> Action {
    // log it and report it in the cluster's status
    super::log(&message);
    crds::set_status(&meta.client, &meta.status, phase, Some(message)).await;
    Action::requeue(Duration::from_secs(secs))
}

/// Pick the revision today's detection would have picked for a namespace with no record: the
/// baseline for a converted pre-Helm deployment, the first Helm revision otherwise
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
async fn suggestion(meta: &ClusterMeta) -> Result<&'static str, Error> {
    // look at what the namespace holds the way the first operator did
    let detected = detect::detect(&meta.client, &meta.namespace, &meta.cluster).await?;
    Ok(match detected {
        Detected::PreHelm(_) => BASELINE_REVISION,
        Detected::Fresh | Detected::BeforeTracking => FIRST_HELM_REVISION,
    })
}

/// Save the state a record mirrors, creating the `ConfigMap` when none exists
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `mirrored` - The state to save
/// * `loaded` - The state read before, whose version the replacement is pinned to
async fn save_mirror(
    meta: &ClusterMeta,
    mirrored: UpgradeState,
    loaded: Option<&StoredState>,
) -> Result<StoredState, Error> {
    // pin a replacement to the version read, so a concurrent writer isn't overwritten
    let mut stored = StoredState {
        state: mirrored,
        resource_version: loaded.and_then(|loaded| loaded.resource_version.clone()),
    };
    state::save(&meta.client, &meta.namespace, &mut stored).await?;
    Ok(stored)
}

/// Find the Jobs of a kind and step that carry an annotation value
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `kind` - What the Jobs are for
/// * `step` - The step they run
/// * `key` - The annotation to match
/// * `value` - The value it must hold
async fn jobs_with(
    meta: &ClusterMeta,
    kind: JobKind,
    step: &str,
    key: &str,
    value: &str,
) -> Result<Vec<Job>, Error> {
    // the operator finds its Jobs by label, then by what each was created for
    Ok(jobs::find(meta, kind, step)
        .await?
        .into_iter()
        .filter(|job| annotation(job, key) == Some(value))
        .collect())
}

/// Describe a Job that failed, for the status and the log
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `job` - The failed Job
/// * `what` - What the Job was doing
async fn failure(meta: &ClusterMeta, job: &Job, what: &str) -> Result<String, Error> {
    // read the pod's last log lines for the admin
    let tail = match jobs::state(meta, job).await? {
        JobState::Failed {
            reason, log_tail, ..
        } => format!("{reason}: {}", log_tail.join(" | ")),
        _ => String::new(),
    };
    Ok(format!(
        "The Job {} that {what} failed and is kept for its logs; delete it to try again. {tail}",
        name(job)
    ))
}

/// Run the seed that records a namespace's starting revision, when the admin named one, and wait
/// for it
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `revision` - The revision the admin named
async fn seed_starting_revision(meta: &ClusterMeta, revision: &str) -> Result<Action, Error> {
    // a revision that doesn't parse can't be seeded
    let parsed = revision.parse::<RevisionId>().map_err(|error| {
        Error::new(format!(
            "spec.upgrade.starting_revision {revision}: {error}"
        ))
    })?;
    // find the seed Job for this revision
    let found = jobs_with(
        meta,
        JobKind::SchemaApply,
        SEED_STEP,
        REVISION_ANNOTATION,
        revision,
    )
    .await?;
    let Some(job) = found.first() else {
        // none yet: create it and wait
        let spec = seed_spec(meta, &parsed)?;
        jobs::create(&spec, meta).await?;
        let message = format!(
            "Seeding revision {revision} from spec.upgrade.starting_revision with the Job {}",
            spec.name
        );
        return Ok(hold(
            meta,
            ClusterPhase::Provisioning,
            message,
            RUNNING_REQUEUE_SECS,
        )
        .await);
    };
    // report where the seed stands
    let (phase, message, secs) = match jobs::phase(job) {
        JobPhase::Active => (
            ClusterPhase::Provisioning,
            format!(
                "Waiting for the Job {} to seed revision {revision}",
                name(job)
            ),
            RUNNING_REQUEUE_SECS,
        ),
        // the record is read again, which finds the seeded revision
        JobPhase::Complete => (
            ClusterPhase::Provisioning,
            format!(
                "The Job {} seeded revision {revision}; reading the record",
                name(job)
            ),
            RUNNING_REQUEUE_SECS,
        ),
        JobPhase::Failed { .. } => (
            ClusterPhase::Error,
            failure(meta, job, &format!("seeds revision {revision}")).await?,
            BLOCKED_REQUEUE_SECS,
        ),
    };
    Ok(hold(meta, phase, message, secs).await)
}

/// Seed the record from an existing state before any schema-apply Job runs, and wait for it
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `stored` - The state the operator holds
async fn seed_from_state(meta: &ClusterMeta, stored: &StoredState) -> Result<Action, Error> {
    // the seed is named for the state it copies, so a changed state gets its own Job
    let hash = stored.state.hash();
    let revision = stored.state.revision.clone();
    let found = jobs_with(
        meta,
        JobKind::Record,
        RECORD_STEP,
        STATE_HASH_ANNOTATION,
        &hash,
    )
    .await?;
    let seeding = found
        .iter()
        .find(|job| annotation(job, SEED_ANNOTATION) == Some("true"));
    let Some(job) = seeding else {
        // none yet: create it and wait
        let spec = record_spec(meta, &revision, true, &hash)?;
        jobs::create(&spec, meta).await?;
        let message = format!(
            "Seeding the record with revision {revision} and the history of the state ConfigMap \
             with the Job {}",
            spec.name
        );
        return Ok(hold(
            meta,
            ClusterPhase::Provisioning,
            message,
            RUNNING_REQUEUE_SECS,
        )
        .await);
    };
    // report where the seed stands
    let (phase, message) = match jobs::phase(job) {
        JobPhase::Active => (
            ClusterPhase::Provisioning,
            format!("Waiting for the Job {} to seed the record", name(job)),
        ),
        // a seed that finished leaves a record the next read finds
        JobPhase::Complete => (
            ClusterPhase::Error,
            format!(
                "The Job {} finished seeding the record, but the record still holds no revision",
                name(job)
            ),
        ),
        JobPhase::Failed { .. } => (
            ClusterPhase::Error,
            failure(meta, job, "seeds the record").await?,
        ),
    };
    Ok(hold(meta, phase, message, RUNNING_REQUEUE_SECS).await)
}

/// Act on what the status Job found
///
/// Returns the action to take while recovery waits, or nothing once the state mirrors the record.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `report` - What the status Job reported
async fn act(meta: &ClusterMeta, report: StatusReport) -> Result<Option<Action>, Error> {
    // the state the operator holds, if any
    let loaded = state::load(&meta.client, &meta.namespace).await?;
    match report.status {
        // a record is mirrored, whatever the state held, and planned from
        RecordStatus::Recorded { record, .. } => {
            let mirrored = mirror(&record, loaded.as_ref().map(|loaded| &loaded.state));
            save_mirror(meta, mirrored, loaded.as_ref()).await?;
            println!(
                "Rebuilt the upgrade state of {} from the record at revision {}",
                meta.namespace, record.revision
            );
            Ok(None)
        }
        // no keyspace is a fresh install, whatever Secrets exist: it starts at the latest revision
        RecordStatus::NoKeyspace => {
            // no entry is recorded: the apply that creates the schema records its own
            let mut fresh = UpgradeState::new(catalog().latest(), OPERATOR_VERSION);
            fresh
                .schema
                .insert(RECORD_MARKER.to_owned(), "scylla".to_owned());
            println!(
                "Starting {} at revision {} as a fresh install",
                meta.namespace, fresh.revision
            );
            save_mirror(meta, fresh, loaded.as_ref()).await?;
            Ok(None)
        }
        // data without a record: a state that exists seeds it, otherwise the admin names the
        // revision
        RecordStatus::NoRecord { .. } => {
            match (&loaded, &meta.cluster.spec.upgrade.starting_revision) {
                (Some(stored), _) => Ok(Some(seed_from_state(meta, stored).await?)),
                (None, Some(revision)) => Ok(Some(seed_starting_revision(meta, revision).await?)),
                (None, None) => {
                    let suggested = suggestion(meta).await?;
                    let message = format!(
                        "This namespace has a Thorium keyspace with data but no record of the \
                         revision it is at (no record in schema_migrations and no state \
                         ConfigMap). Nothing is changed until spec.upgrade.starting_revision is \
                         set (Helm value operator.cluster.upgrade.startingRevision) to the \
                         revision the data is at; today's detection suggests {suggested}. The \
                         seed checks the live schema against that revision's snapshot and \
                         refuses, naming the differences, when they disagree"
                    );
                    Ok(Some(
                        hold(
                            meta,
                            ClusterPhase::UpgradeRequired,
                            message,
                            BLOCKED_REQUEUE_SECS,
                        )
                        .await,
                    ))
                }
            }
        }
        // Scylla can't be reached, so nothing is known: wait, never assume a fresh install
        RecordStatus::Unreachable { message } => {
            let message = format!(
                "Scylla can't be reached, so the deployment's record can't be read and nothing is \
                 changed ({message}); trying again"
            );
            Ok(Some(
                hold(
                    meta,
                    ClusterPhase::Provisioning,
                    message,
                    RUNNING_REQUEUE_SECS,
                )
                .await,
            ))
        }
        // a refused login: nothing is known either, and the config must be fixed
        RecordStatus::AuthFailed => {
            let message = "Scylla refused Thorium's login, so the deployment's record can't be \
                           read and nothing is changed; check the Scylla credentials in the \
                           config; trying again"
                .to_owned();
            Ok(Some(
                hold(meta, ClusterPhase::Error, message, RUNNING_REQUEUE_SECS).await,
            ))
        }
    }
}

/// Recover the record when the state `ConfigMap` is missing or mirrors none: prepare the backends
/// and the `thorium` Secret, read the record, and act on what it holds
///
/// Returns the action to take while recovery waits, or nothing once the state mirrors the record.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `prepare` - Writes the `thorium` Secret and bootstraps Thorium's Scylla role, returning an
///   action while a backend isn't ready
pub async fn recover_with(
    meta: &ClusterMeta,
    prepare: impl AsyncFnOnce(&ClusterMeta) -> Result<Option<Action>, Error>,
) -> Result<Option<Action>, Error> {
    // the status Job mounts the `thorium` Secret and logs in as Thorium's role, so both come first
    if let Some(action) = prepare(meta).await? {
        return Ok(Some(action));
    }
    // read the record
    match read_report(meta, "record", None).await? {
        Read::Wait(action) => {
            let message = "Reading the deployment's record from Scylla".to_owned();
            crds::set_status(
                &meta.client,
                &meta.status,
                ClusterPhase::Provisioning,
                Some(message),
            )
            .await;
            Ok(Some(action))
        }
        Read::Failed(reason) => {
            let message = format!("Reading the deployment's record failed: {reason}; trying again");
            Ok(Some(
                hold(
                    meta,
                    ClusterPhase::Provisioning,
                    message,
                    BLOCKED_REQUEUE_SECS,
                )
                .await,
            ))
        }
        Read::Report(report) => act(meta, *report).await,
    }
}

/// Read the record before planning, rebuild the state from it, and plan again from what it
/// finished
///
/// Returns the action to take while the read waits.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade about to run, whose state and steps are replaced
pub async fn refresh(meta: &ClusterMeta, pending: &mut Pending) -> Result<Option<Action>, Error> {
    // a recent read of the record by the same image still holds, a future date never does
    let acks = drift::acknowledgements(meta)?;
    let recent = pending
        .stored
        .state
        .schema
        .get(READ_MARKER)
        .and_then(|at| instant(Some(at)))
        .is_some_and(|at| (0..READ_VALID_SECS).contains(&(Utc::now() - at).num_seconds()))
        && pending
            .stored
            .state
            .schema
            .get(READ_IMAGE_MARKER)
            .is_some_and(|image| *image == meta.cluster.get_image())
        && !drift::acknowledgements_changed(&pending.stored.state, &acks);
    if recent {
        // plan from the finished steps the read noted, which the state's window may have lost
        let planning = with_finished(
            &noted_finished(&pending.stored.state),
            &pending.stored.state,
        );
        pending.steps = catalog()
            .plan(&planning, &pending.target)
            .map_err(|error| Error::new(error.to_string()))?;
        return Ok(None);
    }
    // read the record the plan starts from
    let report = match read_report(meta, "plan", None).await? {
        Read::Wait(action) => return Ok(Some(action)),
        Read::Failed(reason) => {
            let message = format!("Reading the deployment's record failed: {reason}; trying again");
            return Ok(Some(
                hold(
                    meta,
                    ClusterPhase::Provisioning,
                    message,
                    BLOCKED_REQUEUE_SECS,
                )
                .await,
            ));
        }
        Read::Report(report) => report,
    };
    // a record that has gone missing since the state was mirrored holds the plan
    let RecordStatus::Recorded {
        record,
        drift: observed,
    } = report.status
    else {
        let message = format!(
            "The deployment's record can't be read before planning ({:?}); nothing is changed",
            report.status
        );
        return Ok(Some(
            hold(meta, ClusterPhase::Error, message, BLOCKED_REQUEUE_SECS).await,
        ));
    };
    // Scylla wins: rewrite the state, noting when it was read, keeping what the operator
    // recorded since
    let mut mirrored = mirror(&record, Some(&pending.stored.state));
    mirrored
        .schema
        .insert(READ_MARKER.to_owned(), Utc::now().to_rfc3339());
    // note the image whose thoradm made the read
    mirrored
        .schema
        .insert(READ_IMAGE_MARKER.to_owned(), meta.cluster.get_image());
    mirrored.schema.insert(
        FINISHED_MARKER.to_owned(),
        serde_json::to_string(&record.finished).unwrap_or_default(),
    );
    mirrored.schema.insert(
        HISTORY_LEN_MARKER.to_owned(),
        record.history_len.to_string(),
    );
    // note the catalog the image's thoradm knows, which the data-changing steps compare with
    mirrored.schema.insert(
        CATALOG_MARKER.to_owned(),
        serde_json::to_string(&report.catalog).unwrap_or_default(),
    );
    drift::note(&mut mirrored, observed.as_ref(), &acks);
    pending.stored = save_mirror(meta, mirrored.clone(), Some(&pending.stored)).await?;
    // show the drift the read found, which blocks the steps that overlap it
    drift::publish(meta, observed.as_ref()).await;
    // plan from the full history's finished steps, never from the window
    let planning = planning_state(&record, &mirrored);
    pending.steps = catalog()
        .plan(&planning, &pending.target)
        .map_err(|error| Error::new(error.to_string()))?;
    Ok(None)
}

/// Get how many entries the record's full history holds, as far as the operator knows
///
/// That is the length the last read of the record noted plus the entries the state holds that were
/// recorded since the read, so every step the operator completes lengthens it. A state no read
/// noted a length in (it was never read from the record) has only its own entries to count.
///
/// # Arguments
///
/// * `state` - The state the read noted the length in
#[must_use]
pub(super) fn history_len(state: &UpgradeState) -> usize {
    // the noted length and the time of the read it came from
    let noted = state
        .schema
        .get(HISTORY_LEN_MARKER)
        .and_then(|len| len.parse::<usize>().ok());
    let read = state
        .schema
        .get(READ_MARKER)
        .and_then(|at| instant(Some(at)));
    let (Some(noted), Some(read)) = (noted, read) else {
        return state.applied.len();
    };
    // every entry since the read adds one
    let since = state
        .applied
        .iter()
        .filter(|entry| instant(entry.at.as_deref()).is_some_and(|at| at > read))
        .count();
    noted + since
}

/// Make the next plan read the record again within a number of seconds
///
/// A step held failed may be finished by hand with a `--reason` run, whose entry thoradm writes
/// into the record, so the operator reads the record again soon instead of after a full read's
/// validity. Returns whether the state changed.
///
/// # Arguments
///
/// * `state` - The recorded upgrade state, whose read marker is moved
/// * `secs` - The most seconds until the record is read again
pub(super) fn read_again_within(state: &mut UpgradeState, secs: i64) -> bool {
    // a read that is already old enough, or none at all, needs no change
    let age = state
        .schema
        .get(READ_MARKER)
        .and_then(|at| instant(Some(at)))
        .map(|at| (Utc::now() - at).num_seconds());
    let Some(age) = age else {
        return false;
    };
    if age >= READ_VALID_SECS - secs {
        return false;
    }
    // back date the read so it lapses in `secs` seconds
    let backdated = Utc::now() - chrono::Duration::seconds(READ_VALID_SECS - secs);
    state
        .schema
        .insert(READ_MARKER.to_owned(), backdated.to_rfc3339());
    true
}

/// Move the recorded revision with a `migrate record` Job, then mirror the record
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade whose state is mirrored once the revision moved
/// * `revision` - The revision to record
pub async fn move_revision(
    meta: &ClusterMeta,
    pending: &mut Pending,
    revision: &RevisionId,
) -> Result<Moved, Error> {
    // a state already at the revision has nothing to move
    if pending.stored.state.revision == *revision {
        return Ok(Moved::Done);
    }
    // find the Job for this revision and the entries the state holds now
    let hash = pending.stored.state.hash();
    let found = jobs_with(
        meta,
        JobKind::Record,
        RECORD_STEP,
        STATE_HASH_ANNOTATION,
        &hash,
    )
    .await?;
    let moving = found.iter().find(|job| {
        annotation(job, SEED_ANNOTATION) != Some("true")
            && annotation(job, REVISION_ANNOTATION) == Some(revision.as_str())
    });
    let wait = Action::requeue(Duration::from_secs(RUNNING_REQUEUE_SECS));
    let Some(job) = moving else {
        // none yet: create it once no other writing Job runs, and wait
        if !jobs::active_writers(meta).await?.is_empty() {
            return Ok(Moved::Wait(wait));
        }
        let spec = record_spec(meta, revision, false, &hash)?;
        jobs::create(&spec, meta).await?;
        return Ok(Moved::Wait(wait));
    };
    match jobs::phase(job) {
        JobPhase::Active => Ok(Moved::Wait(wait)),
        // a failed Job holds the move, kept for its logs
        JobPhase::Failed { .. } => Err(Error::new(
            failure(meta, job, &format!("records revision {revision}")).await?,
        )),
        // a finished Job's result row, read through a status Job, is what the state mirrors
        JobPhase::Complete => {
            let uid = job.metadata.uid.clone().unwrap_or_default();
            let report = match read_report(meta, "record-result", Some(&uid)).await? {
                Read::Wait(action) => return Ok(Moved::Wait(action)),
                Read::Failed(reason) => {
                    return Err(Error::new(format!(
                        "Reading the Job's result failed: {reason}"
                    )));
                }
                Read::Report(report) => report,
            };
            let Some(RecordStatus::Recorded { record, .. }) = report.job_result else {
                return Err(Error::new(format!(
                    "The Job {} left no result for revision {revision}",
                    name(job)
                )));
            };
            // mirror the record the Job left, keeping the entries recorded since
            let mirrored = mirror(&record, Some(&pending.stored.state));
            pending.stored = save_mirror(meta, mirrored, Some(&pending.stored)).await?;
            Ok(Moved::Done)
        }
    }
}

/// Whether a step is the one the `migrate record` Job replaces: the Job records the revision, so
/// the operator runs it instead of recording the step itself
///
/// # Arguments
///
/// * `id` - The id of the step
#[must_use]
pub fn is_finalize(id: &str) -> bool {
    // the catalog's finalize step
    id == FINALIZE_STEP
}
