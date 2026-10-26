//! Data migrations run as Jobs: the read-only check per gated step, the approval gate, the pause of
//! the components the step changes data under, the migration Job and what becomes of it
//!
//! A step that `thoradm migrate run` performs is planned with a check Job (`thoradm migrate check`)
//! whose report the operator caches in the upgrade state, so the admin approves knowing the scope.
//! The step runs only with an approval naming a backup. The operator then records the replica counts
//! of the components the step pauses on the step's `Running` entry, scales them to zero, and creates
//! the migration Job, which it re-attaches to after a restart, cancels when the approval is removed,
//! and holds failed until the approval changes. Nothing here reaches Scylla: thoradm writes the
//! records, and the operator reads them through Jobs.

use chrono::{DateTime, Utc};
use k8s_openapi::api::batch::v1::Job;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use thorium::Error;
use thorium::models::schema::{CatalogIdentity, CheckReport, PassCounts, catalog_identity};
use thorium::models::upgrades::{
    AppliedStep, BlockReason, CONSUMED_REFUSAL, CatalogEntry, IssueSummary, JobFailure,
    PlannedStep, RESET_STATE_STEP, RESTORED_STEP, Reversibility, RevisionId, StepDef, StepLoss,
    StepState, UpgradeApproval, UpgradeConditionType, UpgradeState, Work,
};

use super::lossy;
use super::pause;
use super::record::{self, Read};
use super::state::{self, StoredState};
use super::steps;
use super::{OPERATOR_VERSION, Pending, catalog};
use crate::k8s::clusters::ClusterMeta;
use crate::k8s::crds;
use crate::k8s::jobs::{self, JobKind, JobPhase, JobSpec, JobState, Pinned};

/// The annotation on a migration Job holding the hash of the approval it runs under
pub(super) const APPROVAL_ANNOTATION: &str = "thorium.sandia.gov/approval";

/// The annotation on a migration or check Job holding its attempt counter
const ATTEMPT_ANNOTATION: &str = "thorium.sandia.gov/attempt";

/// The annotation on a migration Job holding the approval's backup reference
const BACKUP_ANNOTATION: &str = "thorium.sandia.gov/backup";

/// The annotation on a migration Job holding the confirmation code of the lossy step it runs
pub(super) const CONFIRM_ANNOTATION: &str = "thorium.sandia.gov/confirm";

/// The annotation on a migration Job holding the issue codes its approval accepted
const ACCEPTED_ANNOTATION: &str = "thorium.sandia.gov/accepted-issues";

/// The annotation on a migration Job holding the revision the upgrade moves to
const TARGET_ANNOTATION: &str = "thorium.sandia.gov/target-revision";

/// The annotation on a migration or check Job holding the revision of the step's edge
const REVISION_ANNOTATION: &str = "thorium.sandia.gov/revision";

/// The annotation on a check Job holding the key its report is cached under
const CHECK_KEY_ANNOTATION: &str = "thorium.sandia.gov/check-key";

/// The annotation on a `ThoriumCluster` that runs a gated step's check again when its value changes
pub const REFRESH_ANNOTATION: &str = "thorium.sandia.gov/refresh-check";

/// The prefix of the state markers holding a step's cached check
const CHECK_MARKER: &str = "check:";

/// The state marker holding the catalog identity the last status Job reported
pub const CATALOG_MARKER: &str = "catalog";

/// The marker in the upgrade state holding the revision the running upgrade started from, which
/// bounds the entries its messages list however far the recorded revision has moved
pub(super) const START_MARKER: &str = "upgrade_start";

/// How many workers a check Job's pass runs with, since checks share the cluster with live traffic
const CHECK_SLICES: &str = "4";

/// How long in seconds to wait before checking on a running Job
const RUNNING_REQUEUE_SECS: u64 = 15;

/// The fewest seconds a check that asks to wait is run again after, whatever it asks for
const MIN_WAIT_SECS: u64 = 5;

/// The most seconds a check that asks to wait is run again after, whatever it asks for
const MAX_WAIT_SECS: u64 = 600;

/// How soon in seconds the record is read again while a step is held failed, since an admin may
/// finish the step by hand
const FAILED_READ_SECS: i64 = 60;

/// How many of the largest issue groups a message names
const LARGEST_GROUPS: usize = 5;

/// The most steps the `MigrationIssues` condition lists, the rest being counted
const ISSUE_STEPS: usize = 5;

/// Where an approval message points an admin to read about what backups don't cover
pub(super) const BACKUP_DOCS: &str =
    "the backup limitations in the admin guide (admins/thoradm/taking_backups.md)";

#[cfg(test)]
thread_local! {
    /// The time tests give the operator on their thread, so waits can lapse without sleeping
    pub(crate) static NOW: std::cell::RefCell<Option<DateTime<Utc>>> =
        const { std::cell::RefCell::new(None) };
}

/// Get the time the operator acts at
pub(super) fn now() -> DateTime<Utc> {
    // a test's injected time stands in for the clock
    #[cfg(test)]
    if let Some(at) = NOW.with(|now| *now.borrow()) {
        return at;
    }
    Utc::now()
}

/// What advancing a step that runs as a migration came to
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Advance {
    /// The step had nothing to change, and why
    Skipped(String),
    /// The step finished, with the entry to record
    Finished(Box<AppliedStep>),
    /// The step is held in a state until something changes
    Held {
        /// The state to report for the step
        state: StepState,
        /// What the step waits on
        message: String,
    },
}

/// A check Job's report cached for the step it checked, and what it was cached for
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct DoneCheck {
    /// The key of the step's state when the check Job was created
    base: String,
    /// The signature of the step's approvals then
    approval: String,
    /// When the report was read (RFC 3339)
    at: String,
    /// The report
    report: CheckReport,
}

/// A check Job created and not yet read
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PendingCheck {
    /// The Job's name
    name: String,
    /// The key of the step's state when it was created
    base: String,
    /// The signature of the step's approvals then
    approval: String,
}

/// What the state remembers of a step's checks
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct CheckMark {
    /// How many check Jobs the step has had, which keeps each Job's name its own
    runs: u32,
    /// The check Job that hasn't been read yet
    job: Option<PendingCheck>,
    /// The newest report read
    done: Option<DoneCheck>,
}

/// What asking for a step's check came to
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Check {
    /// The step's check report, current for the step's state
    Ready(Box<CheckReport>),
    /// The check Job is running or being read, and what is waited on
    Wait(String),
    /// The check Job failed, and how
    Failed(String),
}

/// Get a Job's name
///
/// # Arguments
///
/// * `job` - The Job
fn name(job: &Job) -> String {
    // a Job read from the API has a name
    job.metadata.name.clone().unwrap_or_default()
}

/// Get an annotation of a Job
///
/// # Arguments
///
/// * `job` - The Job
/// * `key` - The annotation's key
pub(super) fn annotation<'a>(job: &'a Job, key: &str) -> Option<&'a str> {
    // read it from the Job's metadata
    job.metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(key))
        .map(String::as_str)
}

/// Hash parts, each after its length so parts can't run into each other, as lowercase hex
///
/// # Arguments
///
/// * `parts` - What the hash covers
fn hash(parts: &[&str]) -> String {
    // length prefixed parts into one digest
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

/// Get the approval that lets a step run: the first for it that names a backup
///
/// # Arguments
///
/// * `step` - The step
/// * `approvals` - The approvals set on the cluster
pub(super) fn usable<'a>(
    step: &'static StepDef,
    approvals: &'a [UpgradeApproval],
) -> Option<&'a UpgradeApproval> {
    // an approval without a backup doesn't count
    approvals
        .iter()
        .find(|approval| approval.step == step.id && !approval.backup.trim().is_empty())
}

/// Whether a finished run already used up an approval of a step that can run again
///
/// A catalog step runs once, so its approval needs no tracking. A re-bucket step is planned again
/// by every refused partition size change, so an approval whose backup a finished run of the step
/// recorded belongs to that move, and the next one waits for a backup of its own.
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
/// * `step` - The step
/// * `approval` - The approval
fn spent(state: &UpgradeState, step: &'static StepDef, approval: &UpgradeApproval) -> bool {
    // only the steps the schema apply plans again can run more than once
    super::rebucket::STEPS
        .iter()
        .any(|known| known.id == step.id)
        && state.applied.iter().any(|entry| {
            entry.step == step.id
                && entry.outcome.finishes()
                && entry.backup.as_deref().map(str::trim) == Some(approval.backup.trim())
        })
}

/// Get the approval that lets a step run now: the first for it that names a backup no finished
/// run of the step used up
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
/// * `step` - The step
/// * `approvals` - The approvals set on the cluster
pub(super) fn usable_now<'a>(
    state: &UpgradeState,
    step: &'static StepDef,
    approvals: &'a [UpgradeApproval],
) -> Option<&'a UpgradeApproval> {
    // an approval with a backup counts until a finished run of the step used it
    approvals.iter().find(|approval| {
        approval.step == step.id
            && !approval.backup.trim().is_empty()
            && !spent(state, step, approval)
    })
}

/// List the issue codes an approval accepts, sorted and without repeats
///
/// # Arguments
///
/// * `approval` - The approval
fn accepted(approval: &UpgradeApproval) -> Vec<String> {
    // trimmed, sorted, and unique
    approval
        .accept_issues
        .iter()
        .map(|code| code.trim().to_owned())
        .filter(|code| !code.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// List the issue codes earlier finished runs of a step consumed
///
/// An approval's accepted codes are used up by the run they were given for, so a later run of the
/// step, after its revision was reset, never reuses them.
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
/// * `step` - The step's id
fn consumed(state: &UpgradeState, step: &str) -> BTreeSet<String> {
    // the codes recorded on the step's finished entries
    state
        .applied
        .iter()
        .filter(|entry| entry.step == step && entry.outcome.finishes())
        .filter_map(|entry| entry.issues.as_ref())
        .flat_map(|issues| issues.accepted.iter().cloned())
        .collect()
}

/// List the issue codes a run is given: the approval's, less those a finished run consumed
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
/// * `step` - The step's id
/// * `approval` - The step's approval
pub(super) fn effective_accepted(
    state: &UpgradeState,
    step: &str,
    approval: &UpgradeApproval,
) -> Vec<String> {
    // codes already consumed by a finished run don't count again
    let used = consumed(state, step);
    accepted(approval)
        .into_iter()
        .filter(|code| !used.contains(code))
        .collect()
}

/// Hash what makes an approval a fresh one: its backup and the codes it accepts
///
/// # Arguments
///
/// * `backup` - The approval's backup reference
/// * `accepted` - The codes the run is given
/// * `confirm` - The confirmation code a lossy step's approval repeats
pub(super) fn approval_hash(backup: &str, accepted: &[String], confirm: Option<&str>) -> String {
    // a changed backup, a changed list of codes, or a changed confirmation code is a new approval
    hash(&[
        backup.trim(),
        &accepted.join(","),
        confirm.unwrap_or_default(),
    ])[..16]
        .to_owned()
}

/// Sign the approvals set for a step, whether or not they name a backup
///
/// # Arguments
///
/// * `step` - The step
/// * `approvals` - The approvals set on the cluster
fn approval_signature(step: &'static StepDef, approvals: &[UpgradeApproval]) -> String {
    // every approval for the step as written, in order
    let written = approvals
        .iter()
        .filter(|approval| approval.step == step.id)
        .map(|approval| {
            format!(
                "{}|{}",
                approval.backup.trim(),
                accepted(approval).join(",")
            )
        })
        .collect::<Vec<_>>();
    hash(&[&written.join(";")])[..16].to_owned()
}

/// Get the key of the state a step's check was made for
///
/// The key covers the step, the revision the deployment is recorded at, the image, how many runs
/// of the step have finished, which of the step's `reads_after` steps had finished, and the value
/// of the refresh annotation, so a check is made again only when one of them changes. A step that
/// can run again, like a re-bucket, is checked again after each run.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade being run
/// * `step` - The step
fn base_key(meta: &ClusterMeta, pending: &Pending, step: &'static StepDef) -> String {
    // which of the steps this one reads after have finished
    let catalog = catalog();
    let finished = step
        .reads_after
        .iter()
        .filter(|id| {
            catalog
                .step(id)
                .is_some_and(|(edge, _)| catalog.finished(&pending.stored.state, edge.id, id))
        })
        .copied()
        .collect::<Vec<_>>();
    // how many runs of this step have finished, which only a step that runs again can raise
    let runs = pending
        .stored
        .state
        .applied
        .iter()
        .filter(|entry| entry.step == step.id && entry.outcome.finishes())
        .count()
        .to_string();
    let refresh = meta
        .cluster
        .metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(REFRESH_ANNOTATION))
        .map_or("", String::as_str);
    hash(&[
        step.id,
        pending.stored.state.revision.as_str(),
        &meta.cluster.get_image(),
        &runs,
        &finished.join(","),
        refresh,
    ])
}

/// Get the key of the state marker holding a step's cached check
///
/// # Arguments
///
/// * `step` - The step's id
fn mark_key(step: &str) -> String {
    // one marker per step
    format!("{CHECK_MARKER}{step}")
}

/// Read what the state remembers of a step's checks
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
/// * `step` - The step's id
fn mark(state: &UpgradeState, step: &str) -> CheckMark {
    // a marker that is missing or doesn't parse remembers nothing
    state
        .schema
        .get(&mark_key(step))
        .and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or_default()
}

/// Save what the state remembers of a step's checks
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade being run, whose state is saved
/// * `step` - The step's id
/// * `mark` - What to remember
async fn save_mark(
    meta: &ClusterMeta,
    pending: &mut Pending,
    step: &str,
    mark: &CheckMark,
) -> Result<(), Error> {
    // keep the marker beside the schema markers and save the state at the version read
    let raw = serde_json::to_string(mark)
        .map_err(|error| Error::new(format!("Failed to serialize a check marker: {error}")))?;
    pending.stored.state.schema.insert(mark_key(step), raw);
    state::save(&meta.client, &meta.namespace, &mut pending.stored).await
}

/// Get how long in seconds a check that asked to wait is waited for
///
/// # Arguments
///
/// * `asked` - How long the check asked for
fn clamp_wait(asked: u64) -> u64 {
    // never a busy loop, never most of an hour
    asked.clamp(MIN_WAIT_SECS, MAX_WAIT_SECS)
}

/// Get when a cached check that asked to wait is run again
///
/// # Arguments
///
/// * `done` - The cached check
fn retry_at(done: &DoneCheck) -> Option<DateTime<Utc>> {
    // only a report with a wait has a time to run again
    let wait = done.report.wait.as_ref()?;
    let at = DateTime::parse_from_rfc3339(&done.at)
        .ok()?
        .with_timezone(&Utc);
    let secs = i64::try_from(clamp_wait(wait.retry_after_secs)).unwrap_or(i64::MAX);
    Some(at + chrono::Duration::seconds(secs))
}

/// Whether a cached check still holds for a step's state
///
/// It holds when it was made for the same state and, until the step is approved, the same
/// approvals, and when a wait it asked for hasn't lapsed.
///
/// # Arguments
///
/// * `done` - The cached check
/// * `base` - The key of the step's state now
/// * `approval` - The signature of the step's approvals now
/// * `approved` - Whether the step has an approval naming a backup
fn holds(done: &DoneCheck, base: &str, approval: &str, approved: bool) -> bool {
    // the same state, the same approvals unless approved, and no wait that ran out
    done.base == base
        && (approved || done.approval == approval)
        && retry_at(done).is_none_or(|at| now() < at)
}

/// Append formatted text to a message
///
/// # Arguments
///
/// * `message` - The message to append to
/// * `text` - What to append
fn append(message: &mut String, text: std::fmt::Arguments<'_>) {
    // writing to a string can't fail
    let _ = message.write_fmt(text);
}

/// Describe a duration in seconds for an admin
///
/// # Arguments
///
/// * `secs` - The duration
fn duration(secs: u64) -> String {
    // minutes once it is a minute or more, hours once it is an hour or more
    match secs {
        0..=59 => format!("{secs} seconds"),
        60..=3599 => format!("{} minutes", secs.div_ceil(60)),
        _ => format!("{}.{} hours", secs / 3600, secs % 3600 * 10 / 3600),
    }
}

/// List the largest issue groups in the form a message shows them: the code, its class, and count
///
/// # Arguments
///
/// * `groups` - The groups as `(code, class, count)`, in any order
fn groups_text(groups: &[(String, String, u64)]) -> String {
    // the largest first, ties broken by code
    let mut sorted = groups.to_vec();
    sorted.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    sorted
        .iter()
        .take(LARGEST_GROUPS)
        .map(|(code, class, count)| format!("{code} ({class}, {count})"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Describe what a check found, for the step's status and its approval message
///
/// # Arguments
///
/// * `step` - The step checked
/// * `report` - The check's report
fn describe(step: &'static StepDef, report: &CheckReport) -> String {
    // the objects by class, the rows to rewrite with the estimated time, and the largest groups
    let counts = &report.counts;
    let mut text = format!(
        "{} of {} objects need migrating ({} unrecognized, {} with errors)",
        counts.migratable, counts.read, counts.unrecognized, counts.error
    );
    let rows = report.rows.values().sum::<u64>();
    if rows > 0 {
        append(&mut text, format_args!(", {rows} rows to rewrite"));
    }
    if let Some(secs) = report.estimated_seconds {
        append(
            &mut text,
            format_args!(
                ", about {} while the paused components are down",
                duration(secs)
            ),
        );
    }
    text.push('.');
    if !step.pause.is_empty() {
        let paused = step
            .pause
            .iter()
            .map(|component| component.deployment_name())
            .collect::<Vec<_>>();
        append(
            &mut text,
            format_args!(" It pauses {} while it runs.", paused.join(", ")),
        );
    }
    if !report.reasons.is_empty() {
        let groups = report
            .reasons
            .iter()
            .map(|reason| {
                (
                    reason.code.clone(),
                    reason.class.as_str().to_owned(),
                    reason.count,
                )
            })
            .collect::<Vec<_>>();
        append(
            &mut text,
            format_args!(
                " Largest issue groups: {}{}.",
                groups_text(&groups),
                if report.more > 0 {
                    format!(" and {} more", report.more)
                } else {
                    String::new()
                }
            ),
        );
    }
    text
}

/// Judge what a step's check found
///
/// # Arguments
///
/// * `step` - The step checked
/// * `report` - The check's report
pub(super) fn work(step: &'static StepDef, report: &CheckReport) -> Work {
    // nothing to change when every object is in the target layout
    let counts = &report.counts;
    if counts.migratable == 0 && counts.unrecognized == 0 && counts.error == 0 {
        // a step that only checks or waits converts nothing, so it found nothing to wait for
        let why = if step.check_only {
            format!(
                "none of the {} objects the check read needs this step",
                counts.read
            )
        } else {
            format!(
                "every one of the {} objects is already in the target layout",
                counts.read
            )
        };
        return Work::Nothing(why);
    }
    Work::Needed(describe(step, report))
}

/// Build the message that asks an admin to approve a step
///
/// # Arguments
///
/// * `step` - The step waiting for an approval
/// * `found` - What the step's check found
/// * `approvals` - The approvals set on the cluster
fn approval_message(step: &'static StepDef, found: &str, approvals: &[UpgradeApproval]) -> String {
    // the shared approval message, then how to take the backup and inspect the issues
    let blank = approvals
        .iter()
        .any(|approval| approval.step == step.id && approval.backup.trim().is_empty());
    let mut message = steps::approval_needed(step, found, blank);
    append(
        &mut message,
        format_args!(" Take the backup with `thoradm backup new` and read {BACKUP_DOCS} first."),
    );
    if let Some(migration) = step.thoradm_migration {
        append(
            &mut message,
            format_args!(
                " See every issue group and sample keys with `thoradm migrate check --id {migration} \
             --all --samples`; to run the step despite a group of issues, add its code to the \
             approval's acceptIssues (Helm value operator.cluster.upgrade.approvals[].acceptIssues)."
            ),
        );
    }
    message
}

/// Build the check Job for a step
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade being run
/// * `step` - The step to check
/// * `migration` - The thoradm migration the step runs
/// * `runs` - The number of this check among the step's
/// * `base` - The key of the step's state the check is made for
fn check_spec(
    meta: &ClusterMeta,
    pending: &Pending,
    step: &'static StepDef,
    migration: &str,
    runs: u32,
    base: &str,
) -> Result<JobSpec, Error> {
    // name the Job from what it checks and when, so no earlier Job's name is reused
    let runs = runs.to_string();
    let job = jobs::job_name(
        JobKind::MigrationCheck,
        step.id,
        &[pending.target.as_str(), "check", &runs, base],
    );
    // a read-only classification of every object, with few workers
    let args = [
        "migrate",
        "check",
        "--id",
        migration,
        "--slices",
        CHECK_SLICES,
    ]
    .map(ToOwned::to_owned)
    .to_vec();
    let mut spec = jobs::base_spec(
        JobKind::MigrationCheck,
        step.id,
        job,
        meta.cluster.get_image(),
        args,
    )?;
    // a check is short next to a migration, and nothing waits on its logs for long
    spec.active_deadline_seconds = 3600;
    spec.annotations.extend([
        (ATTEMPT_ANNOTATION.to_owned(), runs),
        (CHECK_KEY_ANNOTATION.to_owned(), base.to_owned()),
        (
            REVISION_ANNOTATION.to_owned(),
            pending.stored.state.revision.to_string(),
        ),
    ]);
    Ok(spec)
}

/// Describe a failed Job for a message
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `job` - The failed Job
async fn failed_text(meta: &ClusterMeta, job: &Job) -> Result<String, Error> {
    // read why it failed and the end of its pod's log
    let found = jobs::state(meta, job).await?;
    let (reason, tail) = match found {
        JobState::Failed {
            reason, log_tail, ..
        } => (reason, log_tail.join(" | ")),
        _ => (String::new(), String::new()),
    };
    Ok(format!("{reason} {tail}").trim().to_owned())
}

/// Read a finished check Job's report through a status Job and cache it
///
/// Returns what to wait on or fail with when the report isn't cached yet.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade being run, whose state holds the cache
/// * `step` - The step
/// * `created` - The check Job as the state remembers it
/// * `job` - The finished Job
/// * `remembered` - What the state remembers of the step's checks, which gains the report
async fn collect(
    meta: &ClusterMeta,
    pending: &mut Pending,
    step: &'static StepDef,
    created: &PendingCheck,
    job: &Job,
    remembered: &mut CheckMark,
) -> Result<Option<Check>, Error> {
    // ask for the Job's result row through a status Job
    let uid = job.metadata.uid.clone().unwrap_or_default();
    let purpose = format!("check-{uid}");
    let report = match record::read_report(meta, &purpose, Some(&uid)).await? {
        Read::Wait(_) => {
            pending.requeue_hint = Some(RUNNING_REQUEUE_SECS);
            return Ok(Some(Check::Wait(format!(
                "Reading the report of the check Job {}",
                created.name
            ))));
        }
        Read::Failed(why) => {
            return Ok(Some(Check::Wait(format!(
                "Reading the report of the check Job {} failed ({why}); trying again",
                created.name
            ))));
        }
        Read::Report(report) => report,
    };
    let Some(check) = report.check else {
        return Ok(Some(Check::Failed(format!(
            "The check Job {} left no report",
            created.name
        ))));
    };
    // cache the report for the state the Job was created for
    remembered.job = None;
    remembered.done = Some(DoneCheck {
        base: created.base.clone(),
        approval: created.approval.clone(),
        at: now().to_rfc3339(),
        report: check,
    });
    save_mark(meta, pending, step.id, remembered).await?;
    Ok(None)
}

/// Ask for a step's check report: the cached one, or a check Job's, creating the Job when none runs
///
/// A step whose last report asked it to wait keeps showing that reason while its next check runs,
/// instead of the text of each polling stage of the check Job.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade being run, whose state holds the cache
/// * `step` - The step
/// * `migration` - The thoradm migration the step runs
/// * `approved` - Whether the step has an approval naming a backup
pub(super) async fn ensure_check(
    meta: &ClusterMeta,
    pending: &mut Pending,
    step: &'static StepDef,
    migration: &str,
    approved: bool,
) -> Result<Check, Error> {
    // ask for the check, which says how the polling of its Job stands while it runs
    let found = poll_check(meta, pending, step, migration, approved).await?;
    // a step that was waiting on a reason keeps it until a report says otherwise, since the text of
    // a Job being created, run, and read would replace it at every poll
    if let Check::Wait(_) = &found
        && let Some(reason) = mark(&pending.stored.state, step.id)
            .done
            .and_then(|done| done.report.wait)
            .map(|wait| wait.reason)
    {
        return Ok(Check::Wait(reason));
    }
    Ok(found)
}

/// Ask for a step's check report, with the polling text of its Job while it isn't ready
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade being run, whose state holds the cache
/// * `step` - The step
/// * `migration` - The thoradm migration the step runs
/// * `approved` - Whether the step has an approval naming a backup
async fn poll_check(
    meta: &ClusterMeta,
    pending: &mut Pending,
    step: &'static StepDef,
    migration: &str,
    approved: bool,
) -> Result<Check, Error> {
    // the state the check is for, and what the state remembers of the step's checks
    let approvals = &meta.cluster.spec.upgrade.approvals;
    let base = base_key(meta, pending, step);
    let signature = approval_signature(step, approvals);
    let mut remembered = mark(&pending.stored.state, step.id);
    // a report made for this state still holds
    if let Some(done) = &remembered.done
        && holds(done, &base, &signature, approved)
    {
        return Ok(Check::Ready(Box::new(done.report.clone())));
    }
    // a Job created and not read yet is waited on, read, or found failed or lost
    if let Some(created) = remembered.job.clone() {
        let found = jobs::find(meta, JobKind::MigrationCheck, step.id).await?;
        // a Job that is gone is made again below
        if let Some(job) = found.iter().find(|job| name(job) == created.name) {
            match jobs::phase(job) {
                JobPhase::Active => {
                    pending.requeue_hint = Some(RUNNING_REQUEUE_SECS);
                    return Ok(Check::Wait(format!(
                        "The check Job {} is running",
                        created.name
                    )));
                }
                JobPhase::Failed { .. } if created.base == base => {
                    let why = failed_text(meta, job).await?;
                    return Ok(Check::Failed(format!(
                        "The check Job {} failed ({why}); set the {REFRESH_ANNOTATION} annotation \
                         on the ThoriumCluster to a new value to check again",
                        created.name
                    )));
                }
                JobPhase::Failed { .. } => (),
                JobPhase::Complete => {
                    // read the report through a status Job, and cache it for the state the Job was
                    // created for
                    if let Some(waiting) =
                        collect(meta, pending, step, &created, job, &mut remembered).await?
                    {
                        return Ok(waiting);
                    }
                    // it holds when the state didn't change while the Job ran; otherwise another
                    // check is made below
                    if let Some(done) = &remembered.done
                        && holds(done, &base, &signature, approved)
                    {
                        return Ok(Check::Ready(Box::new(done.report.clone())));
                    }
                }
            }
        }
    }
    // check Jobs run one at a time in a namespace, since they share the cluster with live traffic
    let active = jobs::find_kind(meta, JobKind::MigrationCheck)
        .await?
        .into_iter()
        .filter(|job| jobs::phase(job) == JobPhase::Active)
        .map(|job| name(&job))
        .collect::<Vec<_>>();
    if let Some(other) = active.first() {
        pending.requeue_hint = Some(RUNNING_REQUEUE_SECS);
        return Ok(Check::Wait(format!(
            "Waiting for the check Job {other} to finish before checking step {}",
            step.id
        )));
    }
    // create the next check Job and remember it
    remembered.runs += 1;
    let spec = check_spec(meta, pending, step, migration, remembered.runs, &base)?;
    jobs::create(&spec, meta).await?;
    remembered.job = Some(PendingCheck {
        name: spec.name.clone(),
        base,
        approval: signature,
    });
    save_mark(meta, pending, step.id, &remembered).await?;
    pending.requeue_hint = Some(RUNNING_REQUEUE_SECS);
    Ok(Check::Wait(format!(
        "Checking what step {} would change with the Job {}",
        step.id, spec.name
    )))
}

/// Show a cached check in a planned step's status
///
/// # Arguments
///
/// * `pending` - The upgrade being run
/// * `index` - The index of the planned step
/// * `step` - The step
/// * `report` - Its check report
fn show(pending: &mut Pending, index: usize, step: &'static StepDef, report: &CheckReport) {
    // a lossy step's loss report comes from its check, with the counts and the backup coverage
    let planned = &mut pending.steps[index];
    if let (Some(_), Some(loss)) = (&planned.loss, &report.loss) {
        planned.loss = Some(loss.clone());
    }
    // a wait holds the step; otherwise the report's scope is the step's message while it is planned
    if let Some(wait) = &report.wait {
        planned.state = StepState::Waiting;
        planned.message = Some(wait.reason.clone());
    } else if matches!(
        planned.state,
        StepState::Pending | StepState::Waiting | StepState::Blocked
    ) {
        planned.state = if planned.state == StepState::Waiting {
            StepState::Pending
        } else {
            planned.state
        };
        planned.message = Some(describe(step, report));
    }
}

/// Run the checks of every gated step in the plan, one Job at a time, so the admin sees each
/// step's scope before approving
///
/// The steps after a schema-apply step in the plan are checked once it has run, since a check
/// reads the tables the schema apply creates.
///
/// A step whose check is cached for its state is left alone. A step that reads an earlier step's
/// output is checked again when that step finishes, and a step whose check asked to wait is checked
/// again once its wait lapses.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade being run, whose steps show the reports
pub(super) async fn preflight(meta: &ClusterMeta, pending: &mut Pending) -> Result<(), Error> {
    // check each gated step in plan order
    let approvals = meta.cluster.spec.upgrade.approvals.clone();
    for index in 0..pending.steps.len() {
        let id = pending.steps[index].step.clone();
        let Some((_, step)) = catalog().step(&id) else {
            continue;
        };
        // no check reads the data before the schema is applied, so the steps after it wait
        if steps::is_schema_apply(step) {
            break;
        }
        let Some(migration) = step
            .thoradm_migration
            .filter(|_| steps::runs_in_thoradm(step) && !reverses_dual_write(step))
        else {
            continue;
        };
        // a step already run through is shown no scope, and one with a Job running needs no check
        if open_run(&pending.stored.state, step.id).is_some() {
            continue;
        }
        let approved = usable_now(&pending.stored.state, step, &approvals).is_some();
        if let Check::Ready(report) = ensure_check(meta, pending, step, migration, approved).await?
        {
            show(pending, index, step, &report);
        }
    }
    Ok(())
}

/// What retries a failed run of a step that loses no data
const NOT_RETRIED: &str = "It is not retried by itself: change the approval's backup (normally to a \
                           newer backup) or its acceptIssues to run it again, resuming from its \
                           checkpoints";

/// What a failed run of a step that loses data says of its retry, which needs a new approval
pub(super) const NOT_RETRIED_BY_ITSELF: &str = "It is not retried by itself";

/// Find the run of a step that started and hasn't ended: its newest `Running` entry, when no entry
/// that finished or reset the step came after it
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
/// * `step` - The step's id
pub(super) fn open_run<'a>(state: &'a UpgradeState, step: &str) -> Option<&'a AppliedStep> {
    // walk back from the newest entry to the first that decides the step's run
    for entry in state.applied.iter().rev() {
        // a reset or a restore moved the revision, so the run it came after is over
        if entry.step == RESET_STATE_STEP || entry.step == RESTORED_STEP {
            return None;
        }
        if entry.step != step {
            continue;
        }
        match entry.outcome {
            StepState::Running => return Some(entry),
            outcome if outcome.finishes() => return None,
            _ => (),
        }
    }
    None
}

/// Find the open runs of the catalog steps that run as migration Jobs, in the order they started
///
/// These are the runs a hold stops: a step outside this build's catalog, or one that runs no
/// migration Job, is left alone.
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
#[must_use]
pub(super) fn open_runs(state: &UpgradeState) -> Vec<&AppliedStep> {
    // each catalog step a migration Job runs, once, by the run that is open for it
    let catalog = catalog();
    let mut seen = BTreeSet::new();
    let mut runs = catalog
        .steps()
        .filter(|(_, step)| steps::runs_in_thoradm(step) && seen.insert(step.id))
        .filter_map(|(_, step)| open_run(state, step.id))
        .collect::<Vec<_>>();
    // oldest start first, so messages name the run that began the work
    runs.sort_by_key(|run| {
        state
            .applied
            .iter()
            .position(|entry| std::ptr::eq(entry, *run))
    });
    runs
}

/// What stopping the open runs of a held cluster came to
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Sweep {
    /// The migration Jobs still stopping, or changed since they were read, which the hold looks at
    /// again soon
    pub(super) stopping: Vec<String>,
}

/// Build the upgrade of one step a hold records a stopped or ended run through, over the state the
/// hold read
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `stored` - The recorded upgrade state
/// * `open` - The open run's `Running` entry
/// * `edge` - The catalog edge the step belongs to
/// * `step` - The step
fn held_run(
    meta: &ClusterMeta,
    stored: &StoredState,
    open: &AppliedStep,
    edge: &CatalogEntry,
    step: &'static StepDef,
) -> Pending {
    // the step as a plan of one at the revision its run belongs to
    let planned = PlannedStep {
        revision: open.revision.clone(),
        step: step.id.to_owned(),
        kind: step.kind,
        description: step.description.to_owned(),
        pause: step.pause.to_vec(),
        state: StepState::Running,
        message: None,
        phase: None,
        prerelease: false,
        reverse: edge.is_downward(),
        pass: None,
        blocked: None,
        loss: None,
    };
    Pending {
        stored: stored.clone(),
        target: stored.state.revision.clone(),
        steps: vec![planned],
        approvals: meta.cluster.spec.upgrade.approvals.clone(),
        requeue_hint: None,
    }
}

/// Record in the history that a hold stopped a step's migration Job
///
/// The entry is a block of the step, as removing its approval leaves: the step is still planned
/// and resumes from its checkpoints when the target is set again.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The held run, whose state is saved
/// * `job` - The stopped Job's name
async fn record_stopped(meta: &ClusterMeta, pending: &mut Pending, job: &str) -> Result<(), Error> {
    // the block names the Job and why it was stopped
    let step = &pending.steps[0];
    let entry = AppliedStep {
        revision: step.revision.clone(),
        step: step.step.clone(),
        outcome: StepState::Blocked,
        message: Some(format!(
            "The upgrade was held before step {} finished (its target was cleared, set to the \
             deployment's revision, or can't be used), so its migration Job {job} was stopped; \
             the paused components stay paused until the upgrade resumes",
            step.step
        )),
        at: Some(now().to_rfc3339()),
        by: Some(format!("thorium-operator {OPERATOR_VERSION}")),
        job: Some(job.to_owned()),
        ..AppliedStep::default()
    };
    pending.stored.state.record(entry);
    // save it at the version read, like a finished step
    state::save(&meta.client, &meta.namespace, &mut pending.stored).await
}

/// Whether the history holds the stop of a Job: a `Blocked` entry naming the Job follows the open
/// run's `Running` entry
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
/// * `open` - The open run's `Running` entry
/// * `job` - The stopped Job's name
fn stop_recorded(state: &UpgradeState, open: &AppliedStep, job: &str) -> bool {
    // the newest entry that started the run
    let Some(start) = state.applied.iter().rposition(|entry| entry == open) else {
        return false;
    };
    // a block of the step naming the Job after it
    state.applied[start + 1..].iter().any(|entry| {
        entry.step == open.step
            && entry.outcome == StepState::Blocked
            && entry.job.as_deref() == Some(job)
    })
}

/// Stop the migration Jobs of the runs a held cluster has open, and record the ones that ended
///
/// A hold runs no step, so a Job still running would rewrite data unmanaged: it is deleted in the
/// foreground, pinned to the version read, so its pod writes its checkpoint and is gone before the
/// Job is; the stop is recorded once as a block of the step, even when the operator restarts
/// between the delete and the record. A Job of the open run that already completed is recorded as
/// its step's outcome, and one that failed is recorded failed, so neither is lost when the Job's
/// TTL deletes it. The paused components stay paused.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `stored` - The recorded upgrade state, which every record is saved in
pub(super) async fn stop_open_runs(
    meta: &ClusterMeta,
    stored: &mut StoredState,
) -> Result<Sweep, Error> {
    let mut sweep = Sweep::default();
    // the open runs, copied so the state can take records while they are walked
    let runs = open_runs(&stored.state)
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    for open in runs {
        // the catalog step the run belongs to, which open_runs only returns when it is known
        let Some((edge, step)) = catalog().step(&open.step) else {
            continue;
        };
        let migration = step.thoradm_migration.unwrap_or(step.id);
        let mut pending = held_run(meta, stored, &open, edge, step);
        for job in jobs::find(meta, JobKind::Migration, step.id).await? {
            let job_name = name(&job);
            let own = open.job.as_deref() == Some(job_name.as_str());
            match jobs::phase(&job) {
                // a Job already being deleted is still stopping, and its stop is recorded once
                // even when the operator restarted between deleting it and recording the stop
                JobPhase::Active if job.metadata.deletion_timestamp.is_some() => {
                    if !stop_recorded(&pending.stored.state, &open, &job_name) {
                        record_stopped(meta, &mut pending, &job_name).await?;
                    }
                    sweep.stopping.push(job_name);
                }
                // a running Job is stopped, unless it changed since it was read
                JobPhase::Active => match jobs::delete_unchanged(meta, &job).await? {
                    Pinned::Deleting => {
                        record_stopped(meta, &mut pending, &job_name).await?;
                        sweep.stopping.push(job_name);
                    }
                    Pinned::Changed => sweep.stopping.push(job_name),
                    Pinned::Gone => (),
                },
                // the open run's Job finished, so its outcome is recorded
                JobPhase::Complete if own => {
                    if let Advance::Finished(entry) =
                        succeeded(meta, &pending, 0, step, migration, &job).await?
                    {
                        pending.stored.state.record(*entry);
                        state::save(&meta.client, &meta.namespace, &mut pending.stored).await?;
                    }
                }
                // the open run's Job failed, so its failure is recorded once; a failure an
                // earlier run under the same Job name recorded is not this run's
                JobPhase::Failed { .. } if own => {
                    let recorded =
                        failure_since_start(&pending.stored.state, step.id, &job_name).is_some();
                    if !recorded
                        && let JobState::Failed { reason, pod, .. } =
                            jobs::state(meta, &job).await?
                    {
                        record_failed(meta, &mut pending, 0, step, &job, &reason, pod).await?;
                    }
                }
                // a Job of another run that ended has nothing left to stop or record
                _ => (),
            }
        }
        // keep what the hold recorded for the next run
        *stored = pending.stored;
    }
    Ok(sweep)
}

/// Find the failure recorded for the Job of a step's open run, which outlives the Job itself
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
/// * `open` - The open run's `Running` entry
pub(super) fn recorded_failure<'a>(
    state: &'a UpgradeState,
    open: &AppliedStep,
) -> Option<&'a AppliedStep> {
    // a run that named no Job has no failure of its Job
    let job = open.job.as_deref()?;
    // only a failure recorded since the run started is the run's own
    failure_since_start(state, &open.step, job)
}

/// Find the failure recorded for a step's Job since the newest run of that Job started
///
/// A Job's name hashes the run's backup, codes, and attempt, and the attempt counts only the Jobs
/// that still exist, so once an earlier run's Job is deleted a run under the same approval gets
/// the same name. Walking back stops at the newest `Running` entry naming the Job: a failure
/// recorded before that start is an earlier run's, and must neither hold the new run nor stand in
/// for the new run's own failure.
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
/// * `step` - The step's id
/// * `job` - The name of the run's Job
pub(super) fn failure_since_start<'a>(
    state: &'a UpgradeState,
    step: &str,
    job: &str,
) -> Option<&'a AppliedStep> {
    // walk back over the step's entries that name the Job, newest first
    for entry in state
        .applied
        .iter()
        .rev()
        .filter(|entry| entry.step == step && entry.job.as_deref() == Some(job))
    {
        match entry.outcome {
            // a failure newer than the run's start is the run's own
            StepState::Failed => return Some(entry),
            // the run's own start, so any older failure belongs to an earlier run of the name
            StepState::Running => return None,
            _ => (),
        }
    }
    None
}

/// Find the failure recorded for a step's open run whose Job is gone, when it ran under the backup
/// and issue codes a run would be given now
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
/// * `current` - The Job of the open run, if it still exists
/// * `open` - The open run's `Running` entry, if the step started
/// * `backup` - The backup a run would name now, empty for none
/// * `accept` - The issue codes a run would be given now
pub(super) fn gone_failure_entry<'a>(
    state: &'a UpgradeState,
    current: Option<&Job>,
    open: Option<&AppliedStep>,
    backup: &str,
    accept: &[String],
) -> Option<&'a AppliedStep> {
    // only a run whose Job is gone is judged from the history
    if current.is_some() {
        return None;
    }
    // the failure recorded for the run's Job, under the same backup and codes
    let entry = recorded_failure(state, open?)?;
    let codes = entry
        .issues
        .as_ref()
        .map_or(&[][..], |issues| issues.accepted.as_slice());
    let recorded = entry.backup.as_deref().map_or("", str::trim);
    (recorded == backup.trim() && codes == accept).then_some(entry)
}

/// Say what a failed run whose Job was deleted did, from its recorded failure
///
/// # Arguments
///
/// * `step` - The step
/// * `entry` - The failure recorded for the run's Job
pub(super) fn gone_failure_text(step: &StepDef, entry: &AppliedStep) -> String {
    // the recorded failure says what failed, since the Job's logs went with it
    let what = entry
        .message
        .clone()
        .unwrap_or_else(|| format!("A run of step {} failed", step.id));
    format!("{what}, and its Job was deleted with its logs")
}

/// Hold a step whose open run failed and whose Job was deleted, while the approval still matches
/// the one the run was given
///
/// # Arguments
///
/// * `pending` - The upgrade being run
/// * `step` - The step
/// * `current` - The Job of the open run, if it still exists
/// * `open` - The open run's `Running` entry, if the step started
/// * `backup` - The backup a run would name now, empty for none
/// * `accept` - The issue codes a run would be given now
/// * `retry` - What runs the step again
pub(super) fn gone_failure(
    pending: &Pending,
    step: &StepDef,
    current: Option<&Job>,
    open: Option<&AppliedStep>,
    backup: &str,
    accept: &[String],
    retry: &str,
) -> Option<Advance> {
    // the failure recorded for the gone Job under the same backup and codes holds the step
    let entry = gone_failure_entry(&pending.stored.state, current, open, backup, accept)?;
    Some(Advance::Held {
        state: StepState::Failed,
        message: format!("{}. {retry}", gone_failure_text(step, entry)),
    })
}

/// Get the revision a catalog step's edge holds
///
/// # Arguments
///
/// * `pending` - The upgrade being run
/// * `index` - The index of the planned step
pub(super) fn step_revision(pending: &Pending, index: usize) -> String {
    // the planned step carries its edge's revision
    pending.steps[index].revision.clone()
}

/// The text that says pause counts were lost and the spec's counts were recorded instead
const LOST_COUNTS: &str = "were lost before the Job started";

/// Say which components' replica counts were lost, or nothing when none were
///
/// # Arguments
///
/// * `names` - The components whose counts the spec's stand in for
fn lost_note(names: &[String]) -> String {
    // the status shows the note while the step runs
    if names.is_empty() {
        return String::new();
    }
    format!(
        ". The replica counts of {} {LOST_COUNTS}, so the counts the ThoriumCluster spec \
         declares are recorded and restored",
        names.join(", ")
    )
}

/// Build the history entry of a step's start, with the replica counts of the components it pauses
///
/// # Arguments
///
/// * `pending` - The upgrade being run
/// * `index` - The index of the planned step
/// * `step` - The step
/// * `approved` - The approval the step runs under
/// * `job` - The name of the Job the step starts
/// * `counts` - The replica counts of the components paused now
fn running_entry(
    pending: &Pending,
    index: usize,
    step: &'static StepDef,
    approved: Approved<'_>,
    job: &str,
    counts: &pause::Counts,
) -> AppliedStep {
    // every component paused now, so a later restore reads the counts from one entry
    let mut paused = pause::paused(&pending.stored.state);
    for (name, count) in &counts.counts {
        paused.entry(name.clone()).or_insert(*count);
    }
    AppliedStep {
        revision: step_revision(pending, index),
        step: step.id.to_owned(),
        outcome: StepState::Running,
        message: Some(format!(
            "Running {} with the Job {job}{}",
            step.id,
            lost_note(&counts.from_spec)
        )),
        at: Some(now().to_rfc3339()),
        by: Some(format!("thorium-operator {OPERATOR_VERSION}")),
        backup: Some(approved.backup.trim().to_owned()).filter(|backup| !backup.is_empty()),
        confirm: approved.confirm.map(ToOwned::to_owned),
        loss: approved.loss.cloned(),
        job: Some(job.to_owned()),
        chain: vec![step.id.to_owned()],
        paused,
        ..AppliedStep::default()
    }
}

/// What a run of a step is given: the approval's backup, the issue codes it accepts, and for a lossy
/// step the confirmation code and the loss the run records
///
/// The dev setting waives a lossy step's approval, so its backup is empty and it has no code.
#[derive(Debug, Clone, Copy)]
pub(super) struct Approved<'a> {
    /// The approval's backup reference, empty when the dev setting waived the approval
    pub(super) backup: &'a str,
    /// The issue codes the run is given: the approval's, less those a finished run consumed
    pub(super) accept: &'a [String],
    /// The confirmation code a lossy step's approval repeated, which the run consumes
    pub(super) confirm: Option<&'a str>,
    /// What a lossy step loses, as its check estimated it, recorded with the run
    pub(super) loss: Option<&'a StepLoss>,
}

/// Build the migration Job
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade being run
/// * `step` - The step to run
/// * `migration` - The thoradm migration the step runs
/// * `approved` - The step's approval and the codes the run is given
/// * `attempt` - The attempt counter
/// * `entry` - The step's `Running` entry
fn run_spec(
    meta: &ClusterMeta,
    pending: &Pending,
    step: &'static StepDef,
    migration: &str,
    approved: Approved<'_>,
    attempt: u32,
    entry: &AppliedStep,
) -> Result<JobSpec, Error> {
    // the approval and the codes the run is given
    let Approved {
        backup,
        accept,
        confirm,
        ..
    } = approved;
    // the name covers everything that makes an attempt its own
    let backup = backup.trim();
    let accepted_csv = accept.join(",");
    let attempt_text = attempt.to_string();
    let job = run_name(
        pending,
        step,
        backup,
        &format!("{accepted_csv}|{}", confirm.unwrap_or_default()),
        &attempt_text,
    );
    // run the migration under the approval, passing the entry thoradm writes first
    let running = serde_json::to_string(entry)
        .map_err(|error| Error::new(format!("Failed to serialize the running entry: {error}")))?;
    let mut args = ["migrate", "run", "--id", migration]
        .map(ToOwned::to_owned)
        .to_vec();
    if !backup.is_empty() {
        args.extend(["--backup".to_owned(), backup.to_owned()]);
    }
    if let Some(code) = confirm {
        args.extend(["--confirm".to_owned(), code.to_owned()]);
    }
    if !accept.is_empty() {
        args.extend(["--accept-issues".to_owned(), accepted_csv.clone()]);
    }
    args.extend(["--running-entry".to_owned(), running]);
    // the operator ran no writer beside this Job, so a lease an earlier pod left is broken
    if attempt > 0 {
        args.push("--force-break-lease".to_owned());
    }
    let mut spec = jobs::base_spec(
        JobKind::Migration,
        step.id,
        job,
        meta.cluster.get_image(),
        args,
    )?;
    // the audit fields travel with the Job
    spec.annotations.extend([
        (
            APPROVAL_ANNOTATION.to_owned(),
            approval_hash(backup, accept, confirm),
        ),
        (ATTEMPT_ANNOTATION.to_owned(), attempt_text),
        (BACKUP_ANNOTATION.to_owned(), backup.to_owned()),
        (
            CONFIRM_ANNOTATION.to_owned(),
            confirm.unwrap_or_default().to_owned(),
        ),
        (ACCEPTED_ANNOTATION.to_owned(), accepted_csv),
        (TARGET_ANNOTATION.to_owned(), pending.target.to_string()),
        (REVISION_ANNOTATION.to_owned(), entry.revision.clone()),
    ]);
    Ok(spec)
}

/// Build the name of a migration Job
///
/// # Arguments
///
/// * `pending` - The upgrade being run
/// * `step` - The step
/// * `backup` - The approval's backup reference
/// * `accepted` - The issue codes the run is given, joined
/// * `attempt` - The attempt counter
fn run_name(
    pending: &Pending,
    step: &'static StepDef,
    backup: &str,
    accepted: &str,
    attempt: &str,
) -> String {
    // the target, the approval, and the attempt make the name its own
    jobs::job_name(
        JobKind::Migration,
        step.id,
        &[pending.target.as_str(), backup, accepted, attempt],
    )
}

/// Read what a run's termination summary says about its issue groups
///
/// # Arguments
///
/// * `summary` - The summary the Job's pod wrote
fn summary_groups(summary: &Value) -> Vec<(String, String, u64)> {
    // each group names its code, class, and count
    summary
        .get("groups")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|group| {
            Some((
                group.get("code")?.as_str()?.to_owned(),
                group
                    .get("class")
                    .and_then(Value::as_str)
                    .unwrap_or("unrecognized")
                    .to_owned(),
                group.get("count")?.as_u64()?,
            ))
        })
        .collect()
}

/// List the steps of this upgrade that finished with issues before the current one, newest first
///
/// Only a step's newest finishing entry counts, so a step run again with no issues is no longer
/// listed.
///
/// # Arguments
///
/// * `pending` - The upgrade being run, whose history holds the finished steps
/// * `current` - The step that just finished, which the caller reports itself
fn earlier_issues(pending: &Pending, current: &str) -> Vec<String> {
    // the entries this upgrade made lie above the revision it started from, up to its target; the
    // recorded revision rises as each edge finishes, so the start comes from its marker
    let state = &pending.stored.state;
    let catalog = catalog();
    let from = state
        .schema
        .get(START_MARKER)
        .and_then(|raw| raw.parse::<RevisionId>().ok())
        .filter(|from| catalog.at_or_below(from, &state.revision))
        .unwrap_or_else(|| state.revision.clone());
    let mut seen = BTreeSet::from([current.to_owned()]);
    let mut found = Vec::new();
    for entry in state.applied.iter().rev() {
        // only a finished step of this upgrade, the newest entry of each
        let Ok(revision) = entry.revision.parse::<RevisionId>() else {
            continue;
        };
        if !entry.outcome.finishes()
            || catalog.at_or_below(&revision, &from)
            || !catalog.at_or_below(&revision, &pending.target)
            || !seen.insert(entry.step.clone())
        {
            continue;
        }
        // a step that left issues is listed with how to read them
        let Some(issues) = entry.issues.as_ref().filter(|issues| issues.groups > 0) else {
            continue;
        };
        let migration = catalog
            .step(&entry.step)
            .and_then(|(_, def)| def.thoradm_migration)
            .unwrap_or(entry.step.as_str());
        found.push(format!(
            "{}: {} objects in {} groups; `thoradm migrate issues --id {migration}`",
            entry.step, issues.objects, issues.groups
        ));
    }
    found
}

/// Report or clear the condition for a run that finished with issues
///
/// The condition lists every step of the upgrade that ended with issues, this run's first, so a
/// later step that ends clean doesn't hide an earlier one's issues. At most `ISSUE_STEPS` steps
/// are listed, with a count of the rest.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade being run, whose history holds the steps finished before
/// * `step` - The step that ran
/// * `migration` - The migration it ran
/// * `groups` - The issue groups the run left
/// * `unmatched` - Accepted codes that matched no group
async fn report_issues(
    meta: &ClusterMeta,
    pending: &Pending,
    step: &'static StepDef,
    migration: &str,
    groups: &[(String, String, u64)],
    unmatched: &[String],
) {
    // name the groups, their objects, and how to read them
    let objects = groups.iter().map(|group| group.2).sum::<u64>();
    let mut message = if groups.is_empty() {
        format!("{}: finished with no issues", step.id)
    } else {
        format!(
            "{}: {objects} objects in {} groups: {}; `thoradm migrate issues --id {migration}`",
            step.id,
            groups.len(),
            groups_text(groups)
        )
    };
    if !unmatched.is_empty() {
        append(
            &mut message,
            format_args!(
                "; accepted codes that matched no group were ignored: {}",
                unmatched.join(", ")
            ),
        );
    }
    // the steps that finished with issues before this one, after this run's own
    let mut listed = Vec::new();
    if !groups.is_empty() {
        listed.push(message.clone());
    }
    listed.extend(earlier_issues(pending, step.id));
    // a clean run with nothing earlier to show reports its own message and clears the condition
    let holds = !listed.is_empty();
    if holds {
        let more = listed.len().saturating_sub(ISSUE_STEPS);
        listed.truncate(ISSUE_STEPS);
        if more > 0 {
            listed.push(format!("and {more} more steps with issues"));
        }
        message = listed.join(" | ");
    }
    crds::set_upgrade_condition(
        &meta.client,
        &meta.status,
        UpgradeConditionType::MigrationIssues,
        holds,
        message,
    )
    .await;
}

/// List the accepted codes that match no issue group
///
/// # Arguments
///
/// * `accepted` - The codes the run was given
/// * `groups` - The issue groups the run left
fn unmatched(accepted: &[String], groups: &[(String, String, u64)]) -> Vec<String> {
    // a code with no group of its own
    accepted
        .iter()
        .filter(|code| groups.iter().all(|group| &group.0 != *code))
        .cloned()
        .collect()
}

/// Handle a finished migration Job: record the step and report its issues
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade being run
/// * `index` - The index of the planned step
/// * `step` - The step
/// * `migration` - The migration it ran
/// * `job` - The finished Job
async fn succeeded(
    meta: &ClusterMeta,
    pending: &Pending,
    index: usize,
    step: &'static StepDef,
    migration: &str,
    job: &Job,
) -> Result<Advance, Error> {
    // read the summary the run left, which holds its counts and groups
    let summary = match jobs::state(meta, job).await? {
        JobState::Succeeded(summary) => summary,
        _ => Value::Null,
    };
    let counts = summary
        .get("counts")
        .and_then(|counts| serde_json::from_value::<PassCounts>(counts.clone()).ok());
    let groups = summary_groups(&summary);
    let given = annotation(job, ACCEPTED_ANNOTATION)
        .unwrap_or_default()
        .split(',')
        .filter(|code| !code.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let ignored = unmatched(&given, &groups);
    // a run that resumed an earlier pod's counts only its own work, so one that finds nothing left
    // finished a step the earlier pod did the work of
    let resumed = summary
        .get("resumed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // a first pass that found nothing to change is a skipped step
    let nothing = !resumed
        && counts.is_some_and(|counts| {
            counts.migratable == 0 && counts.written == 0 && counts.flagged == 0
        })
        && groups.is_empty();
    // report the run's issues, or clear an earlier report
    report_issues(meta, pending, step, migration, &groups, &ignored).await;
    crds::set_upgrade_failure(&meta.client, &meta.status, None).await;
    let held = groups.iter().filter(|group| group.2 > 0);
    let issues = IssueSummary {
        groups: u32::try_from(held.clone().count()).unwrap_or(u32::MAX),
        objects: held.map(|group| group.2).sum(),
        accepted: given
            .iter()
            .filter(|code| groups.iter().any(|group| &group.0 == *code))
            .cloned()
            .collect(),
    };
    let message = if nothing {
        format!("{migration} had nothing to change")
    } else {
        format!(
            "{migration} finished{}",
            if groups.is_empty() {
                String::new()
            } else {
                format!(" with issues: {}", groups_text(&groups))
            }
        )
    };
    Ok(Advance::Finished(Box::new(AppliedStep {
        revision: step_revision(pending, index),
        step: step.id.to_owned(),
        outcome: if nothing {
            StepState::Skipped
        } else if thorium::models::schema::reingests(migration) {
            // a derived rebuild only starts its reingest, which the search-streamer finishes after
            // the rollout
            StepState::Reingesting
        } else {
            StepState::Done
        },
        message: Some(message),
        at: Some(now().to_rfc3339()),
        by: Some(format!("thorium-operator {OPERATOR_VERSION}")),
        backup: annotation(job, BACKUP_ANNOTATION)
            .filter(|backup| !backup.is_empty())
            .map(ToOwned::to_owned),
        confirm: annotation(job, CONFIRM_ANNOTATION)
            .filter(|code| !code.is_empty())
            .map(ToOwned::to_owned),
        loss: open_run(&pending.stored.state, step.id).and_then(|entry| entry.loss.clone()),
        work: counts.map(|counts| counts.work_text()),
        job: Some(name(job)),
        chain: vec![step.id.to_owned()],
        issues: (issues.groups > 0 || !issues.accepted.is_empty()).then_some(issues),
        ..AppliedStep::default()
    })))
}

/// Record in the history that a step's Job failed, once for each run of the Job
///
/// The entry keeps the Job's and pod's names and no log lines.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade being run, whose history records the failure
/// * `index` - The index of the planned step
/// * `step` - The step
/// * `job` - The failed Job
/// * `reason` - Why the Job failed
/// * `pod` - The pod whose log was read, if one existed
async fn record_failed(
    meta: &ClusterMeta,
    pending: &mut Pending,
    index: usize,
    step: &'static StepDef,
    job: &Job,
    reason: &str,
    pod: Option<String>,
) -> Result<(), Error> {
    // a Job whose failure is already recorded since its run started adds nothing, while an
    // earlier run's failure under the same name doesn't count
    let job_name = name(job);
    if failure_since_start(&pending.stored.state, step.id, &job_name).is_some() {
        return Ok(());
    }
    // record the failure with the approval's backup and the code the run consumed
    let entry = AppliedStep {
        revision: step_revision(pending, index),
        step: step.id.to_owned(),
        outcome: StepState::Failed,
        message: Some(format!("The Job {job_name} failed ({reason})")),
        at: Some(now().to_rfc3339()),
        by: Some(format!("thorium-operator {OPERATOR_VERSION}")),
        backup: annotation(job, BACKUP_ANNOTATION)
            .filter(|backup| !backup.is_empty())
            .map(ToOwned::to_owned),
        confirm: annotation(job, CONFIRM_ANNOTATION)
            .filter(|code| !code.is_empty())
            .map(ToOwned::to_owned),
        // the codes the run was given, so the failure holds the step after its Job is deleted
        issues: annotation(job, ACCEPTED_ANNOTATION)
            .filter(|csv| !csv.is_empty())
            .map(|csv| IssueSummary {
                groups: 0,
                objects: 0,
                accepted: csv.split(',').map(ToOwned::to_owned).collect(),
            }),
        job: Some(job_name.clone()),
        failure: Some(JobFailure {
            job: job_name,
            pod,
            log_tail: Vec::new(),
        }),
        ..AppliedStep::default()
    };
    pending.stored.state.record(entry);
    state::save(&meta.client, &meta.namespace, &mut pending.stored).await
}

/// A failed migration Job as the step holds it
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Failure {
    /// The held step, failed, with what the admin should do
    pub(super) advance: Advance,
    /// Whether the Job refused to run because its confirmation code was already consumed
    pub(super) consumed: bool,
}

/// Hold a step whose migration Job failed: report the failure and record it once
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade being run, whose history records the failure
/// * `index` - The index of the planned step
/// * `step` - The step
/// * `job` - The failed Job
/// * `accept` - The issue codes the run was given
/// * `lossy` - Whether the step loses data, whose retry needs a new confirmation code
pub(super) async fn failed(
    meta: &ClusterMeta,
    pending: &mut Pending,
    index: usize,
    step: &'static StepDef,
    job: &Job,
    accept: &[String],
    lossy: bool,
) -> Result<Failure, Error> {
    // read why it failed: the summary's groups for a run that failed on its issues, the reason and
    // the end of the log otherwise
    let JobState::Failed {
        reason,
        summary,
        pod,
        log_tail,
    } = jobs::state(meta, job).await?
    else {
        return Ok(Failure {
            advance: Advance::Held {
                state: StepState::Failed,
                message: format!("The Job {} failed", name(job)),
            },
            consumed: false,
        });
    };
    // a run that found its confirmation code consumed says so in its summary
    let consumed = summary
        .as_ref()
        .and_then(|summary| summary.get("error"))
        .and_then(Value::as_str)
        .is_some_and(|error| error.contains(CONSUMED_REFUSAL));
    let job_name = name(job);
    let groups = summary.as_ref().map_or_else(Vec::new, summary_groups);
    let outcome = summary
        .as_ref()
        .and_then(|summary| summary.get("outcome"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let ignored = unmatched(accept, &groups);
    // the status shows the failed Job and the end of its log
    let tail = log_tail
        .iter()
        .rev()
        .take(usize::try_from(jobs::LOG_TAIL_LINES).unwrap_or(20))
        .rev()
        .cloned()
        .collect::<Vec<_>>();
    crds::set_upgrade_failure(
        &meta.client,
        &meta.status,
        Some(JobFailure {
            job: job_name.clone(),
            pod: pod.clone(),
            log_tail: tail,
        }),
    )
    .await;
    // the history keeps the Job and pod names, never log lines
    record_failed(meta, pending, index, step, job, &reason, pod).await?;
    // say what happened and what retries it
    let what = if groups.is_empty() {
        format!("{outcome} {reason}").trim().to_owned()
    } else {
        format!(
            "a run over its issue limit; largest groups: {}",
            groups_text(&groups)
        )
    };
    let retry = if lossy {
        NOT_RETRIED_BY_ITSELF
    } else {
        NOT_RETRIED
    };
    let mut message = format!(
        "The Job {job_name} failed ({what}); see status.upgrade.failure for its logs. {retry}"
    );
    if !ignored.is_empty() {
        append(
            &mut message,
            format_args!(
                "; accepted codes that match no group of this run: {}",
                ignored.join(", ")
            ),
        );
    }
    // a run by hand may finish the step, which the record shows, so it is read again soon
    if record::read_again_within(&mut pending.stored.state, FAILED_READ_SECS) {
        state::save(&meta.client, &meta.namespace, &mut pending.stored).await?;
    }
    Ok(Failure {
        advance: Advance::Held {
            state: StepState::Failed,
            message,
        },
        consumed,
    })
}

/// Get the catalog identity the last status Job reported, from the state's marker
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
fn reported_catalog(state: &UpgradeState) -> Option<CatalogIdentity> {
    // a marker that is missing or doesn't parse says nothing
    state
        .schema
        .get(CATALOG_MARKER)
        .and_then(|raw| serde_json::from_str(raw).ok())
}

/// Find how the image's thoradm catalog differs from the operator's, when it does
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
pub(super) fn skew(state: &UpgradeState) -> Option<String> {
    // compare the revision and hash the status Job reported with this operator's
    let theirs = reported_catalog(state)?;
    let ours = catalog_identity(&catalog());
    (theirs != ours).then(|| {
        format!(
            "The image's thoradm knows catalog {} (hash {}), but this operator plans with catalog \
             {} (hash {}), so no data-changing step is planned until they agree: deploy the same \
             Thorium release for the operator and the image",
            theirs.latest,
            theirs.hash.as_str(),
            ours.latest,
            ours.hash.as_str()
        )
    })
}

/// Raise or clear the `CatalogSkew` condition from the last status Job's report
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `state` - The recorded upgrade state
pub(super) async fn report_skew(meta: &ClusterMeta, state: &UpgradeState) {
    // name both catalogs while they differ
    let message = skew(state);
    let holds = message.is_some();
    crds::set_upgrade_condition(
        &meta.client,
        &meta.status,
        UpgradeConditionType::CatalogSkew,
        holds,
        message.unwrap_or_else(|| {
            "The image's thoradm and the operator know the same catalog".to_owned()
        }),
    )
    .await;
}

/// Hold a step while a migration Job runs, or cancel it when its approval is gone
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade being run
/// * `job` - The active Job
/// * `approved` - Whether the step is approved now: it has an approval, and a lossy step's approval
///   still gets past the gate
async fn active(
    meta: &ClusterMeta,
    pending: &mut Pending,
    job: &Job,
    approved: bool,
) -> Result<Advance, Error> {
    // a Job runs while its step is approved and the upgrade still moves to the target it ran for
    let job_name = name(job);
    let same_target = annotation(job, TARGET_ANNOTATION) == Some(pending.target.as_str());
    // a Job a hold stopped is still being deleted, so the step waits for it before it resumes
    if approved && same_target && job.metadata.deletion_timestamp.is_some() {
        pending.requeue_hint = Some(RUNNING_REQUEUE_SECS);
        return Ok(Advance::Held {
            state: StepState::Running,
            message: format!(
                "Waiting for the stopped migration Job {job_name} to stop and write its \
                 checkpoint before the step resumes"
            ),
        });
    }
    if approved && same_target {
        pending.requeue_hint = Some(RUNNING_REQUEUE_SECS);
        // the note a lost pause count left on the step's start stays in its message
        let step = job
            .metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get(jobs::STEP_LABEL))
            .cloned()
            .unwrap_or_default();
        let note = open_run(&pending.stored.state, &step)
            .and_then(|entry| entry.message.as_deref())
            .and_then(|message| {
                message
                    .split_once(". ")
                    .map(|(_, note)| format!(". {note}"))
            })
            .filter(|note| note.contains(LOST_COUNTS))
            .unwrap_or_default();
        return Ok(Advance::Held {
            state: StepState::Running,
            message: format!("The migration Job {job_name} is running{note}"),
        });
    }
    // otherwise delete it in the foreground, so it stays until its pod has checkpointed and gone
    if job.metadata.deletion_timestamp.is_none() {
        jobs::delete(meta, &job_name).await?;
    }
    pending.requeue_hint = Some(RUNNING_REQUEUE_SECS);
    let why = if same_target {
        "its approval was removed"
    } else {
        "the target revision changed"
    };
    Ok(Advance::Held {
        state: StepState::Running,
        message: format!(
            "Waiting for the migration Job {job_name} to stop and write its checkpoint: {why}"
        ),
    })
}

/// Whether a step reverses a step whose revision wrote both forms of its data
///
/// The forward step's migration backfilled the new form while the application wrote both, so every
/// object already has a valid old form and reversing it needs no data pass.
///
/// # Arguments
///
/// * `step` - The step
fn reverses_dual_write(step: &StepDef) -> bool {
    // look the forward step up and see how it declared its reverse
    step.reverses
        .and_then(|forward| catalog().step(forward))
        .is_some_and(|(_, forward)| {
            matches!(forward.reversibility, Some(Reversibility::DualWrite { .. }))
        })
}

/// Advance a step that runs as a migration one move: check it, wait for its approval, pause its
/// components, and run and follow its Job
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade being run
/// * `index` - The index of the planned step
/// * `step` - The step
pub(super) async fn advance(
    meta: &ClusterMeta,
    pending: &mut Pending,
    index: usize,
    step: &'static StepDef,
) -> Result<Advance, Error> {
    // a step that hasn't started is checked first
    advance_with(meta, pending, index, step, true).await
}

/// Advance a step that runs as a migration one move, running it again without a check when asked
///
/// A step run again was found at fault by the operator's own read of the backend, so it needs no
/// check Job to say what it would change: it runs under the approval it already has.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade being run
/// * `index` - The index of the planned step
/// * `step` - The step
/// * `check` - Whether a step that hasn't started is checked first
// the step's holds are tried in one order, each in a few lines, which reads best in one place
#[allow(clippy::too_many_lines)]
pub(super) async fn advance_with(
    meta: &ClusterMeta,
    pending: &mut Pending,
    index: usize,
    step: &'static StepDef,
    check: bool,
) -> Result<Advance, Error> {
    // reversing a dual write runs no data pass: every object has its old form, so the revision is
    // all that changes
    if reverses_dual_write(step) {
        return Ok(Advance::Skipped(
            "the revision wrote both forms of the data, so there is nothing to reverse".to_owned(),
        ));
    }
    // the image's thoradm must plan the same catalog before any Job is created for a step
    let migration = step.thoradm_migration.unwrap_or(step.id);
    if let Some(message) = skew(&pending.stored.state) {
        return Ok(Advance::Held {
            state: StepState::Blocked,
            message,
        });
    }
    let approvals = meta.cluster.spec.upgrade.approvals.clone();
    let approval = usable_now(&pending.stored.state, step, &approvals).cloned();
    // a lossy step is gated by its loss report and its confirmation code instead
    let lossy = pending.steps[index].loss.is_some();
    // a Job that runs holds the step, or is cancelled when its approval is gone
    let found = jobs::find(meta, JobKind::Migration, step.id).await?;
    if let Some(job) = found
        .iter()
        .find(|job| jobs::phase(job) == JobPhase::Active)
    {
        let approved = if lossy {
            lossy::approved(meta, step, &approvals).await?
        } else {
            approval.is_some()
        };
        return active(meta, pending, job, approved).await;
    }
    // the Job of the run that started and hasn't ended
    let open = open_run(&pending.stored.state, step.id).cloned();
    let named = open.as_ref().and_then(|entry| entry.job.clone());
    let current = named
        .as_deref()
        .and_then(|named| found.iter().find(|job| name(job) == named));
    // a finished Job ends the step, whatever became of its approval since
    if let Some(job) = current
        && jobs::phase(job) == JobPhase::Complete
    {
        return succeeded(meta, pending, index, step, migration, job).await;
    }
    if lossy {
        return lossy::advance(
            meta,
            pending,
            index,
            step,
            migration,
            found.len(),
            current,
            &approvals,
        )
        .await;
    }
    // without an approval the step waits for one, with what its check found
    let Some(approval) = approval else {
        return unapproved(meta, pending, index, step, migration, open.is_some()).await;
    };
    let accept = effective_accepted(&pending.stored.state, step.id, &approval);
    // a failed Job holds the step until its approval changes
    if let Some(job) = current
        && matches!(jobs::phase(job), JobPhase::Failed { .. })
        && annotation(job, APPROVAL_ANNOTATION)
            == Some(approval_hash(&approval.backup, &accept, None).as_str())
    {
        return Ok(failed(meta, pending, index, step, job, &accept, false)
            .await?
            .advance);
    }
    // a failed run holds the step once its Job is deleted too, as its TTL does
    if let Some(held) = gone_failure(
        pending,
        step,
        current,
        open.as_ref(),
        &approval.backup,
        &accept,
        NOT_RETRIED,
    ) {
        return Ok(held);
    }
    // a step that hasn't started is checked first, and skipped when there is nothing to change
    if open.is_none() && check {
        let report = match ensure_check(meta, pending, step, migration, true).await? {
            Check::Ready(report) => report,
            Check::Wait(message) => {
                return Ok(Advance::Held {
                    state: StepState::Waiting,
                    message,
                });
            }
            Check::Failed(message) => {
                return Ok(Advance::Held {
                    state: StepState::Blocked,
                    message,
                });
            }
        };
        if let Some(wait) = &report.wait {
            return Ok(Advance::Held {
                state: StepState::Waiting,
                message: wait.reason.clone(),
            });
        }
        if let Work::Nothing(reason) = work(step, &report) {
            return Ok(Advance::Skipped(reason));
        }
    }
    let approved = Approved {
        backup: &approval.backup,
        accept: &accept,
        confirm: None,
        loss: None,
    };
    start(meta, pending, index, step, migration, approved, found.len()).await
}

/// Hold a step that has no approval: say what its check found and how to approve it
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade being run
/// * `index` - The index of the planned step
/// * `step` - The step
/// * `migration` - The migration it runs
/// * `started` - Whether the step already started, so its components are paused
async fn unapproved(
    meta: &ClusterMeta,
    pending: &mut Pending,
    index: usize,
    step: &'static StepDef,
    migration: &str,
    started: bool,
) -> Result<Advance, Error> {
    // a step that started and lost its approval says so
    let approvals = meta.cluster.spec.upgrade.approvals.clone();
    if started {
        let found = format!(
            "Step {} started and its approval is gone, so its Job was stopped; its components stay \
             paused.",
            step.id
        );
        return Ok(Advance::Held {
            state: StepState::Blocked,
            message: approval_message(step, &found, &approvals),
        });
    }
    // the check says what the step would change, and whether it needs an approval at all
    let report = match ensure_check(meta, pending, step, migration, false).await? {
        Check::Ready(report) => report,
        Check::Wait(message) => {
            return Ok(Advance::Held {
                state: StepState::Waiting,
                message,
            });
        }
        Check::Failed(message) => {
            return Ok(Advance::Held {
                state: StepState::Blocked,
                message,
            });
        }
    };
    if let Some(wait) = &report.wait {
        return Ok(Advance::Held {
            state: StepState::Waiting,
            message: wait.reason.clone(),
        });
    }
    match work(step, &report) {
        Work::Nothing(reason) => Ok(Advance::Skipped(reason)),
        Work::Needed(found) => {
            show(pending, index, step, &report);
            pending.steps[index].blocked = Some(BlockReason::Approval);
            Ok(Advance::Held {
                state: StepState::Blocked,
                message: approval_message(step, &found, &approvals),
            })
        }
    }
}

/// Start an approved step: pause its components, record its start, and create its Job
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The upgrade being run
/// * `index` - The index of the planned step
/// * `step` - The step
/// * `migration` - The migration it runs
/// * `approved` - What the run is given: the approval's backup and codes, and a lossy step's code
/// * `attempt` - The attempt counter
pub(super) async fn start(
    meta: &ClusterMeta,
    pending: &mut Pending,
    index: usize,
    step: &'static StepDef,
    migration: &str,
    approved: Approved<'_>,
    attempt: usize,
) -> Result<Advance, Error> {
    // the counts of the components to pause, read before anything is scaled down
    let attempt = u32::try_from(attempt).unwrap_or(u32::MAX);
    let counts = pause::read_counts(meta, &pending.stored.state, step.pause).await?;
    let job = run_name(
        pending,
        step,
        approved.backup.trim(),
        &format!(
            "{}|{}",
            approved.accept.join(","),
            approved.confirm.unwrap_or_default()
        ),
        &attempt.to_string(),
    );
    // record the start with the counts before scaling anything, so they outlive a lost state
    let entry = running_entry(pending, index, step, approved, &job, &counts);
    let open = open_run(&pending.stored.state, step.id);
    if open.is_none_or(|open| open.job.as_deref() != Some(job.as_str())) {
        pending.stored.state.record(entry.clone());
        state::save(&meta.client, &meta.namespace, &mut pending.stored).await?;
    }
    // the step is no longer waiting on anything an admin can answer
    pending.steps[index].blocked = None;
    // scale the components down and wait for their pods to stop
    let names = counts.counts.keys().map(String::as_str).collect::<Vec<_>>();
    pause::scale_down(meta, &names).await?;
    if let Some(message) = pause::drained(meta, &names).await? {
        pending.requeue_hint = Some(RUNNING_REQUEUE_SECS);
        return Ok(Advance::Held {
            state: StepState::Waiting,
            message,
        });
    }
    // one writing Job runs in a namespace at a time, counting pods that haven't terminated
    let writers = jobs::active_writers(meta).await?;
    if let Some(writer) = writers.first() {
        pending.requeue_hint = Some(RUNNING_REQUEUE_SECS);
        return Ok(Advance::Held {
            state: StepState::Waiting,
            message: format!(
                "Waiting for the writing Job {writer} to end before step {} starts its Job",
                step.id
            ),
        });
    }
    // create the Job the step runs in
    let spec = run_spec(meta, pending, step, migration, approved, attempt, &entry)?;
    jobs::create(&spec, meta).await?;
    crds::set_upgrade_failure(&meta.client, &meta.status, None).await;
    pending.requeue_hint = Some(RUNNING_REQUEUE_SECS);
    Ok(Advance::Held {
        state: StepState::Running,
        message: format!(
            "Created the migration Job {} for step {}{}",
            spec.name,
            step.id,
            lost_note(&counts.from_spec)
        ),
    })
}
