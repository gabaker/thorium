//! The arguments for the data migrations behind Thorium's upgrade revisions

use clap::Parser;
use std::path::PathBuf;

/// The data migration subcommands
#[derive(Parser, Debug, Clone)]
pub enum MigrateSubCommands {
    /// List every revision and its upgrade steps, with the migrations thoradm runs
    #[clap(version, author)]
    List,
    /// Show the upgrade steps that bring a cluster from one revision to another
    #[clap(version, author)]
    Plan(PlanMigrations),
    /// Run a data migration
    #[clap(version, author)]
    Run(RunMigration),
    /// Classify what a migration would touch, writing nothing
    #[clap(version, author)]
    Check(CheckMigration),
    /// Record the deployment's new revision once every step of the path is recorded
    #[clap(version, author)]
    Record(RecordRevision),
    /// Check whether a data migration still needs to run
    #[clap(version, author)]
    Verify(MigrationTarget),
    /// List the issue groups a migration recorded, or one group's sample keys
    #[clap(version, author)]
    Issues(ListIssues),
    /// Print what a migration's last pass applied
    #[clap(version, author)]
    Report(ReportMigration),
    /// Move a cluster back to an older revision through revert or reverse edges
    #[clap(version, author)]
    Revert(RevertMigrations),
    /// Rewrite the recorded revision after restoring or wiping data, with an audit entry
    #[clap(version, author)]
    ResetState(ResetState),
}

/// Show the upgrade steps between two revisions
#[derive(Parser, Debug, Clone)]
pub struct PlanMigrations {
    /// The revision the cluster is at (e.g. 2026-10-v01)
    #[clap(long)]
    pub from: String,
    /// The revision to upgrade to (defaults to the latest revision)
    #[clap(long)]
    pub to: Option<String>,
}

/// The data migration to run or verify, named by `--id` or by a positional id
#[derive(Parser, Debug, Clone)]
pub struct MigrationTarget {
    /// The id of the migration (see `thoradm migrate list`)
    #[clap(
        long = "id",
        value_name = "ID",
        required_unless_present = "positional_id",
        conflicts_with = "positional_id"
    )]
    pub id: Option<String>,
    /// The id of the migration given without `--id`, as scripts written before `--id` pass it
    #[clap(value_name = "ID", hide = true)]
    pub positional_id: Option<String>,
}

impl MigrationTarget {
    /// Get the id of the migration from whichever of `--id` or the positional id was given
    #[must_use]
    pub fn id(&self) -> &str {
        // clap requires exactly one of the two forms, so at most one of them is set
        self.id
            .as_deref()
            .or(self.positional_id.as_deref())
            .unwrap_or_default()
    }
}

/// The data migration to run
#[derive(Parser, Debug, Clone)]
pub struct RunMigration {
    /// The migration to run
    #[clap(flatten)]
    pub target: MigrationTarget,
    /// Only process these object keys, such as objects an admin fixed after an issue
    #[clap(long, value_delimiter = ',')]
    pub objects: Vec<String>,
    /// Re-run one issue group: scan the family, write the objects that now convert, and recount
    /// every group seen
    #[clap(long, conflicts_with = "objects")]
    pub reason: Option<String>,
    /// Reason codes whose groups don't fail the run, passed by the operator from the approval's
    /// `accept_issues`
    #[clap(long, value_delimiter = ',')]
    pub accept_issues: Vec<String>,
    /// The step's `Running` history entry as JSON, with its paused replica counts, written before
    /// anything else (passed by the operator)
    #[clap(long)]
    pub running_entry: Option<String>,
    /// The lossy step's confirmation code, written as a consumed code before the run; the run is
    /// refused when that code is already consumed
    #[clap(long)]
    pub confirm: Option<String>,
    /// Take the namespace's writer lease even though another writer holds it; the holder's
    /// workers stop at their next batch
    #[clap(long)]
    pub force_break_lease: bool,
    /// The approval's backup reference, recorded in the pass row (passed by the operator)
    #[clap(long)]
    pub backup: Option<String>,
    /// How many slices to split a new pass into, one worker each
    #[clap(long, default_value_t = 16)]
    pub slices: u32,
    /// The most rows one batch reads before the worker checkpoints
    #[clap(long, default_value_t = 10_000)]
    pub batch_rows: usize,
    /// The least time between two checkpoints of one worker, in milliseconds
    #[clap(long, default_value_t = 1000, hide = true)]
    pub checkpoint_ms: u64,
    /// How many times a timed out read or write is retried before the run stops resumable
    #[clap(long, default_value_t = 12, hide = true)]
    pub retries: u32,
    /// The first pause before a retry, in milliseconds
    #[clap(long, default_value_t = 250, hide = true)]
    pub backoff_ms: u64,
}

/// The migration whose last pass report to print
#[derive(Parser, Debug, Clone)]
pub struct ReportMigration {
    /// The id of the migration (see `thoradm migrate list`)
    #[clap(long)]
    pub id: String,
}

/// The data migration to check
#[derive(Parser, Debug, Clone)]
pub struct CheckMigration {
    /// The id of the migration (see `thoradm migrate list`)
    #[clap(long)]
    pub id: String,
    /// Print every issue group, not only the ten largest (stdout only, never a Job result)
    #[clap(long)]
    pub all: bool,
    /// Print up to 20 sample keys per group (stdout only, never a Job result)
    #[clap(long)]
    pub samples: bool,
    /// How many slices to split the check into, one worker each
    #[clap(long, default_value_t = 4)]
    pub slices: u32,
    /// The most rows one batch reads
    #[clap(long, default_value_t = 10_000)]
    pub batch_rows: usize,
}

/// The issue groups to list
#[derive(Parser, Debug, Clone)]
pub struct ListIssues {
    /// The id of the migration (see `thoradm migrate list`)
    #[clap(long)]
    pub id: String,
    /// Show one group: its code, class, count, resolved count, whether it was accepted, and its
    /// sample keys from every worker, sorted
    #[clap(long)]
    pub reason: Option<String>,
}

/// The revision to revert to
#[derive(Parser, Debug, Clone)]
pub struct RevertMigrations {
    /// The older revision to move the cluster to (e.g. 2026-10-v02)
    #[clap(long)]
    pub to: String,
    /// Print the steps the revert runs, what each loses, and the confirmation code of each lossy
    /// one, and change nothing
    #[clap(long)]
    pub dry_run: bool,
    /// Where the backup taken before the revert is, recorded with every step that changes data
    /// (required to run one)
    #[clap(long)]
    pub backup: Option<String>,
    /// The confirmation code of the lossy step the revert stops at, repeated to accept its loss;
    /// each code is for one block of one step, so a revert with several lossy steps is run once
    /// per step
    #[clap(long)]
    pub confirm: Option<String>,
    /// Let a lossy step run although a field it loses isn't covered by `thoradm backup`, so a
    /// backup can't restore it; the plan shows each lost field's coverage
    #[clap(long)]
    pub accept_backup_gaps: bool,
}

/// The revision to record at the end of a plan
#[derive(Parser, Debug, Clone)]
pub struct RecordRevision {
    /// The revision the plan reached
    #[clap(long)]
    pub revision: String,
    /// The mounted state `ConfigMap`; only operator-recorded entry kinds Scylla lacks are copied,
    /// never a `Done` entry for a gated step
    #[clap(long)]
    pub state: Option<PathBuf>,
    /// Seed the revision and the full history from `state` when `schema_migrations` holds no
    /// revision, creating the table if it is missing; refused once a revision is recorded
    #[clap(long)]
    pub seed: bool,
}

/// The revision to record after a data restore; refused without `--i-restored-data`, and recorded
/// with an audit entry in the history
#[derive(Parser, Debug, Clone)]
pub struct ResetState {
    /// The revision the restored data is at
    #[clap(long)]
    pub to: String,
    /// Confirm the data was restored or wiped, so the recorded revision is wrong
    #[clap(long)]
    pub i_restored_data: bool,
}
