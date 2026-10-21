//! The admin guides to backups describe what thoradm does: the Taking Backups guide describes
//! every file and directory a backup writes, with its mode, and names every flag, component, and
//! exit code of `backup new`; the thoradm guide covers restoring and reading the results; and
//! between them they name every flag of `backup new`, `restore`, `scrub`, and `verify` with its
//! default
//!
//! The guide's directory table is checked against real backups: a finished backup of the broad
//! fixture that was then scrubbed, a backup stopped by `--abort-on-failure` whose resumed run was
//! killed while it was downloading, and a backup of the Redis state that pruned a dangling owner.
//! Every file and directory in them must match a table entry of the same kind and mode, and every
//! table entry must match something in them.

mod common;

use axum::http::Method;
use clap::CommandFactory;
use common::harness::{Harness, redis_pool, write_cluster_conf};
use common::mock_s3::{MockS3, S3Behavior, S3Rule};
use common::names::Names;
use common::{cycle, fixture, kill, seed};
use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use strum::IntoEnumIterator;
use thoradm::args::{Args, BackupComponents};
use thoradm::backup::coverage::{Registry, Store};
use thoradm::outcome::RunOutcome;
use thorium::models::EntityKinds;
use uuid::Uuid;

/// The guide, relative to the thoradm crate
const GUIDE: &str = "../api/docs/src/admins/thoradm/taking_backups.md";

/// The thoradm guide, relative to the thoradm crate
const THORADM_GUIDE: &str = "../api/docs/src/admins/thoradm/thoradm.md";

/// The backup subcommands whose flags the two guides document
const SUBCOMMANDS: [&str; 4] = ["new", "restore", "scrub", "verify"];

/// The exit code of a thoradm that panicked
const PANIC_EXIT: u8 = 101;

/// The placeholder every path in the directory table starts with
const ROOT: &str = "<backup>";

/// Whether a path is a file or a directory
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    /// A regular file
    File,
    /// A directory
    Directory,
}

/// One row of the guide's directory table
#[derive(Debug)]
struct Entry {
    /// The path pattern below the backup directory; empty for the directory itself
    pattern: String,
    /// Whether the entry is a file or a directory
    kind: Kind,
    /// The permission bits the entry has
    mode: u32,
}

/// One file or directory found in a backup
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Found {
    /// Which backup it was found in, for messages
    backup: &'static str,
    /// Its path below the backup directory; empty for the directory itself
    path: String,
    /// Whether it is a file or a directory
    kind: Kind,
    /// Its permission bits
    mode: u32,
}

/// Reads the Taking Backups guide
fn guide() -> String {
    // the guide sits in the API's docs
    read_guide(GUIDE)
}

/// Reads one of the guides
///
/// # Arguments
///
/// * `path` - The guide, relative to the thoradm crate
fn read_guide(path: &str) -> String {
    // cargo runs tests from the crate root
    std::fs::read_to_string(path).unwrap_or_else(|err| panic!("failed to read {path}: {err}"))
}

/// Returns one backup subcommand's clap command
///
/// # Arguments
///
/// * `name` - The subcommand's name under `backup`
fn backup_subcommand(name: &str) -> clap::Command {
    // find the subcommand under `backup`
    Args::command()
        .find_subcommand("backup")
        .and_then(|backup| backup.find_subcommand(name))
        .cloned()
        .unwrap_or_else(|| panic!("thoradm has no backup {name} command"))
}

/// Returns a default value as the guides write it: a path under the home directory starts with `~`
///
/// # Arguments
///
/// * `value` - The default value clap holds
fn written_default(value: &str) -> String {
    // the guides can't name the reader's home directory
    match dirs::home_dir() {
        Some(home) if !home.as_os_str().is_empty() => {
            value.replacen(&*home.to_string_lossy(), "~", 1)
        }
        _ => value.to_owned(),
    }
}

/// Returns the cells of the Markdown table whose header row starts with a prefix
///
/// # Arguments
///
/// * `text` - The guide
/// * `header` - The start of the table's header row
fn table_rows(text: &str, header: &str) -> Vec<Vec<String>> {
    // find the header, skip its separator, and read rows until the table ends
    let mut lines = text.lines().skip_while(|line| !line.starts_with(header));
    assert!(
        lines.next().is_some(),
        "the guide has no table headed {header:?}"
    );
    lines
        .skip(1)
        .take_while(|line| line.starts_with('|'))
        .map(|line| {
            line.trim_matches('|')
                .split('|')
                .map(|cell| cell.trim().to_owned())
                .collect()
        })
        .collect()
}

/// Parses the guide's directory table
///
/// # Arguments
///
/// * `text` - The guide
fn entries(text: &str) -> Vec<Entry> {
    // each row names a path in backticks, a kind, and an octal mode
    table_rows(text, "| Path | Kind | Mode |")
        .into_iter()
        .map(|cells| {
            let path = cells[0].trim_matches('`');
            let pattern = path
                .strip_prefix(ROOT)
                .unwrap_or_else(|| panic!("{path} doesn't start with {ROOT}"))
                .trim_start_matches('/')
                .to_owned();
            let kind = match cells[1].as_str() {
                "file" => Kind::File,
                "directory" => Kind::Directory,
                other => panic!("{path} has an unknown kind {other:?}"),
            };
            let mode = u32::from_str_radix(&cells[2], 8)
                .unwrap_or_else(|err| panic!("{path} has a bad mode {:?}: {err}", cells[2]));
            Entry {
                pattern,
                kind,
                mode,
            }
        })
        .collect()
}

/// Returns whether a placeholder's value is one it stands for
///
/// # Arguments
///
/// * `name` - The placeholder's name, without its angle brackets
/// * `value` - The text it would stand for
fn valid(name: &str, value: &str) -> bool {
    // each placeholder admits only the values the guide says it stands for
    let is_uuid = Uuid::parse_str(value).is_ok();
    match name {
        "table" => Registry::backed_up(Store::ScyllaTable)
            .filter(|entry| entry.columns.is_some())
            .any(|entry| entry.name == value),
        "uuid" | "id" | "comment" | "attachment" | "result" => is_uuid,
        "role" => matches!(value, "files" | "repos"),
        "prefix" => (1..=6).contains(&value.chars().count()),
        "kind" => value.parse::<EntityKinds>().is_ok(),
        "sha256" => value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit()),
        "operation" => matches!(value, "backup" | "restore" | "scrub" | "verify"),
        "time" | "file" | "path" | "key" => !value.is_empty(),
        other => panic!("the guide uses an unknown placeholder <{other}>"),
    }
}

/// Returns whether one path segment matches one segment of a pattern, which may hold literal
/// text and placeholders
///
/// # Arguments
///
/// * `pattern` - The rest of the pattern's segment
/// * `text` - The rest of the path's segment
fn segment_matches(pattern: &str, text: &str) -> bool {
    // an exhausted pattern matches only the end of the segment
    if pattern.is_empty() {
        return text.is_empty();
    }
    // a placeholder takes a non-empty run of the text that is valid for it
    if let Some(rest) = pattern.strip_prefix('<') {
        let (name, after) = rest.split_once('>').expect("an unclosed placeholder");
        return text
            .char_indices()
            .map(|(index, ch)| index + ch.len_utf8())
            .any(|end| valid(name, &text[..end]) && segment_matches(after, &text[end..]));
    }
    // literal text must match up to the next placeholder
    let literal_end = pattern.find('<').unwrap_or(pattern.len());
    let literal = &pattern[..literal_end];
    text.strip_prefix(literal)
        .is_some_and(|rest| segment_matches(&pattern[literal_end..], rest))
}

/// Returns whether a path matches a pattern, segment by segment; `<file>`, `<path>`, and `<key>` as
/// a whole segment match one or more path segments
///
/// # Arguments
///
/// * `pattern` - The pattern's segments
/// * `path` - The path's segments
fn path_matches(pattern: &[&str], path: &[&str]) -> bool {
    // both end together, or the next pattern segment takes one or more path segments
    match (pattern.split_first(), path.split_first()) {
        (None, None) => true,
        (None, Some(_)) | (Some(_), None) => false,
        (Some((&("<file>" | "<path>" | "<key>"), rest)), _) => (1..=path.len()).any(|taken| {
            path[..taken].iter().all(|segment| !segment.is_empty())
                && path_matches(rest, &path[taken..])
        }),
        (Some((segment, rest)), Some((first, others))) => {
            segment_matches(segment, first) && path_matches(rest, others)
        }
    }
}

/// Returns whether an entry describes a found file or directory, mode included
///
/// # Arguments
///
/// * `entry` - The guide's entry
/// * `found` - What a backup holds
fn describes(entry: &Entry, found: &Found) -> bool {
    // the directory itself has no segments
    let split = |path: &str| -> Vec<String> {
        if path.is_empty() {
            Vec::new()
        } else {
            path.split('/').map(ToOwned::to_owned).collect()
        }
    };
    let pattern = split(&entry.pattern);
    let path = split(&found.path);
    let pattern: Vec<&str> = pattern.iter().map(String::as_str).collect();
    let path: Vec<&str> = path.iter().map(String::as_str).collect();
    entry.kind == found.kind && entry.mode == found.mode && path_matches(&pattern, &path)
}

/// Lists a backup directory and everything in it with its kind and permission bits
///
/// # Arguments
///
/// * `backup` - Which backup it is, for messages
/// * `root` - The backup directory
fn walk(backup: &'static str, root: &Path) -> Vec<Found> {
    // start from the directory itself and walk depth first
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        let kind = if metadata.is_dir() {
            Kind::Directory
        } else {
            assert!(
                metadata.is_file(),
                "{} is neither file nor directory",
                path.display()
            );
            Kind::File
        };
        // a directory's entries are listed too
        if kind == Kind::Directory {
            for entry in std::fs::read_dir(&path).unwrap() {
                pending.push(entry.unwrap().path());
            }
        }
        found.push(Found {
            backup,
            path: path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            kind,
            mode: metadata.permissions().mode() & 0o7777,
        });
    }
    found.sort();
    found
}

/// Builds a thoradm command that reads a cluster config other than the harness's
///
/// # Arguments
///
/// * `harness` - The harness naming the Thorctl config
/// * `cluster_conf` - The cluster config to read
fn thoradm_with(harness: &Harness, cluster_conf: &Path) -> Command {
    // drop every config override the test process inherited, as the harness's command does
    let mut command = Command::new(env!("CARGO_BIN_EXE_thoradm"));
    for (key, _) in std::env::vars() {
        if key.starts_with("THORIUM__") || key.starts_with("THORCTL__") {
            command.env_remove(key);
        }
    }
    command
        .arg("--cluster-conf")
        .arg(cluster_conf)
        .arg("--ctl-conf")
        .arg(&harness.ctl_conf);
    command
}

/// Returns whether an S3 component's staging directory under a backup holds a file
///
/// # Arguments
///
/// * `root` - The backup directory
fn staging_holds_a_file(root: &Path) -> bool {
    // look in every table directory's staging directory
    let Ok(tables) = std::fs::read_dir(root) else {
        return false;
    };
    tables.filter_map(Result::ok).any(|table| {
        std::fs::read_dir(table.path().join("staging"))
            .is_ok_and(|mut staged| staged.next().is_some())
    })
}

/// Returns one section of the guide: from its heading to the next heading of the same level
///
/// # Arguments
///
/// * `text` - The guide
/// * `heading` - The section's heading line
fn section<'a>(text: &'a str, heading: &str) -> &'a str {
    // the section starts at its heading and ends at the next heading of its level
    let start = text
        .find(heading)
        .unwrap_or_else(|| panic!("the guide has no section {heading}"));
    let level = heading.split(' ').next().unwrap_or("##");
    let rest = &text[start + heading.len()..];
    let end = rest
        .find(&format!("\n{level} "))
        .map_or(text.len(), |end| start + heading.len() + end);
    &text[start..end]
}

/// Returns the exit codes the guide must list: every run outcome's, and a panic's
fn exit_codes() -> BTreeSet<u8> {
    // every outcome, guarded so a new one fails to compile until it is listed here
    let outcomes = [
        RunOutcome::Complete,
        RunOutcome::Aborted,
        RunOutcome::CompletedWithFailures,
    ];
    let mut codes: BTreeSet<u8> = outcomes
        .iter()
        .map(|outcome| match outcome {
            RunOutcome::Complete | RunOutcome::Aborted | RunOutcome::CompletedWithFailures => {
                outcome.code()
            }
        })
        .collect();
    codes.insert(PANIC_EXIT);
    codes
}

/// The guide names every flag of `backup new` and every global flag, every component, exactly the
/// exit codes thoradm has, and every row of the coverage registry's page
#[test]
fn the_guide_names_every_flag_component_exit_code_and_coverage_row() {
    // every long flag of thoradm itself and of `backup new`, read from clap
    let text = guide();
    let command = Args::command();
    let new = command
        .find_subcommand("backup")
        .and_then(|backup| backup.find_subcommand("new"))
        .expect("thoradm has no backup new command");
    let flags: Vec<String> = command
        .get_arguments()
        .chain(new.get_arguments())
        .filter_map(|arg| arg.get_long())
        .filter(|long| !matches!(*long, "help" | "version"))
        .map(|long| format!("`--{long}`"))
        .collect();
    assert!(flags.len() >= 8, "too few flags read from clap: {flags:?}");
    for flag in &flags {
        assert!(
            text.contains(flag.as_str()),
            "the guide doesn't name {flag}"
        );
    }
    // its restore section names every flag of `backup restore`
    let restore = command
        .find_subcommand("backup")
        .and_then(|backup| backup.find_subcommand("restore"))
        .expect("thoradm has no backup restore command");
    let section = section(&text, "## How Restore Uses a Backup");
    let restore_flags: Vec<String> = restore
        .get_arguments()
        .filter_map(|arg| arg.get_long())
        .filter(|long| !matches!(*long, "help" | "version"))
        .map(|long| format!("`--{long}`"))
        .collect();
    assert!(
        restore_flags.iter().any(|flag| flag == "`--merge`"),
        "too few restore flags read from clap: {restore_flags:?}"
    );
    for flag in &restore_flags {
        assert!(
            section.contains(flag.as_str()),
            "the guide's restore section doesn't name {flag}"
        );
    }
    // every component by the name the command line takes
    for component in BackupComponents::iter() {
        let name = format!("`{component}`");
        assert!(text.contains(&name), "the guide doesn't name {name}");
    }
    // the exit-code table lists exactly the codes thoradm exits with
    let listed: BTreeSet<u8> = table_rows(&text, "| Exit code |")
        .iter()
        .map(|cells| cells[0].parse().expect("an exit code that isn't a number"))
        .collect();
    assert_eq!(listed, exit_codes());
    // every row of the coverage page generated from the registry appears unchanged
    let lines: BTreeSet<&str> = text.lines().collect();
    for row in Registry::render_page()
        .lines()
        .filter(|line| line.starts_with("| "))
    {
        assert!(
            lines.contains(row),
            "the guide lacks the coverage row {row}"
        );
    }
}

/// The thoradm guide links the Taking Backups guide, lists exactly the exit codes thoradm has,
/// names every excluded resource, names every restore flag in its restore section, describes how
/// each backup command runs with and without `--abort-on-failure`, and never says a restore wipes
/// all current data
#[test]
fn the_thoradm_guide_documents_restoring_and_reading_the_results() {
    // the guide links the Taking Backups guide and drops the old claim about restore
    let text = read_guide(THORADM_GUIDE);
    assert!(
        text.contains("](./taking_backups.md"),
        "the thoradm guide doesn't link the Taking Backups guide"
    );
    assert!(
        !text.to_lowercase().contains("wipe all current data"),
        "the thoradm guide still says a restore wipes all current data"
    );
    // the exit-code table lists exactly the codes thoradm exits with
    let listed: BTreeSet<u8> = table_rows(&text, "| Exit code |")
        .iter()
        .map(|cells| cells[0].parse().expect("an exit code that isn't a number"))
        .collect();
    assert_eq!(listed, exit_codes());
    // every excluded resource of the coverage registry is named where the guide says what a
    // backup holds
    let holds = section(&text, "### What a Backup Holds");
    let unnamed: Vec<&str> = Registry::excluded()
        .map(|entry| entry.name)
        .filter(|name| !holds.contains(&format!("`{name}`")))
        .collect();
    assert!(
        unnamed.is_empty(),
        "the thoradm guide's What a Backup Holds doesn't name the excluded {unnamed:?}"
    );
    // its restore section names every flag of `backup restore`
    let restore = section(&text, "## Restoring a Backup");
    let restore_flags: Vec<String> = backup_subcommand("restore")
        .get_arguments()
        .filter_map(|arg| arg.get_long())
        .filter(|long| !matches!(*long, "help" | "version"))
        .map(|long| format!("`--{long}`"))
        .collect();
    for required in ["`--merge`", "`--wipe`", "`--legacy`", "`--accept-failures`"] {
        assert!(
            restore_flags.iter().any(|flag| flag == required),
            "backup restore has no {required}: {restore_flags:?}"
        );
    }
    for flag in &restore_flags {
        assert!(
            restore.contains(flag.as_str()),
            "the thoradm guide's restore section doesn't name {flag}"
        );
    }
    // each backup command has a row saying how it runs with and without --abort-on-failure
    let commands: BTreeSet<String> =
        table_rows(&text, "| Command | Without `--abort-on-failure` |")
            .into_iter()
            .map(|cells| cells[0].clone())
            .collect();
    let expected: BTreeSet<String> = SUBCOMMANDS
        .iter()
        .map(|name| format!("`backup {name}`"))
        .collect();
    assert_eq!(commands, expected);
}

/// Every flag that `--help` prints for `backup new`, `restore`, `scrub`, and `verify`, and every
/// global flag, is named in the Taking Backups guide or the thoradm guide by its long and short
/// form, and every flag that has a default is named on a line that gives it
#[test]
fn the_guides_name_every_backup_flag_with_its_default() {
    // both guides, read as one text and as lines
    let text = format!("{}\n{}", guide(), read_guide(THORADM_GUIDE));
    let lines: Vec<&str> = text.lines().collect();
    // the global flags and the flags of each backup subcommand
    let global = Args::command();
    let mut commands = vec![("thoradm".to_owned(), global)];
    commands.extend(
        SUBCOMMANDS
            .iter()
            .map(|name| (format!("backup {name}"), backup_subcommand(name))),
    );
    let mut checked = 0;
    let mut problems = Vec::new();
    for (command, clap) in &commands {
        for arg in clap.get_arguments() {
            // help and version are clap's own, and a positional has no flag to name
            let Some(long) = arg.get_long() else {
                continue;
            };
            if matches!(long, "help" | "version") {
                continue;
            }
            checked += 1;
            let flag = format!("`--{long}`");
            if !text.contains(&flag) {
                problems.push(format!("{command}: {flag} isn't named"));
            }
            if let Some(short) = arg.get_short() {
                let short = format!("`-{short}`");
                if !text.contains(&short) {
                    problems.push(format!("{command}: {short} ({flag}) isn't named"));
                }
            }
            // a flag that takes a value and has a default is named beside it
            if !arg.get_action().takes_values() {
                continue;
            }
            for default in arg.get_default_values() {
                let default = format!("`{}`", written_default(&default.to_string_lossy()));
                if !lines
                    .iter()
                    .any(|line| line.contains(&flag) && line.contains(&default))
                {
                    problems.push(format!(
                        "{command}: no line names {flag} with its default {default}"
                    ));
                }
            }
        }
    }
    assert!(checked >= 20, "too few flags read from clap: {checked}");
    assert!(problems.is_empty(), "{problems:#?}");
}

/// Both guides describe `--redis-flush-all` for large restores: off by default on the line that
/// names it, what it flushes (the API's database with `FLUSHDB ASYNC`) and what it doesn't
/// (`FLUSHALL`, Redis's other databases), when to use it with the measured speeds, that memory is
/// freed in the background, and that writers are paused first
#[test]
fn the_guides_describe_flushing_the_redis_database_for_large_restores() {
    // each guide on its own, since both describe the flag
    for path in [GUIDE, THORADM_GUIDE] {
        let text = read_guide(path);
        // the flag is named off by default on one line
        assert!(
            text.lines()
                .any(|line| line.contains("`--redis-flush-all`") && line.contains("off")),
            "{path}: no line names `--redis-flush-all` with its default (off)"
        );
        // what it does and doesn't flush, when to use it, and its gotchas
        let lower = text.to_lowercase();
        for needle in [
            "`flushdb async`",
            "`flushall`",
            "other databases",
            "every key in the database",
            "dedicated",
            "1.5 s per million keys",
            "0.1 s per million keys",
            "in the background",
            "pause",
            "large restores",
        ] {
            assert!(
                lower.contains(needle),
                "{path}: the guide doesn't say {needle:?} about `--redis-flush-all`"
            );
        }
    }
}

/// The `--help` of `backup new`, `restore`, `scrub`, and `verify` lists exactly the exit codes
/// thoradm has
#[test]
fn every_backup_command_help_lists_the_exit_codes() {
    // render each subcommand's long help, as `--help` prints it
    for name in SUBCOMMANDS {
        let help = backup_subcommand(name).render_long_help().to_string();
        // the exit codes follow their heading, one indented line each
        let listed: BTreeSet<u8> = help
            .lines()
            .skip_while(|line| line.trim() != "Exit codes:")
            .skip(1)
            .map_while(|line| {
                line.split_whitespace()
                    .next()
                    .and_then(|code| code.parse().ok())
            })
            .collect();
        assert_eq!(listed, exit_codes(), "backup {name} --help:\n{help}");
    }
}

/// Takes a finished backup of everything, then scrubs and verifies it, which adds their reports
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
fn finished_backup(harness: &Harness) -> PathBuf {
    // a backup of every component
    let full = harness.scratch("guide-full");
    let output = cycle::backup(harness, &full);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        cycle::describe(&output)
    );
    // a scrub and a verify of it, which add their reports
    for command in ["scrub", "verify"] {
        let output = harness
            .thoradm()
            .args(["backup", command, "-b"])
            .arg(&full)
            .output()
            .expect("failed to run thoradm");
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            cycle::describe(&output)
        );
    }
    full
}

/// Takes a backup that `--abort-on-failure` stops at an object S3 says is missing, then resumes
/// it while every download stalls and kills the resumed run once a download is staged
///
/// Children that talk to `MockS3` are awaited, so the mock keeps serving on the test's runtime.
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
async fn interrupted_backup(harness: &Harness) -> PathBuf {
    // thoradm's S3 requests go through a mock for the interrupted backup
    let mock = MockS3::start(&harness.conf.thorium.s3.endpoint)
        .await
        .expect("failed to start MockS3");
    let mut conf = harness.conf.clone();
    conf.thorium.s3.endpoint.clone_from(&mock.endpoint);
    let confs = harness.scratch("guide-confs");
    let cluster_conf = confs.join("thorium.yml");
    write_cluster_conf(&conf, &cluster_conf).unwrap();
    let rule = |behavior| S3Rule {
        method: Method::GET,
        key: "*".to_owned(),
        query: None,
        nth: None,
        behavior,
    };
    // a backup stopped by --abort-on-failure at an object S3 says is missing
    let interrupted = harness.scratch("guide-interrupted");
    mock.set_rules(vec![rule(S3Behavior::Status {
        status: 404,
        code: "NoSuchKey".to_owned(),
    })])
    .await;
    let mut command = thoradm_with(harness, &cluster_conf);
    command
        .args([
            "--workers",
            "2",
            "backup",
            "new",
            "--abort-on-failure",
            "-m",
            "2",
            "-o",
        ])
        .arg(&interrupted);
    let output = tokio::process::Command::from(command)
        .output()
        .await
        .expect("failed to run thoradm");
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        cycle::describe(&output)
    );
    // resumed while every download stalls, and killed once a download is staged
    mock.set_rules(vec![rule(S3Behavior::StallAfter(1))]).await;
    let mut command = thoradm_with(harness, &cluster_conf);
    command
        .args(["--workers", "2", "backup", "new", "-m", "2", "-o"])
        .arg(&interrupted);
    let killed = kill::run_until_found(command, || staging_holds_a_file(&interrupted))
        .await
        .expect("failed to run thoradm");
    assert!(
        killed.killed,
        "the resumed backup ended before a download was staged: {:?}",
        killed.stderr
    );
    interrupted
}

/// Takes a backup of the Redis state with `--prune-dangling` while a group owner names a user that
/// is gone, so it writes `pruned.jsonl`, then puts the group back
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
async fn pruned_backup(harness: &Harness) -> PathBuf {
    // a group whose owners include a user that doesn't exist
    let ns = &harness.conf.thorium.namespace;
    let names = Names::new();
    let group = names.group("minerva");
    seed::group(&harness.client().await, &group).await;
    let ghost = names.user("elara");
    let sets = [
        format!("{ns}:groups_direct:{group}:owners"),
        format!("{ns}:groups_combined:{group}:owners"),
    ];
    let pool = redis_pool(&harness.conf).await.unwrap();
    let mut conn = pool.get().await.unwrap();
    for set in &sets {
        let () = redis::cmd("SADD")
            .arg(set)
            .arg(&ghost)
            .query_async(&mut *conn)
            .await
            .unwrap();
    }
    // the backup drops the owner and exits 2
    let pruned = harness.scratch("guide-pruned");
    let output = harness
        .thoradm()
        .args(["backup", "new", "redis", "--prune-dangling", "-o"])
        .arg(&pruned)
        .output()
        .expect("failed to run thoradm");
    // put the group back before checking, so a failure leaves nothing dangling
    for set in &sets {
        let () = redis::cmd("SREM")
            .arg(set)
            .arg(&ghost)
            .query_async(&mut *conn)
            .await
            .unwrap();
    }
    assert_eq!(
        output.status.code(),
        Some(2),
        "{}",
        cycle::describe(&output)
    );
    pruned
}

/// Every file and directory a finished, scrubbed backup, an interrupted backup, and a backup that
/// pruned dangling references hold matches an entry of the guide's directory table with its
/// mode, and every entry matches one of them
#[tokio::test]
async fn the_guide_describes_every_file_a_backup_writes() {
    // the guide's table, and a namespace seeded with one of every kind of resource
    let harness = Harness::get().await;
    let _guard = harness.shared_guard().await;
    let table = entries(&guide());
    Box::pin(fixture::broad(harness)).await;
    // the three backups
    let full = finished_backup(harness);
    let interrupted = Box::pin(interrupted_backup(harness)).await;
    let pruned = pruned_backup(harness).await;
    // every file and directory the backups hold
    let mut found = walk("finished", &full);
    found.extend(walk("interrupted", &interrupted));
    found.extend(walk("pruned", &pruned));
    // each one is described by an entry of its kind and mode
    let undescribed: Vec<String> = found
        .iter()
        .filter(|found| !table.iter().any(|entry| describes(entry, found)))
        .map(|found| {
            format!(
                "{}: {:?} ({:?}, {:o})",
                found.backup, found.path, found.kind, found.mode
            )
        })
        .collect();
    assert!(
        undescribed.is_empty(),
        "the guide's directory table describes none of: {undescribed:#?}"
    );
    // each entry describes something a backup holds
    let stale: Vec<&Entry> = table
        .iter()
        .filter(|entry| !found.iter().any(|found| describes(entry, found)))
        .collect();
    assert!(
        stale.is_empty(),
        "these entries of the guide's directory table match nothing either backup holds: \
         {stale:#?}"
    );
    // every field the files of both backups hold is named in the guide
    let text = guide();
    let mut missing = Vec::new();
    for (file, field) in json_fields(&full, &interrupted, &pruned) {
        if !text.contains(&format!("`{field}`")) {
            missing.push(format!("{file}: {field}"));
        }
    }
    assert!(
        missing.is_empty(),
        "the guide doesn't name these fields: {missing:#?}"
    );
}

/// Returns the keys of a JSON object
///
/// # Arguments
///
/// * `value` - The value, which must be an object
fn keys(value: &serde_json::Value) -> Vec<String> {
    // every key of the object, in order
    value
        .as_object()
        .unwrap_or_else(|| panic!("expected a JSON object, found {value}"))
        .keys()
        .cloned()
        .collect()
}

/// Reads one JSON file
///
/// # Arguments
///
/// * `path` - The file
fn read_json(path: &Path) -> serde_json::Value {
    // each file is whole JSON
    serde_json::from_slice(&std::fs::read(path).unwrap())
        .unwrap_or_else(|err| panic!("{} isn't JSON: {err}", path.display()))
}

/// Returns the first line of a JSON lines file under a directory whose path ends with a suffix,
/// parsed
///
/// # Arguments
///
/// * `root` - The directory to search
/// * `suffix` - The end of the file's path
fn first_line(root: &Path, suffix: &str) -> serde_json::Value {
    // search depth first for a non-empty file with the suffix
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.to_string_lossy().ends_with(suffix) {
                let text = std::fs::read_to_string(&path).unwrap();
                if let Some(line) = text.lines().next() {
                    return serde_json::from_str(line).unwrap();
                }
            }
        }
    }
    panic!("no non-empty {suffix} under {}", root.display())
}

/// Lists the field names of every JSON file the three backups hold, with the file each came from;
/// the keys that name tables, components, bucket roles, and data types are values, not fields
///
/// # Arguments
///
/// * `full` - The finished backup
/// * `interrupted` - The interrupted backup
/// * `pruned` - The backup that pruned dangling references
fn json_fields(full: &Path, interrupted: &Path, pruned: &Path) -> Vec<(&'static str, String)> {
    // the manifest's own fields and those of each nested entry kind
    let manifest = read_json(&full.join("manifest.json"));
    let mut fields: Vec<(&'static str, String)> = keys(&manifest)
        .into_iter()
        .map(|key| ("manifest.json", key))
        .collect();
    let first = |map: &serde_json::Value| -> serde_json::Value {
        map.as_object()
            .and_then(|map| map.values().next().cloned())
            .expect("an empty map in the manifest")
    };
    let table = first(&manifest["tables"]);
    let nested = [
        table.clone(),
        table["archives"][0].clone(),
        table["ranges"][0].clone(),
        first(&manifest["s3"]),
        manifest["redis"].clone(),
    ];
    for value in &nested {
        fields.extend(keys(value).into_iter().map(|key| ("manifest.json", key)));
    }
    // the pruning backup's manifest, with its pruned entry
    let pruned_manifest = read_json(&pruned.join("manifest.json"));
    for value in [&pruned_manifest, &pruned_manifest["pruned"]] {
        fields.extend(keys(value).into_iter().map(|key| ("manifest.json", key)));
    }
    // the marker, a failure record, and an object index line
    let others = [
        (
            "backup-in-progress.json",
            read_json(&interrupted.join("backup-in-progress.json")),
        ),
        ("failures.jsonl", first_line(interrupted, "failures.jsonl")),
        (
            "objects.index.jsonl",
            first_line(full, "objects.index.jsonl"),
        ),
        ("pruned.jsonl", first_line(pruned, "pruned.jsonl")),
    ];
    for (file, value) in others {
        fields.extend(keys(&value).into_iter().map(|key| (file, key)));
    }
    fields
}
