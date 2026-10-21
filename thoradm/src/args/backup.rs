//! The arguments for backups

use clap::{Parser, ValueEnum};
use std::path::PathBuf;

/// The exit codes of backup, restore, verify, and scrub, printed after their help
const EXIT_CODES_HELP: &str = "Exit codes:
  0    Every object was handled or checked, and nothing was recorded
  1    The run stopped early (--abort-on-failure, a refused check, or an error; see stderr)
  2    The run finished with failures.jsonl records (failed, skipped, broken-link) or pruned.jsonl
  101  Thoradm crashed (a panic)";

/// The exit codes of `backup coverage`, printed after its help
const COVERAGE_EXIT_CODES_HELP: &str = "Exit codes:
  0    The coverage registry was printed
  1    The arguments were invalid or the output couldn't be written
  101  Thoradm crashed (a panic)";

/// The backup specific subcommands
///
/// Backup, restore, verify, and scrub exit 0 when every object was handled or checked, 2 when
/// they finished with failed or skipped objects listed in failures.jsonl, and 1 when they stopped
/// early.
#[derive(Parser, Debug, Clone)]
pub enum BackupSubCommands {
    /// Take a new backup
    #[clap(version, author, after_help = EXIT_CODES_HELP)]
    New(NewBackup),
    /// Scrub a backup for bitrot: the partition checks of `verify`, for every table
    #[clap(after_help = EXIT_CODES_HELP)]
    Scrub(ScrubBackup),
    /// Check a backup directory, or a restored cluster, against the backup's manifest
    #[clap(version, author, after_help = EXIT_CODES_HELP)]
    Verify(VerifyBackup),
    /// Restore a backup to a Thorium cluster
    #[clap(version, author, after_help = EXIT_CODES_HELP)]
    Restore(RestoreBackup),
    /// Print what a backup covers and what it excludes, from this binary, with no cluster or config
    #[clap(after_help = COVERAGE_EXIT_CODES_HELP)]
    Coverage(CoverageBackup),
}

/// Define the default backup components
fn default_backup_components() -> Vec<BackupComponents> {
    // return all by default
    vec![BackupComponents::All]
}

/// The tables that can be backed up
#[derive(
    Debug,
    Clone,
    Copy,
    strum::Display,
    strum::EnumIter,
    ValueEnum,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
)]
#[strum(serialize_all = "kebab-case")]
#[serde(rename_all = "kebab-case")]
pub enum BackupComponents {
    /// Backup all components
    All,
    /// Backup all components except those in S3
    NoS3,
    Redis,
    Associations,
    CommentAttachments,
    Comments,
    Commitish,
    CommitishList,
    Entities,
    NetworkPolicies,
    Nodes,
    Notifications,
    RepoData,
    RepoList,
    ResultFiles,
    Results,
    ResultsStream,
    S3Ids,
    S3IdsObjects,
    SamplesList,
    Tags,
    /// The user icons and entity images the backup's own records name: the `image` of each
    /// entity row and of each user in the Redis state, so it needs `entities` and `redis`
    Graphics,
}

impl BackupComponents {
    /// Returns true if the component is just a filter rather than an actual component
    pub fn is_filter(self) -> bool {
        matches!(self, BackupComponents::All | BackupComponents::NoS3)
    }

    /// Returns true if the component is stored in S3
    #[must_use]
    pub fn is_in_s3(self) -> bool {
        matches!(
            self,
            BackupComponents::CommentAttachments
                | BackupComponents::ResultFiles
                | BackupComponents::S3IdsObjects
                | BackupComponents::Graphics
        )
    }

    /// Returns the table component whose archives name an S3 component's objects, or `None` for
    /// an S3 component that doesn't read one table alone (`graphics` reads the entities archives
    /// and the Redis state, see [`BackupComponents::graphics_inputs`])
    #[must_use]
    pub fn s3_input(self) -> Option<BackupComponents> {
        // each S3 component reads the archives of one table component
        match self {
            BackupComponents::S3IdsObjects => Some(BackupComponents::S3Ids),
            BackupComponents::CommentAttachments => Some(BackupComponents::Comments),
            BackupComponents::ResultFiles => Some(BackupComponents::Results),
            _ => None,
        }
    }

    /// Returns the components whose records name the graphics, which the `graphics` component
    /// can't back up without
    #[must_use]
    pub fn graphics_inputs() -> [BackupComponents; 2] {
        // entity rows name entity images and the Redis state's users name their icons
        [BackupComponents::Entities, BackupComponents::Redis]
    }

    /// Checks that a selection of components, with `all` and `no-s3` already expanded, holds
    /// everything `graphics` needs; a selection without `graphics` always passes
    ///
    /// # Arguments
    ///
    /// * `components` - The components a backup will run
    ///
    /// # Errors
    ///
    /// Returns a message naming the components `graphics` needs that are missing.
    pub fn check_graphics_inputs(components: &[BackupComponents]) -> Result<(), String> {
        // only a backup of graphics needs its inputs
        if !components.contains(&BackupComponents::Graphics) {
            return Ok(());
        }
        // name every input the selection lacks
        let missing: Vec<String> = Self::graphics_inputs()
            .into_iter()
            .filter(|input| !components.contains(input))
            .map(|input| input.to_string())
            .collect();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "graphics needs {}: add them to the components (graphics names the keys in the \
                 entities rows and the Redis state)",
                missing.join(" and ")
            ))
        }
    }

    /// Returns the bucket roles an S3 component copies objects from
    #[must_use]
    pub fn bucket_roles(self) -> &'static [&'static str] {
        // the S3 ids name samples in the files bucket and repos in the repos bucket
        match self {
            BackupComponents::S3IdsObjects => &["files", "repos"],
            BackupComponents::CommentAttachments => &["attachments"],
            BackupComponents::ResultFiles => &["results"],
            BackupComponents::Graphics => &["graphics"],
            _ => &[],
        }
    }

    /// Determine whether the component should be backed up
    ///
    /// # Arguments
    ///
    /// * `components` - The list of components to back up supplied by the user
    pub fn should_backup(self, components: &[BackupComponents]) -> bool {
        // check if we should backup all or backup just in S3;
        // we'll check inside this function for clarity; O(n) lookups over a
        // tiny array like this shouldn't have any meaningful performance impact
        let backup_all = components.contains(&BackupComponents::All);
        let no_s3 = components.contains(&BackupComponents::NoS3);
        // determine whether we should backup this component
        match (
            backup_all,
            components.contains(&self),
            no_s3,
            self.is_in_s3(),
        ) {
            // always backup if we're set to backup all
            (true, _, _, _) |
            // backup if the component was explicitly provided
            (_, true, _, _) |
            // not explicitly provided, but backup if no_s3 and it's *not* in S3
            (_, _, true, false) => true,
            // otherwise don't backup
            _ => false,
        }
    }
}

/// Backup a Thorium cluster
// each switch is one command-line flag, so a field per flag reads as the command line does
#[allow(clippy::struct_excessive_bools)]
#[derive(Parser, Debug, Clone)]
pub struct NewBackup {
    /// The components to backup
    ///
    /// 'all' will backup all components while 'no-s3' will backup all components
    /// except for those stored in S3. If both are given, 'all' overrides 'no-s3'.
    #[clap(value_enum, default_values_t = default_backup_components(), value_delimiter = ',')]
    pub components: Vec<BackupComponents>,
    /// Where to store the backup; must be new, empty, or an interrupted backup to resume. A
    /// directory holding a finished backup is never reused, so each new backup needs its own
    #[clap(short, long, default_value = "ThoriumBackups")]
    pub output: PathBuf,
    /// The chunk multiplier to use with our worker count; at least 1
    #[clap(short, long, default_value = "100", value_parser = clap::value_parser!(u64).range(1..))]
    pub multiplier: u64,
    /// Attempts per object, including the first, before its failure is recorded; a transient
    /// error (a timeout, a reset connection, a busy node) is retried after a growing delay
    #[clap(long, default_value = "5", value_parser = clap::value_parser!(u32).range(1..))]
    pub retries: u32,
    /// The most S3 downloads in flight at once, across every S3 component of the run; 1 to
    /// 65,536
    #[clap(long, default_value = "64", value_parser = clap::value_parser!(u64).range(1..=65_536))]
    pub s3_concurrency: u64,
    /// Stop at the first object that fails: record it, exit 1, and leave the backup to be
    /// resumed. Without this flag the backup continues past every failure, records each one in
    /// failures.jsonl, and exits 2 if anything was recorded
    #[clap(long)]
    pub abort_on_failure: bool,
    /// Back up the Redis state even when the API reports a version other than thoradm's own
    /// (the full version, patch included) or none; the mismatch is logged at WARN and recorded
    /// in the manifest. Without it such a backup is refused before anything is written
    #[clap(long)]
    pub allow_version_mismatch: bool,
    /// Drop the user and group references that are still dangling after every read of the Redis
    /// state and whose user or group the API confirms is gone, instead of failing the redis
    /// component: group members, user memberships, and scoped-token groups. Each change is listed
    /// in pruned.jsonl and counted in the manifest, the run exits 2, and a restore of the backup
    /// needs --accept-failures. Every other problem still fails the redis component
    #[clap(long)]
    pub prune_dangling: bool,
    /// Keep going past a broken link, a reference to an S3 object the cluster itself lacks (an
    /// entity's or user's graphic, a sample or repo object, a result file, a comment attachment),
    /// instead of failing the object: the link stays as the cluster holds it, and each one is
    /// recorded as a broken-link record in failures.jsonl and counted apart in the manifest. The
    /// run exits 2 when anything is recorded, and a restore of a backup whose only records are
    /// accepted broken links needs no --accept-failures. Every other failure is unchanged
    #[clap(long)]
    pub accept_broken_links: bool,
}

/// Print the backup coverage registry
#[derive(Parser, Debug, Clone)]
pub struct CoverageBackup {
    /// Print a JSON array with one object per registry entry instead of the Markdown page
    #[clap(long)]
    pub json: bool,
}

/// Scrub a backup for bitrot
#[derive(Parser, Debug, Clone)]
pub struct ScrubBackup {
    /// The path to the backup to scrub
    #[clap(short, long)]
    pub backup: PathBuf,
    /// Stop at the first partition that fails its check: record it and exit 1. Without this flag
    /// every partition is checked, each failure is recorded, and the run exits 2 if anything was
    /// recorded
    #[clap(long)]
    pub abort_on_failure: bool,
}

/// Check a backup directory, or a restored cluster, against the backup's manifest
#[derive(Parser, Debug, Clone)]
pub struct VerifyBackup {
    /// The backup to check, or to check the cluster against
    #[clap(short, long)]
    pub backup: PathBuf,
    /// Check the cluster in the loaded config against the backup instead of the backup directory:
    /// rows per token range, objects, the Redis state through the API, views, and census streams.
    /// Pause every writer first, since rows written since the restore count as differences
    #[clap(long)]
    pub cluster: bool,
    /// Attempts per read of the cluster, including the first, before its failure is recorded
    #[clap(long, default_value = "5", value_parser = clap::value_parser!(u32).range(1..))]
    pub retries: u32,
    /// Where to write the report and failures file (default `<backup>/reports/verify-<time>/`)
    #[clap(long)]
    pub report_dir: Option<PathBuf>,
    /// Stop at the first failed check: record it and exit 1. Without this flag every check runs,
    /// each failure is recorded, and the run exits 2 if anything was recorded; a backup without a
    /// manifest, whose completeness can't be checked, exits 1
    #[clap(long)]
    pub abort_on_failure: bool,
}

/// Restore a backup to a specific Thorium cluster
// each switch is one command-line flag, so a field per flag reads as the command line does
#[allow(clippy::struct_excessive_bools)]
#[derive(Parser, Debug, Clone)]
pub struct RestoreBackup {
    /// The path to the backup to restore
    #[clap(short, long)]
    pub backup: PathBuf,
    /// Confirm the restore without asking; required when no terminal is attached. A `--wipe`
    /// restore on a terminal still asks for the namespace to be typed
    #[clap(long)]
    pub confirm: bool,
    /// Restore into a target whose backed-up tables already hold data, keeping what
    /// is there: data created after the backup survives, data deleted after it comes back, and
    /// on a key both hold the backup's values win. Without it such a target is refused
    #[clap(long, conflicts_with = "wipe")]
    pub merge: bool,
    /// Replace the target's backed-up tables with the backup: truncate them first, in the
    /// namespace thoradm's config names and the API serves. No bucket is touched (no S3 request
    /// is sent), so objects no restored row names may remain; excluded data (stage logs, events)
    /// is kept. On a terminal the namespace is typed again to confirm; without one, `--confirm`
    /// is required
    #[clap(long, value_name = "NAMESPACE", value_parser = thorium::conf::parse_namespace)]
    pub wipe: Option<String>,
    /// Have the API flush its whole Redis database with FLUSHDB ASYNC before writing redis.json,
    /// instead of deleting only this namespace's keys: every key in that database is deleted,
    /// other namespaces' included, while Redis's other databases are untouched (it is not
    /// FLUSHALL). Much faster for a large database; use it only when the database holds nothing
    /// but this Thorium. The backup is still validated first. No effect without redis.json
    #[clap(long)]
    pub redis_flush_all: bool,
    /// Restore a legacy backup, one with neither manifest.json nor backup-in-progress.json, whose
    /// completeness can't be checked; refused for any other backup
    #[clap(long)]
    pub legacy: bool,
    /// Restore a backup whose manifest records failed or skipped objects or references
    /// --prune-dangling dropped; the restore summary lists the objects the backup is missing.
    /// Without it such a backup is refused
    #[clap(long)]
    pub accept_failures: bool,
    /// Restore a backup holding redis.json, or wipe, even when the API reports a version other
    /// than thoradm's own (the full version, patch included) or none; the mismatch is logged at
    /// WARN and recorded in the summary. Without it such a restore is refused before anything is
    /// written
    #[clap(long)]
    pub allow_version_mismatch: bool,
    /// Where to write the report, failures file, and summary (default
    /// `<backup>/reports/restore-<time>/`)
    #[clap(long)]
    pub report_dir: Option<PathBuf>,
    /// Attempts per object, including the first, before its failure is recorded; a transient
    /// error (a 5xx, a throttling response, a reset connection) is retried after a growing delay
    #[clap(long, default_value = "5", value_parser = clap::value_parser!(u32).range(1..))]
    pub retries: u32,
    /// The most S3 uploads in flight at once, across every S3 component of the restore; 1 to
    /// 65,536
    #[clap(long, default_value = "64", value_parser = clap::value_parser!(u64).range(1..=65_536))]
    pub s3_concurrency: u64,
    /// Stop at the first object that fails: record it, exit 1, and leave a partly restored
    /// target. Without this flag the restore continues past every failure, records each one, and
    /// exits 2 if anything was recorded
    #[clap(long)]
    pub abort_on_failure: bool,
}

#[cfg(test)]
mod tests {
    use strum::IntoEnumIterator;

    use super::BackupComponents;

    /// Graphics needs the entities and redis components, and nothing else needs them
    #[test]
    fn graphics_needs_entities_and_redis() {
        use BackupComponents::{Entities, Graphics, Redis, Tags};
        // a selection without graphics needs nothing
        assert!(BackupComponents::check_graphics_inputs(&[Tags]).is_ok());
        // the message names exactly the components the selection lacks
        let neither = BackupComponents::check_graphics_inputs(&[Graphics]).unwrap_err();
        assert!(
            neither.starts_with("graphics needs entities and redis"),
            "{neither}"
        );
        let no_redis = BackupComponents::check_graphics_inputs(&[Graphics, Entities]).unwrap_err();
        assert!(no_redis.starts_with("graphics needs redis"), "{no_redis}");
        let no_entities = BackupComponents::check_graphics_inputs(&[Redis, Graphics]).unwrap_err();
        assert!(
            no_entities.starts_with("graphics needs entities"),
            "{no_entities}"
        );
        // both inputs, in any order, satisfy it
        assert!(BackupComponents::check_graphics_inputs(&[Redis, Graphics, Entities]).is_ok());
    }

    #[test]
    fn backup_all_always_true() {
        // when the user asks for `All`, every component should be backed up
        let all = [BackupComponents::All];
        for comp in BackupComponents::iter() {
            assert!(
                comp.should_backup(&all),
                "component {comp} should be backed up when `All` is requested",
            );
        }
        // even if `NoS3` is set
        let all = [BackupComponents::All, BackupComponents::NoS3];
        for comp in BackupComponents::iter() {
            assert!(
                comp.should_backup(&all),
                "component {comp} should be backed up when `All` is requested even if `NoS3` is also set",
            );
        }
    }

    #[test]
    fn no_s3_filters_out_s3_components() {
        // `NoS3` should skip anything that lives in S3 unless explicitly provided
        let set = [BackupComponents::NoS3];
        // check S3 components
        for comp in BackupComponents::iter().filter(|comp| comp.is_in_s3()) {
            assert!(
                !comp.should_backup(&set),
                "S3 component {comp} should be skipped with `NoS3`"
            );
        }
        // check non-S3 components
        for comp in BackupComponents::iter().filter(|comp| !comp.is_in_s3()) {
            assert!(
                comp.should_backup(&set),
                "Non-S3 component {comp} should not be skipped with `NoS3`",
            );
        }
    }

    #[test]
    fn explicit_component() {
        // no `All` and no `NoS3` means only the components the user listed are backed up
        let set = [BackupComponents::CommentAttachments];
        // listed component is true
        assert!(BackupComponents::CommentAttachments.should_backup(&set));
        // anything else is false
        for comp in
            BackupComponents::iter().filter(|comp| *comp != BackupComponents::CommentAttachments)
        {
            assert!(
                !comp.should_backup(&set),
                "component {comp} should NOT be backed up when not explicitly requested",
            );
        }
    }

    #[test]
    fn explicit_non_s3_with_no_s3_flag() {
        // `NoS3` + an explicit non‑S3 component should be true
        let set = [
            BackupComponents::NoS3,
            // pick a variant that is *not* stored in S3; for demonstration we use `SampleList`
            BackupComponents::SamplesList,
        ];
        assert!(BackupComponents::SamplesList.should_backup(&set));
    }

    #[test]
    fn explicit_s3_with_no_s3_flag() {
        // `NoS3` + an explicit S3 component is true (the explicit request should override the filter)
        let set = [BackupComponents::NoS3, BackupComponents::ResultFiles];
        assert!(BackupComponents::ResultFiles.should_backup(&set));
    }

    /// `--merge` and `--wipe` exclude each other, and `--wipe` needs the namespace it wipes; each
    /// is a usage error naming the flags
    #[test]
    fn wipe_takes_a_namespace_and_excludes_merge() {
        use clap::Parser;
        // a namespace given either way parses
        for args in [
            vec!["restore", "-b", "backup", "--wipe", "thorium"],
            vec!["restore", "-b", "backup", "--wipe=thorium"],
        ] {
            let parsed = super::RestoreBackup::try_parse_from(&args)
                .unwrap_or_else(|error| panic!("{args:?}: {error}"));
            assert_eq!(parsed.wipe.as_deref(), Some("thorium"));
            assert!(!parsed.merge);
        }
        // both flags together name both
        let error = super::RestoreBackup::try_parse_from([
            "restore", "-b", "backup", "--merge", "--wipe", "thorium",
        ])
        .expect_err("--merge with --wipe parsed")
        .to_string();
        assert!(
            error.contains("--merge") && error.contains("--wipe"),
            "{error}"
        );
        // a bare --wipe is refused, naming the flag
        let error = super::RestoreBackup::try_parse_from(["restore", "-b", "backup", "--wipe"])
            .expect_err("a bare --wipe parsed")
            .to_string();
        assert!(error.contains("--wipe"), "{error}");
    }

    /// `--wipe` refuses a value no Thorium config can name, stating the namespace rule
    #[test]
    fn wipe_refuses_an_invalid_namespace() {
        use clap::Parser;
        // a lowercase namespace with digits and underscores parses
        let parsed = super::RestoreBackup::try_parse_from([
            "restore",
            "-b",
            "backup",
            "--wipe",
            "thorium_2",
        ])
        .expect("a valid namespace was refused");
        assert_eq!(parsed.wipe.as_deref(), Some("thorium_2"));
        // a value breaking the rule, an uppercase letter included, is a usage error naming the
        // flag and the rule
        for namespace in [
            "",
            "2thorium",
            "thorium-dev",
            "a:b",
            "thorium*",
            "Thorium",
            "thoRium",
            "THORIUM",
            "Thorium_2",
        ] {
            let error = super::RestoreBackup::try_parse_from([
                "restore", "-b", "backup", "--wipe", namespace,
            ])
            .expect_err(namespace)
            .to_string();
            assert!(
                error.contains("--wipe")
                    && error.contains("lowercase ASCII letters (a-z)")
                    && error.contains("start with a lowercase letter"),
                "{namespace:?}: {error}"
            );
        }
    }

    /// `--confirm` is accepted, `--yes` is an unknown flag, and the help of `--confirm` says that
    /// a `--wipe` restore on a terminal still asks for the namespace
    #[test]
    fn confirm_replaces_yes_and_its_help_names_the_typed_wipe_confirmation() {
        use clap::{CommandFactory, Parser};
        // --confirm parses and sets the field
        let parsed = super::RestoreBackup::try_parse_from(["restore", "-b", "backup", "--confirm"])
            .expect("--confirm is accepted");
        assert!(parsed.confirm);
        // --yes is not a flag of restore
        let error = super::RestoreBackup::try_parse_from(["restore", "-b", "backup", "--yes"])
            .expect_err("--yes is rejected")
            .to_string();
        assert!(error.contains("--yes"), "{error}");
        // the help text of --confirm, as `--help` shows it
        let command = super::RestoreBackup::command();
        let help = command
            .get_arguments()
            .find(|arg| arg.get_id() == "confirm")
            .and_then(|arg| arg.get_help())
            .map_or_else(String::new, ToString::to_string);
        assert!(
            help.contains("--wipe") && help.contains("terminal"),
            "{help}"
        );
        // --yes names no argument
        assert!(command.get_arguments().all(|arg| arg.get_id() != "yes"));
    }

    /// `--s3-concurrency` takes 1 to 65,536 on `backup new` and `backup restore` and refuses
    /// anything else as a usage error
    #[test]
    fn s3_concurrency_is_bounded() {
        use clap::Parser;
        // the bounds themselves parse
        for value in ["1", "65536"] {
            let parsed = super::NewBackup::try_parse_from(["new", "--s3-concurrency", value])
                .unwrap_or_else(|error| panic!("{value}: {error}"));
            assert_eq!(parsed.s3_concurrency.to_string(), value);
            let parsed = super::RestoreBackup::try_parse_from([
                "restore",
                "-b",
                "backup",
                "--s3-concurrency",
                value,
            ])
            .unwrap_or_else(|error| panic!("{value}: {error}"));
            assert_eq!(parsed.s3_concurrency.to_string(), value);
        }
        // zero, one past the bound, and a value past tokio's semaphore limit are refused
        for value in ["0", "65537", "18446744073709551615"] {
            let error = super::NewBackup::try_parse_from(["new", "--s3-concurrency", value])
                .expect_err(value)
                .to_string();
            assert!(error.contains("--s3-concurrency"), "{value}: {error}");
            let error = super::RestoreBackup::try_parse_from([
                "restore",
                "-b",
                "backup",
                "--s3-concurrency",
                value,
            ])
            .expect_err(value)
            .to_string();
            assert!(error.contains("--s3-concurrency"), "{value}: {error}");
        }
    }
}
