//! The kinds of migration-state rows, migration pass checkpoints and counts, and the reports a
//! migration check and the status Job produce

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use super::{Fingerprint, RecordStatus, ScyllaStoreAsStr};
use crate::models::InvalidEnum;
use crate::models::upgrades::{IssueReason, RevisionId, StepLoss};

/// The kind of a `schema_migrations` row, its partition key
///
/// Stored as text through `ScyllaStoreAsStr`, which calls the hand-written `as_str` and `FromStr`
/// below, not serde; the stored text is `backend`, `run`, `revision`, `history`, `config`,
/// `consumed_code`, `archived_history`, `job_result`, or `legacy_drop`
///
/// It has no `Unknown` catch-all: a reader that queries one kind's partition by name never sees a
/// kind added later, but a reader that scans the whole table, as a backup of `schema_migrations`
/// does, must tolerate kinds it doesn't know
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ScyllaStoreAsStr)]
pub enum MigrationRecordKind {
    /// The applied fingerprint and layout versions of one backend, or of the deployment; stored as
    /// `backend`
    Backend,
    /// The state of one migration pass (its pass row); stored as `run`
    Run,
    /// The revision the deployment is at; stored as `revision`
    Revision,
    /// One entry of the applied history, append-only and uncapped; stored as `history`
    History,
    /// One data-shaping setting recorded at the first apply; stored as `config`
    Config,
    /// A lossy approval's confirmation code, written when its step runs and never deleted, so it
    /// never matches again; stored as `consumed_code`
    ConsumedCode,
    /// A history entry a restore replaced, kept beside the restored history; stored as
    /// `archived_history`
    ArchivedHistory,
    /// One Job's full result, read by the status Job for the operator; stored as `job_result`
    JobResult,
    /// The audit row of one legacy object `thoradm schema drop-legacy` dropped: who, when, the
    /// object's id and its row count; append-only; stored as `legacy_drop`
    LegacyDrop,
}

impl MigrationRecordKind {
    /// Every record kind, in the order they are declared
    pub const ALL: [MigrationRecordKind; 9] = [
        Self::Backend,
        Self::Run,
        Self::Revision,
        Self::History,
        Self::Config,
        Self::ConsumedCode,
        Self::ArchivedHistory,
        Self::JobResult,
        Self::LegacyDrop,
    ];

    /// The text this kind is stored as in Scylla, which never changes once released
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        // map each kind to its stored text
        match self {
            Self::Backend => "backend",
            Self::Run => "run",
            Self::Revision => "revision",
            Self::History => "history",
            Self::Config => "config",
            Self::ConsumedCode => "consumed_code",
            Self::ArchivedHistory => "archived_history",
            Self::JobResult => "job_result",
            Self::LegacyDrop => "legacy_drop",
        }
    }
}

impl FromStr for MigrationRecordKind {
    /// Stored text that names no record kind fails with an [`InvalidEnum`]
    type Err = InvalidEnum;

    /// Parse a record kind from its stored text
    ///
    /// # Arguments
    ///
    /// * `raw` - The stored text to parse
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        // find the kind stored this way, rejecting anything else
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == raw)
            .ok_or_else(|| InvalidEnum(raw.to_owned()))
    }
}

/// The status of a migration pass
///
/// Stored as text through `ScyllaStoreAsStr` (hand-written `as_str` and `FromStr`, not serde), so
/// the stored text is `snake_case` (`running`, `done`, `done_with_issues`, `failed`), while JSON
/// (serde) uses the variant names (`Running`, `Done`, `DoneWithIssues`, `Failed`)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ScyllaStoreAsStr)]
pub enum MigrationRunStatus {
    /// The pass is running, or stopped and resumable from its checkpoint; stored as `running`
    Running,
    /// The pass finished with no issues; stored as `done`
    Done,
    /// The pass finished with every issue group within the step's `group_max` or accepted; the
    /// layout advanced; stored as `done_with_issues`
    DoneWithIssues,
    /// An issue group went over `group_max` without being accepted, or the retries ran out; the
    /// layout didn't advance; stored as `failed`
    Failed,
}

impl MigrationRunStatus {
    /// Every run status, in the order they are declared
    pub const ALL: [MigrationRunStatus; 4] = [
        Self::Running,
        Self::Done,
        Self::DoneWithIssues,
        Self::Failed,
    ];

    /// The text this status is stored as in Scylla, which never changes once released
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        // map each status to its stored text
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::DoneWithIssues => "done_with_issues",
            Self::Failed => "failed",
        }
    }
}

impl FromStr for MigrationRunStatus {
    /// Stored text that names no run status fails with an [`InvalidEnum`]
    type Err = InvalidEnum;

    /// Parse a run status from its stored text
    ///
    /// # Arguments
    ///
    /// * `raw` - The stored text to parse
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        // find the status stored this way, rejecting anything else
        Self::ALL
            .into_iter()
            .find(|status| status.as_str() == raw)
            .ok_or_else(|| InvalidEnum(raw.to_owned()))
    }
}

/// How a migration classified one object, or how handling it ended
///
/// Stored as text through `ScyllaStoreAsStr` (hand-written `as_str` and `FromStr`, not serde), so
/// the stored text is `snake_case` (`current`, `migratable`, `unrecognized`, `error`), while JSON
/// (serde) uses the variant names (`Current`, `Migratable`, `Unrecognized`, `Error`); the issue
/// table's `class` column holds `unrecognized` or `error`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ScyllaStoreAsStr)]
pub enum ObjectClass {
    /// Already in the target layout; skipped; stored as `current`
    Current,
    /// In a source layout the chain converts; converted and written; stored as `migratable`
    Migratable,
    /// No layout parses; left untouched and recorded as an issue; stored as `unrecognized`
    Unrecognized,
    /// Recognized, but the write still failed after retries; recorded as an issue; stored as
    /// `error`
    Error,
}

impl ObjectClass {
    /// The text this class is stored as in Scylla, which never changes once released
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        // map each class to its stored text
        match self {
            Self::Current => "current",
            Self::Migratable => "migratable",
            Self::Unrecognized => "unrecognized",
            Self::Error => "error",
        }
    }
}

impl FromStr for ObjectClass {
    /// Stored text that names no object class fails with an [`InvalidEnum`]
    type Err = InvalidEnum;

    /// Parse a class from its stored text
    ///
    /// # Arguments
    ///
    /// * `raw` - The stored text to parse
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        // map each stored text back to its class, rejecting anything else
        match raw {
            "current" => Ok(Self::Current),
            "migratable" => Ok(Self::Migratable),
            "unrecognized" => Ok(Self::Unrecognized),
            "error" => Ok(Self::Error),
            _ => Err(InvalidEnum(raw.to_owned())),
        }
    }
}

/// Where one worker of a migration pass is, written in its worker row after each batch that had
/// rows
///
/// Every field defaults, so a checkpoint written before a field was added still resumes
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PassCheckpoint {
    /// The step ids of the transforms the pass runs, in order; a checkpoint for a different chain
    /// restarts the pass from the beginning
    pub chain: Vec<String>,
    /// How many slices the pass was split into when it first started; a checkpoint for a
    /// different slice count restarts the pass
    pub slices: u32,
    /// The last position done in this worker's slice, in a form the migration's runner defines
    /// (the token of the last finished partition, a key, a `SCAN` cursor, a bucket prefix)
    pub position: serde_json::Value,
    /// Whether the worker reached the end of its slice
    pub done: bool,
    /// This worker's counts so far
    pub counts: PassCounts,
    /// What this worker has applied so far in each group of the pass's report, which the pass's
    /// end sums into the applied report
    pub report_counts: BTreeMap<String, ReportCounts>,
}

/// What one pass did, kept per pass (a fused pass counts once for its whole chain)
///
/// Every counter defaults to 0, so adding a counter later doesn't break resuming an older
/// checkpoint
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PassCounts {
    /// Objects read
    pub read: u64,
    /// Objects classified `current`
    pub current: u64,
    /// Objects classified `migratable`
    pub migratable: u64,
    /// Objects classified `unrecognized`
    pub unrecognized: u64,
    /// Objects whose write failed after retries
    pub error: u64,
    /// Objects written
    pub written: u64,
    /// Objects skipped (current, or outside `--objects`)
    pub skipped: u64,
    /// Objects recorded as issues
    pub flagged: u64,
}

impl PassCounts {
    /// Describe the counts as the work a step found, the text a step's history entry records as its
    /// `work`
    ///
    /// The thoradm run writing the step's entry and the operator recording it use this one text, so
    /// both entries of a step agree.
    #[must_use]
    pub fn work_text(&self) -> String {
        // the class counts of the pass, then what it wrote
        format!(
            "{} objects read: {} current, {} migratable, {} unrecognized, {} error; {} written",
            self.read, self.current, self.migratable, self.unrecognized, self.error, self.written
        )
    }
}

/// What `thoradm migrate check` found, written to the check Job's own `job_result` row (its only
/// write) and copied into the status and the approval message
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckReport {
    /// The migration checked
    pub migration: String,
    /// The objects it would touch, by class (nothing is written)
    pub counts: PassCounts,
    /// The families it covers, with their recorded and target layout versions
    #[serde(default)]
    pub families: Vec<String>,
    /// The rows (or objects) it would rewrite, by family
    #[serde(default)]
    pub rows: BTreeMap<String, u64>,
    /// How long the run would take, from the rows and a per-table rate chosen by whether the table
    /// has views, refined by rates earlier runs measured
    #[serde(default)]
    pub estimated_seconds: Option<u64>,
    /// The ten largest issue groups by count, ties broken by code; no sample keys, so the report
    /// stays small
    #[serde(default)]
    pub reasons: Vec<ReasonSummary>,
    /// How many issue groups aren't listed in `reasons`
    #[serde(default)]
    pub more: u32,
    /// Set when the check can't judge the step yet: the operator holds the step `Waiting` and runs
    /// the check Job again after `retry_after_secs`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<CheckWait>,
    /// A digest of the migration's own report, for a migration that builds one: counts per group,
    /// the skipped total, and the warning count, never the skipped list or an item's name, so the
    /// status and the approval message stay small
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<ReportSummary>,
    /// What the step would lose, when the migration performs a lossy reverse step: each lost field
    /// with how many objects hold a value to fill and whether today's `thoradm backup` covers it,
    /// which the approval message and the confirmation code's loss report are built from
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loss: Option<StepLoss>,
}

/// A digest of a migration's report, carried in its check result
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReportSummary {
    /// What was found or changed in each of the report's groups, by group name
    pub groups: BTreeMap<String, ReportCounts>,
    /// How many items were skipped in total
    pub skipped_total: u64,
    /// How many warnings the report holds for the admin to act on before approving
    pub warnings: u64,
}

/// The most bytes of JSON an applied report stores in its pass row
pub const MAX_REPORT_BYTES: usize = 256 * 1024;

/// The most skipped items an applied report lists; the rest are only counted
pub const MAX_REPORT_SKIPPED: usize = 500;

/// The most notes an applied report lists
pub const MAX_REPORT_NOTES: usize = 200;

/// The most bytes of any one string in an applied report
pub const MAX_REPORT_STRING: usize = 256;

/// What a pass applied, stored in its pass row when it ends
///
/// It holds names, codes, and counts only, never object contents or secrets, and stays within
/// [`MAX_REPORT_BYTES`] of JSON: over budget, the notes go first, then the skipped list is
/// trimmed, and the totals always stay
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppliedReport {
    /// What was changed or left alone in each group, by group name
    pub groups: BTreeMap<String, ReportCounts>,
    /// How many items were skipped in total, including those the list leaves out
    pub skipped_total: u64,
    /// The items skipped, with why, up to [`MAX_REPORT_SKIPPED`]
    pub skipped: Vec<SkippedItem>,
    /// How many notes were warnings, including those the list leaves out
    pub warnings_total: u64,
    /// What the migration noted about items, up to [`MAX_REPORT_NOTES`]
    pub notes: Vec<ReportNote>,
    /// How many mappings (an item and what it became) the migration made; counted, never listed
    pub mappings_total: u64,
}

/// One item a pass skipped
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedItem {
    /// The group the item belongs to
    pub group: String,
    /// The item's name or key
    pub item: String,
    /// Why, as a reason code (`<category>:<field>`)
    pub reason: String,
}

/// One note a migration made about an item
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportNote {
    /// The group the item belongs to
    pub group: String,
    /// The item's name or key
    pub item: String,
    /// What the migration noted, a fixed text the migration defines
    pub note: String,
    /// Whether the admin should act on it
    pub warning: bool,
}

impl AppliedReport {
    /// The report without its lists, as a check result carries it
    #[must_use]
    pub fn summary(&self) -> ReportSummary {
        // keep the counts and totals, and drop every list
        ReportSummary {
            groups: self.groups.clone(),
            skipped_total: self.skipped_total,
            warnings: self.warnings_total,
        }
    }

    /// Cut the report to its stored bounds: strings, list lengths, then the whole to fit
    /// [`MAX_REPORT_BYTES`], dropping notes first and then trimming the skipped list
    pub fn bound(&mut self) {
        // cut every string and list to its own bound
        let cut = |text: &mut String| {
            if text.len() > MAX_REPORT_STRING {
                let end = (0..=MAX_REPORT_STRING)
                    .rev()
                    .find(|at| text.is_char_boundary(*at))
                    .unwrap_or(0);
                text.truncate(end);
            }
        };
        self.skipped.truncate(MAX_REPORT_SKIPPED);
        self.notes.truncate(MAX_REPORT_NOTES);
        for item in &mut self.skipped {
            cut(&mut item.group);
            cut(&mut item.item);
            cut(&mut item.reason);
        }
        for note in &mut self.notes {
            cut(&mut note.group);
            cut(&mut note.item);
            cut(&mut note.note);
        }
        // the size of the stored form
        let size =
            |report: &AppliedReport| serde_json::to_string(report).map_or(0, |json| json.len());
        // over budget, drop the notes before touching the skipped list
        if size(self) > MAX_REPORT_BYTES {
            self.notes.clear();
        }
        // still over budget, halve the skipped list until it fits
        while size(self) > MAX_REPORT_BYTES && !self.skipped.is_empty() {
            self.skipped.truncate(self.skipped.len() / 2);
        }
    }
}

/// What a migration found or changed in one group of its report
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReportCounts {
    /// Items created or changed (or that would be, in a check)
    pub changed: u64,
    /// Items an earlier run already migrated
    pub already_migrated: u64,
    /// Items left as they are, with nothing to migrate
    pub unchanged: u64,
    /// Items skipped
    pub skipped: u64,
}

/// Why a check asks the operator to run it again later instead of skipping or blocking the step
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckWait {
    /// What the step waits for, a fixed text the migration defines (such as "workers are still
    /// registered"), never object contents, since it reaches the status
    pub reason: String,
    /// How long to wait before running the check again; the operator clamps it to between 5 and
    /// 600 seconds
    pub retry_after_secs: u64,
}

/// One issue group in a check report
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasonSummary {
    /// The reason code
    pub code: String,
    /// `Unrecognized` or `Error` (stored as `unrecognized` or `error`)
    pub class: ObjectClass,
    /// How many objects the group holds, summed across workers
    pub count: u64,
}

/// One worker's in-memory tally of one issue group, rebuilt from its issue rows when it resumes
/// and never serialized: the issue rows are the record
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupTally {
    /// The step id of the failing transform
    pub transform: String,
    /// The reason code
    pub reason: String,
    /// `Unrecognized` or `Error` (stored as `unrecognized` or `error`)
    pub class: ObjectClass,
    /// How many objects are in the group in this worker's slice
    pub count: u64,
    /// Up to 20 object keys
    pub samples: BTreeSet<String>,
    /// When this worker first recorded the group
    pub first_seen: DateTime<Utc>,
    /// When this worker last recorded the group
    pub last_seen: DateTime<Utc>,
}

/// One object a migration reports as an issue through the runner API; hand-written migrations
/// without a `TransformDef` report issues only this way
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueReport {
    /// The object's key, rendered as text
    pub key: String,
    /// The step id of the transform, or the migration id for a hand-written migration
    pub transform: String,
    /// `Unrecognized` or `Error` (stored as `unrecognized` or `error`)
    pub class: ObjectClass,
    /// Why, as a reason code
    pub reason: IssueReason,
}

/// One step the history counts as finished
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinishedStep {
    /// The revision whose edge holds the step
    pub revision: RevisionId,
    /// The step id
    pub step: String,
}

/// The one line the status Job prints, prefixed `THORIUM-RESULT ` in its pod log
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    /// What the record holds
    pub status: RecordStatus,
    /// The catalog the image's thoradm knows; the operator plans no data-changing step while it
    /// differs from its own
    pub catalog: CatalogIdentity,
    /// The full result of the writing Job asked for with `--result`, when one was
    #[serde(default)]
    pub job_result: Option<RecordStatus>,
    /// The report of the check Job asked for with `--result`, when the row holds one, since a
    /// check Job's result is a [`CheckReport`], not a record status
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<CheckReport>,
}

/// Which catalog a build knows
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogIdentity {
    /// The newest revision on its primary chain
    pub latest: RevisionId,
    /// A hash over every catalog entry and step
    pub hash: Fingerprint,
}
