//! Resubmitting the requests a drain recorded, inside the conversion pause
//!
//! A drain approved to cancel what was in flight records each cancelled request, and once the
//! conversions finished `thoradm migrate run --id resubmit-reactions` recreates them in the
//! converted pipelines. The follow-up is driven from the history, which the operator rebuilds from
//! the record after a lost state `ConfigMap`: it is due when the drain's revision is recorded, the
//! newest drain `Done` entry with a Job recorded at least one request, and neither thoradm's
//! `resubmit-reactions` entry nor an entry that ends the attempt (`upgrade-stopped`, `restored`,
//! `reset-state`) came after it. A drain run whose Job failed leaves the attempt open: the retry
//! records under the failed run's start and counts the requests the failed run recorded, so the
//! attempt's requests are tagged with the start of its first run.
//!
//! While it is due and the conversion pause still holds, no later step starts and no paused
//! component is restored, so nobody can change a pipeline or a group before the resubmission bound
//! each request to the converted pipelines. A Job created while every component of the pause is at
//! zero with no live replica gets `--in-pause <attempt>`, which lets it bind requests and recreate
//! a reserved reaction that is missing. A failed Job is run again inside the pause up to three
//! Jobs per attempt; after that the pause is no longer held, and the resubmission runs again only
//! when the refresh annotation changes, outside the pause and without the flag. The
//! `ReactionsResubmitted` condition reports how it went, with counts only.

use chrono::{DateTime, Utc};
use k8s_openapi::api::batch::v1::Job;
use kube::runtime::controller::Action;
use serde_json::Value;
use std::time::Duration;
use thorium::Error;
use thorium::models::schema::{PassCounts, RecordStatus};
use thorium::models::upgrades::{
    AppliedStep, RESET_STATE_STEP, RESTORED_STEP, RevisionId, StepState, UPGRADE_STOPPED_STEP,
    UpgradeComponent, UpgradeConditionType, UpgradeState, attempt_start,
};
use tracing::{Level, event, instrument};

use super::migration::{self, REFRESH_ANNOTATION};
use super::pause::{self, RESTORE_STEP};
use super::record::{self, Read};
use super::state::{self, StoredState};
use super::{OPERATOR_VERSION, catalog};
use crate::k8s::clusters::ClusterMeta;
use crate::k8s::crds;
use crate::k8s::jobs::{self, JobKind, JobPhase, JobState};

/// The migration, Job step label, and history step of the resubmission
pub const RESUBMIT_STEP: &str = "resubmit-reactions";

/// How many resubmit Jobs an attempt runs inside the conversion pause before it stops holding it
const MAX_JOBS: usize = 3;

/// The state marker holding the refresh annotation's value when a resubmit Job outside the pause
/// last ran, or when the resubmission stopped holding the pause, so only a change runs another
const REFRESH_MARKER: &str = "resubmit_refresh";

/// The annotation on a resubmit Job saying whether it was given the in-pause flag
const IN_PAUSE_ANNOTATION: &str = "thorium.sandia.gov/in-pause";

/// The annotation on a resubmit Job holding the drain attempt it resubmits
const ATTEMPT_ANNOTATION: &str = "thorium.sandia.gov/drain-attempt";

/// The words a flagged Job's `Running` entry carries, which tell it from one outside the pause
const IN_PAUSE_NOTE: &str = "inside the conversion pause";

/// How long in seconds to wait before checking on a resubmit Job or its result again
const RUNNING_REQUEUE_SECS: u64 = 15;

/// How long in seconds to wait before trying again after following the resubmission failed
const RETRY_SECS: u64 = 60;

/// What following the resubmission asks of the reconcile
#[derive(Debug)]
pub enum Tracked {
    /// Hold the reconcile: no later step starts and no paused component is restored
    Hold(Action),
    /// Go on, and look again this many seconds later
    Again(u64),
    /// Nothing to follow
    Idle,
}

/// The drain attempt whose requests are due for resubmission
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    /// The `at` of the `Running` entry that started the drain's attempt, which tags the requests
    /// its runs recorded
    pub at: String,
    /// The revision of the drain's edge, which the resubmission's entries carry
    revision: String,
    /// How many requests the drain's run recorded
    pub(super) requests: u64,
    /// The index in the history of the `Running` entry that started the drain's attempt
    running: usize,
    /// The index of the drain's `Done` entry in the history
    done: usize,
    /// The components the drain's run paused, which the conversions keep paused
    pause: &'static [UpgradeComponent],
}

/// Read the number right before a label in a text
///
/// # Arguments
///
/// * `text` - The text
/// * `label` - The label the number comes before
fn number_before(text: &str, label: &str) -> Option<u64> {
    // the digits that end the text before the label
    let head = &text[..text.find(label)?];
    let digits = head.len() - head.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    head[head.len() - digits..].parse().ok()
}

/// Count the requests a drain's `Done` entry says its run recorded
///
/// The cancel's own counts name the requests recorded and the ones kept from an earlier run; a run
/// that left no cancel report has only the generic counts, whose writes stand in for them.
///
/// # Arguments
///
/// * `work` - The entry's work
fn requests(work: &str) -> u64 {
    // the cancel's counts, or the generic count of writes
    let recorded = number_before(work, " recorded,");
    let kept = number_before(work, " kept from an earlier run");
    if recorded.is_none() && kept.is_none() {
        return number_before(work, " written").unwrap_or(0);
    }
    recorded.unwrap_or(0) + kept.unwrap_or(0)
}

/// Parse a history entry's time
///
/// # Arguments
///
/// * `entry` - The entry
fn time_of(entry: &AppliedStep) -> Option<DateTime<Utc>> {
    // entries carry RFC 3339 times
    entry
        .at
        .as_deref()
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&Utc))
}

/// Find the drain attempt whose requests are due for resubmission
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
#[must_use]
pub fn due(state: &UpgradeState) -> Option<Attempt> {
    // the step that cancels on approval and its edge, whose revision must be recorded
    let catalog = catalog();
    let (edge, step) = catalog.steps().find(|(_, step)| step.cancels_on_approval)?;
    let guard: RevisionId = edge.id.parse().ok()?;
    if !catalog.at_or_below(&guard, &state.revision) {
        return None;
    }
    // the newest drain run that finished with a Job, and the requests it recorded
    let done = state.applied.iter().rposition(|entry| {
        entry.step == step.id && entry.outcome == StepState::Done && entry.job.is_some()
    })?;
    let requests = requests(state.applied[done].work.as_deref().unwrap_or_default());
    if requests == 0 {
        return None;
    }
    // the start of the run's attempt is the attempt its requests are tagged with: a retry after a
    // failed run carries the failed run's start, and its counts the requests the failed run kept
    let newest = state.applied[..done]
        .iter()
        .rposition(|entry| entry.step == step.id && entry.outcome == StepState::Running)?;
    let running = attempt_start(&state.applied, step.id, newest);
    let attempt = state.applied[running].at.clone()?;
    // thoradm's entry finishes the resubmission, and a stop, a restore, or a reset ends the attempt
    let ended = state.applied[done + 1..].iter().any(|entry| {
        (entry.step == RESUBMIT_STEP && entry.outcome == StepState::Done)
            || [UPGRADE_STOPPED_STEP, RESTORED_STEP, RESET_STATE_STEP]
                .contains(&entry.step.as_str())
    });
    (!ended).then(|| Attempt {
        at: attempt,
        revision: edge.id.to_owned(),
        requests,
        running,
        done,
        pause: step.pause,
    })
}

/// Get the resubmission's entries after the drain's `Done` entry, oldest first
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
/// * `due` - The attempt being resubmitted
fn entries<'a>(state: &'a UpgradeState, due: &Attempt) -> impl Iterator<Item = &'a AppliedStep> {
    // the operator's entries of each Job, and thoradm's
    state.applied[due.done + 1..]
        .iter()
        .filter(|entry| entry.step == RESUBMIT_STEP)
}

/// Count the resubmit Jobs this attempt created
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
/// * `due` - The attempt being resubmitted
fn started(state: &UpgradeState, due: &Attempt) -> usize {
    // each Job has one `Running` entry
    entries(state, due)
        .filter(|entry| entry.outcome == StepState::Running)
        .count()
}

/// Find the `Running` entry of a resubmit Job that hasn't ended
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
/// * `due` - The attempt being resubmitted
fn open_entry<'a>(state: &'a UpgradeState, due: &Attempt) -> Option<&'a AppliedStep> {
    // a Job's entry is open until the entry that ends it follows
    entries(state, due)
        .last()
        .filter(|entry| entry.outcome == StepState::Running)
}

/// Whether a resubmit Job's `Running` entry says it was given the in-pause flag
///
/// # Arguments
///
/// * `entry` - The Job's `Running` entry
fn flagged(entry: &AppliedStep) -> bool {
    // the entry's message says so
    entry
        .message
        .as_deref()
        .is_some_and(|message| message.contains(IN_PAUSE_NOTE))
}

/// Whether the history still holds every component the drain's run paused
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
/// * `due` - The attempt being resubmitted
fn pause_held(state: &UpgradeState, due: &Attempt) -> bool {
    // each component the run paused is still paused
    let paused = pause::paused(state);
    let run = &state.applied[due.running];
    !run.paused.is_empty() && run.paused.keys().all(|name| paused.contains_key(name))
}

/// Whether the resubmission holds the pause: it is due and the pause still holds with Jobs left in
/// this attempt, or a flagged Job hasn't ended
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
#[must_use]
pub fn holds(state: &UpgradeState) -> bool {
    // nothing holds without a resubmission due
    let Some(due) = due(state) else {
        return false;
    };
    // a flagged Job's pod retries must run inside the pause too
    if open_entry(state, &due).is_some_and(flagged) {
        return true;
    }
    pause_held(state, &due) && started(state, &due) < MAX_JOBS
}

/// Whether a Job created now may be given the in-pause flag: the state shows every component of the
/// pause paused with no restore after the drain's run, and each has no Deployment, or one whose spec
/// asks for no replica and that has no live replica
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `state` - The recorded upgrade state
/// * `due` - The attempt being resubmitted
async fn in_pause(meta: &ClusterMeta, state: &UpgradeState, due: &Attempt) -> Result<bool, Error> {
    // the pauses the history holds now
    let paused = pause::paused(state);
    for component in due.pause {
        let name = component.deployment_name();
        // a restore after the drain's run ended the pause, whatever paused the component again
        let restored = state.applied[due.running + 1..]
            .iter()
            .any(|entry| entry.step == RESTORE_STEP && entry.paused.contains_key(name));
        if restored {
            return Ok(false);
        }
        // a component with no Deployment has nothing to pause, and a deployed one must be paused
        // at zero, since a restore scales a component up before it records the restore
        match pause::spec_replicas(meta, name).await? {
            None => continue,
            Some(0) if paused.contains_key(name) => (),
            Some(_) => return Ok(false),
        }
        // its status lags right after a scale, so its pods must be gone too
        if pause::drained(meta, &[name]).await?.is_some() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Get the value of the cluster's refresh annotation, or an empty one
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
fn refresh_value(meta: &ClusterMeta) -> String {
    // read it from the cluster's metadata
    meta.cluster
        .metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(REFRESH_ANNOTATION))
        .cloned()
        .unwrap_or_default()
}

/// Get the name of a Job
///
/// # Arguments
///
/// * `job` - The Job
fn name_of(job: &Job) -> &str {
    // a Job read from the API has a name
    job.metadata.name.as_deref().unwrap_or_default()
}

/// Record a resubmit Job's `Running` entry
///
/// # Arguments
///
/// * `stored` - The recorded upgrade state, which gains the entry
/// * `due` - The attempt being resubmitted
/// * `job` - The Job's name
/// * `flag` - Whether the Job was given the in-pause flag
fn record_running(stored: &mut StoredState, due: &Attempt, job: &str, flag: bool) {
    // the entry counts the Job and says whether it runs inside the pause
    let place = if flag {
        IN_PAUSE_NOTE
    } else {
        "outside the conversion pause"
    };
    stored.state.record(AppliedStep {
        revision: due.revision.clone(),
        step: RESUBMIT_STEP.to_owned(),
        outcome: StepState::Running,
        message: Some(format!(
            "Resubmitting the {} requests the drain recorded, {place}",
            due.requests
        )),
        at: Some(migration::now().to_rfc3339()),
        by: Some(format!("thorium-operator {OPERATOR_VERSION}")),
        job: Some(job.to_owned()),
        ..AppliedStep::default()
    });
}

/// Record the `Running` entry of a resubmit Job of this attempt that runs with no entry naming it,
/// as when the state was rebuilt from the record while it ran, so it is followed rather than run
/// twice
///
/// Returns whether an entry was recorded.
///
/// # Arguments
///
/// * `stored` - The recorded upgrade state
/// * `due` - The attempt being resubmitted
/// * `found` - The resubmit Jobs that exist
fn adopt(stored: &mut StoredState, due: &Attempt, found: &[Job]) -> bool {
    // a running Job of this attempt that no entry names
    let unknown = found.iter().find(|job| {
        jobs::phase(job) == JobPhase::Active
            && migration::annotation(job, ATTEMPT_ANNOTATION) == Some(due.at.as_str())
            && !entries(&stored.state, due).any(|entry| entry.job.as_deref() == Some(name_of(job)))
    });
    let Some(job) = unknown else {
        return false;
    };
    // its annotation says whether it runs inside the pause
    let flag = migration::annotation(job, IN_PAUSE_ANNOTATION) == Some("true");
    let job = name_of(job).to_owned();
    record_running(stored, due, &job, flag);
    true
}

/// Create a resubmit Job, recording its `Running` entry first so a crash never loses its count
///
/// Returns the Job's name.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `stored` - The recorded upgrade state, which is saved with the entry
/// * `due` - The attempt being resubmitted
/// * `flag` - Whether to give it the in-pause flag
async fn create(
    meta: &ClusterMeta,
    stored: &mut StoredState,
    due: &Attempt,
    flag: bool,
) -> Result<String, Error> {
    // the attempt, the Job's number in it, and the refresh annotation make its name its own
    let count = started(&stored.state, due);
    let count_text = count.to_string();
    let refresh = refresh_value(meta);
    let job = jobs::job_name(
        JobKind::Migration,
        RESUBMIT_STEP,
        &[due.at.as_str(), count_text.as_str(), refresh.as_str()],
    );
    // the run, with the flag only inside the pause
    let mut args = ["migrate", "run", "--id", RESUBMIT_STEP]
        .map(ToOwned::to_owned)
        .to_vec();
    if flag {
        args.extend(["--in-pause".to_owned(), due.at.clone()]);
    }
    // the earlier Jobs ended and the operator runs no writer beside this one, so a lease a failed
    // pod left is broken
    if count > 0 {
        args.push("--force-break-lease".to_owned());
    }
    let mut spec = jobs::base_spec(
        JobKind::Migration,
        RESUBMIT_STEP,
        job.clone(),
        meta.cluster.get_image(),
        args,
    )?;
    spec.annotations.extend([
        (IN_PAUSE_ANNOTATION.to_owned(), flag.to_string()),
        (ATTEMPT_ANNOTATION.to_owned(), due.at.clone()),
    ]);
    // record the entry, then create the Job
    record_running(stored, due, &job, flag);
    state::save(&meta.client, &meta.namespace, stored).await?;
    jobs::create(&spec, meta).await?;
    event!(
        Level::INFO,
        job = job,
        in_pause = flag,
        requests = due.requests,
        "Created the resubmit Job"
    );
    Ok(job)
}

/// Record that a resubmit Job ended without finishing the resubmission
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `stored` - The recorded upgrade state, which is saved with the entry
/// * `due` - The attempt being resubmitted
/// * `job` - The Job's name
/// * `why` - How it ended
/// * `summary` - The termination summary its pod left, if any
async fn record_failed(
    meta: &ClusterMeta,
    stored: &mut StoredState,
    due: &Attempt,
    job: &str,
    why: &str,
    summary: Option<&Value>,
) -> Result<(), Error> {
    // the run's counts, when its pod left them
    let work = summary
        .and_then(|summary| summary.get("counts"))
        .and_then(|counts| serde_json::from_value::<PassCounts>(counts.clone()).ok())
        .map(|counts| counts.work_text());
    event!(
        Level::WARN,
        job = job,
        counts = work.as_deref().unwrap_or("none"),
        "The resubmit Job left requests to retry"
    );
    stored.state.record(AppliedStep {
        revision: due.revision.clone(),
        step: RESUBMIT_STEP.to_owned(),
        outcome: StepState::Failed,
        message: Some(format!("The resubmit Job {job} {why}")),
        at: Some(migration::now().to_rfc3339()),
        by: Some(format!("thorium-operator {OPERATOR_VERSION}")),
        job: Some(job.to_owned()),
        work,
        ..AppliedStep::default()
    });
    state::save(&meta.client, &meta.namespace, stored).await
}

/// Describe the newest resubmit Job that failed in this attempt, with its counts
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
/// * `due` - The attempt being resubmitted
fn failure_note(state: &UpgradeState, due: &Attempt) -> Option<String> {
    // the newest `Failed` entry names the Job and what it counted
    let failed = entries(state, due)
        .filter(|entry| entry.outcome == StepState::Failed)
        .last()?;
    let job = failed.job.as_deref().unwrap_or("unknown");
    let counts = failed.work.as_deref().unwrap_or("it left no counts");
    Some(format!(
        "The resubmit Job {job} left requests to retry ({counts})"
    ))
}

/// Find thoradm's entry finishing the resubmission in the record a status Job read
///
/// # Arguments
///
/// * `status` - The record status
/// * `state` - The recorded upgrade state
/// * `due` - The attempt being resubmitted
fn finished_entry(
    status: &RecordStatus,
    state: &UpgradeState,
    due: &Attempt,
) -> Option<AppliedStep> {
    // only a recorded history can hold it
    let RecordStatus::Recorded { record, .. } = status else {
        return None;
    };
    // a `Done` entry newer than the drain's
    let drained = time_of(&state.applied[due.done])?;
    record
        .history
        .iter()
        .rev()
        .find(|entry| {
            entry.step == RESUBMIT_STEP
                && entry.outcome == StepState::Done
                && time_of(entry).is_some_and(|at| at > drained)
        })
        .cloned()
}

/// Report the resubmission in the `ReactionsResubmitted` condition, with counts only
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `done` - Whether the resubmission finished
/// * `message` - What an admin should know
async fn report(meta: &ClusterMeta, done: bool, message: String) {
    // a resubmission that hasn't finished is reported too, so a failure is seen
    crds::report_upgrade_condition(
        &meta.client,
        &meta.status,
        UpgradeConditionType::ReactionsResubmitted,
        done,
        message,
    )
    .await;
}

/// What following the resubmission comes to while it holds the pause or not
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
fn waiting(state: &UpgradeState) -> Tracked {
    // a held pause holds the reconcile, and otherwise the Job is looked at again soon
    if holds(state) {
        Tracked::Hold(Action::requeue(Duration::from_secs(RUNNING_REQUEUE_SECS)))
    } else {
        Tracked::Again(RUNNING_REQUEUE_SECS)
    }
}

/// Follow a resubmit Job whose entry is open
///
/// Returns what to ask of the reconcile, or nothing when the Job ended without finishing the
/// resubmission, so another may be created.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `stored` - The recorded upgrade state
/// * `due` - The attempt being resubmitted
/// * `open` - The Job's `Running` entry
/// * `found` - The resubmit Jobs that exist
async fn follow_open(
    meta: &ClusterMeta,
    stored: &mut StoredState,
    due: &Attempt,
    open: &AppliedStep,
    found: &[Job],
) -> Result<Option<Tracked>, Error> {
    // the Job the entry names
    let name = open.job.clone().unwrap_or_default();
    let Some(job) = found.iter().find(|job| name_of(job) == name) else {
        record_failed(meta, stored, due, &name, "is gone", None).await?;
        return Ok(None);
    };
    match jobs::phase(job) {
        // a running Job, or one being deleted whose pods may still run, is waited on
        JobPhase::Active => {
            let lead = failure_note(&stored.state, due)
                .map_or_else(String::new, |note| format!("{note}. "));
            let message = format!(
                "{lead}Resubmitting the {} requests the drain recorded in Job {name}",
                due.requests
            );
            report(meta, false, message).await;
            Ok(Some(waiting(&stored.state)))
        }
        // a failed Job leaves its requests to retry
        JobPhase::Failed { .. } => {
            let summary = match jobs::state(meta, job).await? {
                JobState::Failed { summary, .. } => summary,
                _ => None,
            };
            record_failed(meta, stored, due, &name, "failed", summary.as_ref()).await?;
            Ok(None)
        }
        // a finished one is done when thoradm recorded the resubmission's entry
        JobPhase::Complete => {
            let uid = job.metadata.uid.clone().unwrap_or_default();
            let purpose = format!("resubmit-{uid}");
            let read = match record::read_report(meta, &purpose, Some(&uid)).await? {
                Read::Report(read) => read,
                Read::Wait(_) => return Ok(Some(waiting(&stored.state))),
                Read::Failed(why) => {
                    event!(
                        Level::WARN,
                        job = name,
                        why = why,
                        "Reading the resubmit result failed"
                    );
                    return Ok(Some(waiting(&stored.state)));
                }
            };
            if let Some(done) = finished_entry(&read.status, &stored.state, due) {
                // the entry is copied as thoradm wrote it, so the record's copy matches it
                let work = done.work.clone().unwrap_or_default();
                stored.state.record(done);
                state::save(&meta.client, &meta.namespace, stored).await?;
                event!(
                    Level::INFO,
                    job = name,
                    counts = work,
                    "The drained requests were resubmitted"
                );
                report(
                    meta,
                    true,
                    format!("The requests the drain recorded were resubmitted: {work}"),
                )
                .await;
                return Ok(Some(Tracked::Idle));
            }
            let summary = match jobs::state(meta, job).await? {
                JobState::Succeeded(summary) => Some(summary),
                _ => None,
            };
            let why = "finished but left requests recorded";
            record_failed(meta, stored, due, &name, why, summary.as_ref()).await?;
            Ok(None)
        }
    }
}

/// Run the next resubmit Job when one is called for: inside the pause while Jobs are left in this
/// attempt, and otherwise only when the refresh annotation changed
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `stored` - The recorded upgrade state
/// * `due` - The attempt being resubmitted
async fn next(
    meta: &ClusterMeta,
    stored: &mut StoredState,
    due: &Attempt,
) -> Result<Tracked, Error> {
    // what the last failed Job left, which every message leads with
    let failure = failure_note(&stored.state, due);
    let lead = failure.map_or_else(String::new, |note| format!("{note}. "));
    // inside the pause the next Job runs at once
    let count = started(&stored.state, due);
    if pause_held(&stored.state, due) && count < MAX_JOBS {
        let flag = in_pause(meta, &stored.state, due).await?;
        let job = create(meta, stored, due, flag).await?;
        let message = format!(
            "{lead}Resubmitting the {} requests the drain recorded in Job {job}, {} of {MAX_JOBS} \
             inside the conversion pause",
            due.requests,
            count + 1
        );
        report(meta, false, message).await;
        return Ok(Tracked::Hold(Action::requeue(Duration::from_secs(
            RUNNING_REQUEUE_SECS,
        ))));
    }
    // outside the pause only a change of the refresh annotation runs it again
    let refresh = refresh_value(meta);
    let seen = stored.state.schema.get(REFRESH_MARKER).cloned();
    stored
        .state
        .schema
        .insert(REFRESH_MARKER.to_owned(), refresh.clone());
    match seen {
        Some(seen) if seen != refresh => {
            let job = create(meta, stored, due, false).await?;
            let message = format!(
                "{lead}Resubmitting the {} requests the drain recorded in Job {job}, outside the \
                 conversion pause",
                due.requests
            );
            report(meta, false, message).await;
            Ok(Tracked::Again(RUNNING_REQUEUE_SECS))
        }
        Some(_) => Ok(Tracked::Idle),
        None => {
            // the first time the pause isn't held, which the condition explains
            state::save(&meta.client, &meta.namespace, stored).await?;
            let message = format!(
                "{lead}The {} requests the drain recorded are still to be resubmitted, and the \
                 conversion pause is no longer held ({count} resubmit Jobs ran inside it); set \
                 the {REFRESH_ANNOTATION} annotation to a new value to run it again",
                due.requests
            );
            report(meta, false, message).await;
            Ok(Tracked::Idle)
        }
    }
}

/// Follow the resubmission over a loaded state
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `stored` - The recorded upgrade state
async fn try_follow(meta: &ClusterMeta, stored: &mut StoredState) -> Result<Tracked, Error> {
    // a state with nothing due forgets the refresh annotation it saw for an earlier attempt
    let Some(due) = due(&stored.state) else {
        if stored.state.schema.remove(REFRESH_MARKER).is_some() {
            state::save(&meta.client, &meta.namespace, stored).await?;
        }
        return Ok(Tracked::Idle);
    };
    // a running Job no entry names is taken over rather than run twice
    let found = jobs::find(meta, JobKind::Migration, RESUBMIT_STEP).await?;
    if adopt(stored, &due, &found) {
        state::save(&meta.client, &meta.namespace, stored).await?;
    }
    // follow the open Job, then run the next one when it ended without finishing
    if let Some(open) = open_entry(&stored.state, &due).cloned()
        && let Some(tracked) = follow_open(meta, stored, &due, &open, &found).await?
    {
        return Ok(tracked);
    }
    next(meta, stored, &due).await
}

/// Follow the resubmission over a state already loaded: run, follow, and report its Jobs, holding
/// the reconcile while it holds the pause
///
/// A failure while it holds the pause is returned so nothing is restored; otherwise it is logged
/// and tried again after a while.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `stored` - The recorded upgrade state
#[instrument(name = "resubmit::follow", skip_all)]
pub async fn follow(meta: &ClusterMeta, stored: &mut StoredState) -> Result<Tracked, Error> {
    // a failure never lets the pause end early
    match try_follow(meta, stored).await {
        Ok(tracked) => Ok(tracked),
        Err(error) if holds(&stored.state) => Err(error),
        Err(error) => {
            event!(
                Level::WARN,
                msg = "Following the resubmission failed",
                error = crate::trace::describe(&error)
            );
            Ok(Tracked::Again(RETRY_SECS))
        }
    }
}

/// Follow the resubmission of the requests a drain recorded
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
pub async fn track(meta: &ClusterMeta) -> Result<Tracked, Error> {
    // without a recorded state there is no drain to follow
    let Some(mut stored) = state::load(&meta.client, &meta.namespace).await? else {
        return Ok(Tracked::Idle);
    };
    follow(meta, &mut stored).await
}
