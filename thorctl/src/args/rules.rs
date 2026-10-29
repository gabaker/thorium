//! Arguments for YARA/Sigma detection rule-related Thorctl commands

#![allow(clippy::module_name_repetitions)]

use clap::Parser;
use std::path::PathBuf;
use thorium::Error;
use thorium::models::{EntityKinds, SigmaRuleAppliesTo, YaraRuleAppliesTo};
use uuid::Uuid;

use super::{CreateNotification, GetNotificationOpts, editor_help};

/// The commands to send to the rules task handler
#[derive(Parser, Debug)]
pub enum Rules {
    /// Create a YARA or Sigma detection rule in Thorium
    #[clap(version, author)]
    Create(CreateRule),
    /// Get the full details of a detection rule
    #[clap(version, author)]
    Get(GetRule),
    /// List detection rules in Thorium
    #[clap(version, author)]
    List(ListRules),
    /// Update a detection rule in Thorium
    #[clap(version, author)]
    Update(UpdateRule),
    /// Edit a detection rule's source in an editor
    #[clap(version, author)]
    Edit(EditRule),
    /// Delete detection rules from Thorium
    #[clap(version, author)]
    Delete(DeleteRules),
    /// Bulk import rule files and print a compatibility report
    #[clap(version, author)]
    Import(ImportRules),
    /// Manage a detection rule's notifications (e.g. quarantine reasons)
    #[clap(subcommand)]
    Notifications(RuleNotifications),
    /// Re-admit a quarantined rule by deleting its status tag
    #[clap(version, author)]
    Unquarantine(UnquarantineRule),
    /// Exempt a rule from noisy/slow quarantine (admins only)
    #[clap(version, author)]
    Exempt(ExemptRule),
}

/// The scan engines a detection rule can target
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleEngine {
    /// A YARA rule that scans raw sample bytes
    Yara,
    /// A Sigma rule that scans log-like tool result entities
    Sigma,
}

impl RuleEngine {
    /// Get the entity kind for this rule engine
    #[must_use]
    pub fn kind(self) -> EntityKinds {
        // map each engine to its rule entity kind
        match self {
            RuleEngine::Yara => EntityKinds::YaraRule,
            RuleEngine::Sigma => EntityKinds::SigmaRule,
        }
    }

    /// Cast this [`RuleEngine`] to a str
    #[must_use]
    pub fn as_str(self) -> &'static str {
        // map each engine to its lowercase name
        match self {
            RuleEngine::Yara => "yara",
            RuleEngine::Sigma => "sigma",
        }
    }
}

/// The kinds of data a detection rule can apply to across all engines
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleAppliesTo {
    /// Scan raw sample bytes (YARA rules only)
    Samples,
    /// Scan windows process entities (Sigma rules only)
    WindowsProcesses,
    /// Scan network connection entities (Sigma rules only)
    NetworkConnections,
    /// Scan compiled function entities (Sigma rules only)
    CompiledFunctions,
    /// Scan decompiled function entities (Sigma rules only)
    DecompiledFunctions,
}

impl RuleAppliesTo {
    /// Cast this [`RuleAppliesTo`] to a str
    #[must_use]
    pub fn as_str(self) -> &'static str {
        // map each applies to value to its flag value name
        match self {
            RuleAppliesTo::Samples => "samples",
            RuleAppliesTo::WindowsProcesses => "windows-processes",
            RuleAppliesTo::NetworkConnections => "network-connections",
            RuleAppliesTo::CompiledFunctions => "compiled-functions",
            RuleAppliesTo::DecompiledFunctions => "decompiled-functions",
        }
    }

    /// Convert this applies to value to its YARA model value
    ///
    /// # Errors
    ///
    /// Errors if this applies to value is not valid for YARA rules
    pub fn to_yara(self) -> Result<YaraRuleAppliesTo, Error> {
        // only sample scanning is valid for yara rules
        match self {
            RuleAppliesTo::Samples => Ok(YaraRuleAppliesTo::Samples),
            other => Err(Error::new(format!(
                "YARA rules cannot apply to '{}'; valid values: samples",
                other.as_str()
            ))),
        }
    }

    /// Convert this applies to value to its Sigma model value
    ///
    /// # Errors
    ///
    /// Errors if this applies to value is not valid for Sigma rules
    pub fn to_sigma(self) -> Result<SigmaRuleAppliesTo, Error> {
        // sample scanning is the only value that is not valid for sigma rules
        match self {
            RuleAppliesTo::WindowsProcesses => Ok(SigmaRuleAppliesTo::WindowsProcesses),
            RuleAppliesTo::NetworkConnections => Ok(SigmaRuleAppliesTo::NetworkConnections),
            RuleAppliesTo::CompiledFunctions => Ok(SigmaRuleAppliesTo::CompiledFunctions),
            RuleAppliesTo::DecompiledFunctions => Ok(SigmaRuleAppliesTo::DecompiledFunctions),
            RuleAppliesTo::Samples => Err(Error::new(
                "Sigma rules cannot apply to 'samples'; valid values: windows-processes, \
                network-connections, compiled-functions, decompiled-functions",
            )),
        }
    }
}

/// Trim surrounding whitespace from one comma separated list value
///
/// `value_delimiter` splits on every comma without trimming, so `-g "a, b"`
/// would otherwise look up the literal name ` b`; empty values from doubled
/// or trailing commas are rejected instead of silently resolving nothing.
///
/// # Arguments
///
/// * `raw` - The raw list value to trim
pub(crate) fn trim_arg(raw: &str) -> Result<String, Error> {
    // trim the whitespace the shell or user left around this value
    let trimmed = raw.trim();
    // reject empty values from doubled or trailing commas
    if trimmed.is_empty() {
        return Err(Error::new("list values cannot be empty"));
    }
    Ok(trimmed.to_owned())
}

/// Parse a combined tag into its key and value
///
/// # Arguments
///
/// * `raw` - The raw combined tag to parse
pub(crate) fn parse_tag(raw: &str) -> Result<(String, String), Error> {
    // split this tag on its first '=' so values can contain '='
    match raw.split_once('=') {
        Some((key, value)) if !key.is_empty() && !value.is_empty() => {
            Ok((key.to_owned(), value.to_owned()))
        }
        _ => Err(Error::new(format!(
            "'{raw}' is not a valid tag (expected <KEY>=<VALUE>)"
        ))),
    }
}

/// A command to create a detection rule in Thorium
#[derive(Parser, Debug, Clone)]
pub struct CreateRule {
    /// The name for this rule entity
    #[clap(short, long)]
    pub name: String,
    /// The scan engine this rule targets
    #[clap(short, long, value_enum, ignore_case = true)]
    pub engine: RuleEngine,
    /// The path to the file containing this rule's source
    ///
    /// YARA sources may define several rules in one document and are compiled
    /// server-side; compile errors and warnings are printed verbatim. Sigma
    /// sources must be a single valid Sigma YAML document.
    #[clap(short = 'f', long)]
    pub rule_file: PathBuf,
    /// The groups to create this rule in
    #[clap(short, long, required = true, value_delimiter = ',', value_parser = trim_arg)]
    pub groups: Vec<String>,
    /// The score to apply when this rule hits
    ///
    /// Defaults to 0 for YARA rules and to a score derived from the rule's
    /// `level` for Sigma rules (informational/none 0, low 1, medium 5,
    /// high 20, critical 50).
    #[clap(short, long, allow_negative_numbers = true)]
    pub score: Option<i64>,
    /// The kinds of data this rule applies to
    ///
    /// Defaults to 'samples' for YARA rules and 'windows-processes' for Sigma
    /// rules.
    #[clap(long, value_enum, ignore_case = true, value_delimiter = ',')]
    pub applies_to: Vec<RuleAppliesTo>,
    /// The tags to add to this rule where key/value are separated by '='
    /// (i.e. <KEY>=<VALUE>); repeat the flag to add several tags
    #[clap(short = 'T', long)]
    pub tags: Vec<String>,
    /// A description for this rule
    #[clap(short, long)]
    pub description: Option<String>,
}

/// A command to get a detection rule's full details
#[derive(Parser, Debug, Clone)]
pub struct GetRule {
    /// The id or name of the rule to get
    pub rule: String,
    /// Print this rule as JSON instead of the human-readable summary
    #[clap(long)]
    pub json: bool,
    /// Print condensed JSON (no formatting/whitespace; implies --json)
    #[clap(short, long)]
    pub condensed: bool,
    /// Write the output to a file instead of stdout (files are never
    /// ANSI-styled)
    #[clap(short, long)]
    pub output: Option<PathBuf>,
}

/// A command to list detection rules in Thorium
#[derive(Parser, Debug, Clone)]
pub struct ListRules {
    /// Only list rules for this engine (defaults to all engines)
    #[clap(short, long, value_enum, ignore_case = true)]
    pub engine: Option<RuleEngine>,
    /// Any groups to filter by when listing rules
    ///
    /// If no groups are given, the search will include all groups the user is
    /// a part of.
    #[clap(short, long, value_delimiter = ',', value_parser = trim_arg)]
    pub groups: Vec<String>,
    /// Any tags to filter by where key/value are separated by '='
    /// (i.e. <KEY>=<VALUE>); repeat the flag to filter on several tags
    #[clap(short, long)]
    pub tags: Vec<String>,
    /// The cursor to continue a search with
    #[clap(long)]
    pub cursor: Option<Uuid>,
    /// The max number of total rules to list
    #[clap(short, long, default_value_t = 50, conflicts_with = "no_limit")]
    pub limit: usize,
    /// List all rules without a limit
    #[clap(long)]
    pub no_limit: bool,
    /// The number of rules to retrieve per request
    #[clap(short, long, default_value_t = 50)]
    pub page_size: usize,
    /// Print the rules as JSON instead of a table
    #[clap(long)]
    pub json: bool,
    /// Print condensed JSON (implies `--json`)
    #[clap(short, long)]
    pub condensed: bool,
    /// Write JSON to a file instead of stdout (implies `--json`)
    #[clap(short, long)]
    pub output: Option<PathBuf>,
}

/// A command to update a detection rule in Thorium
#[derive(Parser, Debug, Clone)]
pub struct UpdateRule {
    /// The id or name of the rule to update
    pub rule: String,
    /// The updates to apply to this rule where at least one must be set
    #[clap(flatten)]
    pub opts: RuleUpdateOpts,
}

/// The set of possible updates to a detection rule where at least one is set
#[derive(clap::Args, Debug, Clone)]
#[group(required = true, multiple = true)]
pub struct RuleUpdateOpts {
    /// The path to a file containing the new rule source to set
    ///
    /// New YARA sources are compile-checked server-side. Fixing a rule does not
    /// clear its quarantine status: run 'thorctl rules unquarantine <RULE>'
    /// to re-admit it to scanning.
    #[clap(short = 'f', long)]
    pub rule_file: Option<PathBuf>,
    /// The new score to apply when this rule hits
    #[clap(short, long, allow_negative_numbers = true)]
    pub score: Option<i64>,
    /// A list of groups to add this rule to
    #[clap(long, value_delimiter = ',', value_parser = trim_arg)]
    pub add_groups: Vec<String>,
    /// A list of groups to remove this rule from
    #[clap(long, value_delimiter = ',', value_parser = trim_arg)]
    pub remove_groups: Vec<String>,
    /// The new name to set for this rule
    #[clap(short, long)]
    pub name: Option<String>,
    /// The new description to set for this rule
    #[clap(short, long, conflicts_with = "clear_description")]
    pub description: Option<String>,
    /// Clear this rule's description
    #[clap(long)]
    pub clear_description: bool,
    /// The kinds of data this rule should also apply to
    #[clap(long, value_enum, ignore_case = true, value_delimiter = ',')]
    pub add_applies_to: Vec<RuleAppliesTo>,
    /// The kinds of data this rule should no longer apply to
    #[clap(long, value_enum, ignore_case = true, value_delimiter = ',')]
    pub remove_applies_to: Vec<RuleAppliesTo>,
}

/// A command to delete detection rules from Thorium
#[derive(Parser, Debug, Clone)]
pub struct DeleteRules {
    /// The ids or names of the rules to delete
    #[clap(required = true)]
    pub rules: Vec<String>,
    /// Skip the confirmation prompt before deleting
    #[clap(short = 'y', long)]
    pub skip_confirm: bool,
}

/// A command to bulk import rule files into Thorium
#[derive(Parser, Debug, Clone)]
pub struct ImportRules {
    /// The rule files and/or directories to import
    ///
    /// Directories are walked recursively and each file's engine is inferred
    /// from its extension (.yar/.yara for YARA, .yml/.yaml for Sigma) unless
    /// `--engine` forces one; other files are skipped and counted.
    #[clap(required = true)]
    pub targets: Vec<PathBuf>,
    /// The groups to import these rules into
    #[clap(short, long, required = true, value_delimiter = ',', value_parser = trim_arg)]
    pub groups: Vec<String>,
    /// Force every file to be imported as this engine instead of inferring
    /// engines from file extensions
    #[clap(short, long, value_enum, ignore_case = true)]
    pub engine: Option<RuleEngine>,
    /// Any regular expressions to use to determine which files to import
    ///
    /// Regular expressions follow the syntax for the Rust regex crate:
    /// <https://docs.rs/regex/latest/regex/#syntax>
    #[clap(short, long)]
    pub filter: Vec<String>,
    /// Any regular expressions to use to determine which files to skip
    ///
    /// Regular expressions follow the syntax for the Rust regex crate:
    /// <https://docs.rs/regex/latest/regex/#syntax>
    #[clap(short, long)]
    pub skip: Vec<String>,
    /// Include hidden directories/files
    ///
    /// Note that if a given target is itself hidden, hidden files/directories
    /// within that target will be included.
    #[clap(long)]
    pub include_hidden: bool,
    /// Validate rule files locally and show what would be imported without
    /// creating anything in Thorium
    #[clap(long)]
    pub dry_run: bool,
    /// Write a JSON import report ([{file, name, status, error}]) to this path
    #[clap(long)]
    pub report: Option<PathBuf>,
}

/// The rule notification specific subcommands
#[derive(Parser, Debug, Clone)]
pub enum RuleNotifications {
    /// Get a detection rule's notifications (e.g. quarantine reasons)
    #[clap(version, author)]
    Get(GetRuleNotifications),
    /// Create a detection rule notification (admins only)
    #[clap(version, author)]
    Create(CreateRuleNotification),
    /// Delete a detection rule notification (admins only)
    #[clap(version, author)]
    Delete(DeleteRuleNotification),
}

/// A command to get a detection rule's notifications
#[derive(Parser, Debug, Clone)]
pub struct GetRuleNotifications {
    /// The id or name of the rule to get notifications for
    pub rule: String,
    /// The options for getting notifications
    #[clap(flatten)]
    pub opts: GetNotificationOpts,
}

/// A command to create a detection rule notification
#[derive(Parser, Debug, Clone)]
pub struct CreateRuleNotification {
    /// The id or name of the rule to create a notification for
    pub rule: String,
    /// The params needed when creating a notification
    #[clap(flatten)]
    pub notification: CreateNotification,
}

/// A command to delete a detection rule notification
#[derive(Parser, Debug, Clone)]
pub struct DeleteRuleNotification {
    /// The id or name of the rule whose notification should be deleted
    pub rule: String,
    /// The notification's unique ID
    pub notification_id: Uuid,
}

/// A command to edit a detection rule's source in an editor
///
/// The rule opens as a `.yar` buffer for YARA rules and a `.yml` buffer for
/// Sigma rules; saving with no changes is a no-op and server-side compile
/// errors reopen the editor with the edited text so work is never lost.
#[derive(Parser, Debug, Clone)]
pub struct EditRule {
    /// The id or name of the rule to edit
    pub rule: String,
    /// The editor to use when editing the rule
    #[clap(short, long, help = editor_help("rule"))]
    pub editor: Option<String>,
}

/// A command to re-admit a quarantined rule to scanning
///
/// This deletes the rule's `YaraRuleStatus` tag so the next scanner context
/// rebuild picks the rule back up; it does not protect the rule from being
/// quarantined again (see `rules exempt` for that).
#[derive(Parser, Debug, Clone)]
pub struct UnquarantineRule {
    /// The id or name of the rule to re-admit
    pub rule: String,
}

/// A command to manage a rule's quarantine exemption (admins only)
///
/// Setting an exemption also clears any Noisy/Slow quarantine status so the
/// rule rejoins scanning in the same action; Broken tagging is unaffected
/// since a rule that no longer compiles cannot scan regardless.
#[derive(Parser, Debug, Clone)]
pub struct ExemptRule {
    /// The id or name of the rule to exempt from quarantine
    pub rule: String,
    /// Clear this rule's exemption instead of setting it
    #[clap(long)]
    pub clear: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::{Args, SubCommands};
    use clap::CommandFactory;

    /// A valid rule entity uuid for tests
    const UUID: &str = "bf5b4ea2-7a43-4a69-8c41-1f3b2c6e5d8a";

    /// Parse a rules command
    ///
    /// # Arguments
    ///
    /// * `args` - The rules subcommand args to parse
    fn parse(args: &[&str]) -> Result<Rules, clap::Error> {
        // add our binary and subcommand names
        let full = ["thorctl", "rules"].iter().chain(args.iter());
        match Args::try_parse_from(full)?.cmd {
            SubCommands::Rules(cmd) => Ok(cmd),
            _ => unreachable!("we always parse a rules command"),
        }
    }

    /// The rules command tree must pass clap's own consistency checks
    #[test]
    fn debug_assert() {
        Rules::command().debug_assert();
    }

    /// Create must parse a full yara rule command
    #[test]
    fn create_yara() {
        let cmd = parse(&[
            "create",
            "-n",
            "demo_downloader_c2",
            "-e",
            "yara",
            "-f",
            "rules/demo.yar",
            "-g",
            "a,b",
            "-s",
            "40",
            "-T",
            "Family=Demo",
            "-T",
            "Source=Corpus",
            "-d",
            "a demo rule",
        ])
        .unwrap();
        let Rules::Create(cmd) = cmd else {
            panic!("expected create command")
        };
        assert_eq!(cmd.name, "demo_downloader_c2");
        assert_eq!(cmd.engine, RuleEngine::Yara);
        assert_eq!(cmd.rule_file, PathBuf::from("rules/demo.yar"));
        assert_eq!(cmd.groups, vec!["a", "b"]);
        assert_eq!(cmd.score, Some(40));
        assert_eq!(cmd.tags, vec!["Family=Demo", "Source=Corpus"]);
        assert_eq!(cmd.description.as_deref(), Some("a demo rule"));
        assert!(cmd.applies_to.is_empty());
    }

    /// Create must parse a sigma rule command
    #[test]
    fn create_sigma() {
        let cmd = parse(&[
            "create",
            "-n",
            "demo_susp_process_spawn",
            "--engine",
            "sigma",
            "-f",
            "susp.yml",
            "-g",
            "demolab",
            "--applies-to",
            "windows-processes,network-connections",
        ])
        .unwrap();
        let Rules::Create(cmd) = cmd else {
            panic!("expected create command")
        };
        assert_eq!(cmd.engine, RuleEngine::Sigma);
        assert_eq!(
            cmd.applies_to,
            vec![
                RuleAppliesTo::WindowsProcesses,
                RuleAppliesTo::NetworkConnections
            ]
        );
        // no score was given so the server derives one from the rule's level
        assert_eq!(cmd.score, None);
    }

    /// Get and list must parse their targets and output flags
    #[test]
    fn get_and_list() {
        // get parses its id and output flags
        let cmd = parse(&["get", UUID, "-c"]).unwrap();
        let Rules::Get(cmd) = cmd else {
            panic!("expected get command")
        };
        assert_eq!(cmd.rule, UUID);
        assert!(cmd.condensed);
        // list parses its filters and limits
        let cmd = parse(&["list", "-e", "sigma", "--no-limit", "-t", "Family=Demo"]).unwrap();
        let Rules::List(cmd) = cmd else {
            panic!("expected list command")
        };
        assert_eq!(cmd.engine, Some(RuleEngine::Sigma));
        assert!(cmd.no_limit);
        assert_eq!(cmd.tags, vec!["Family=Demo"]);
    }

    /// Update must parse every updatable field
    #[test]
    fn update() {
        let cmd = parse(&[
            "update",
            UUID,
            "--add-groups",
            "a,b",
            "--score",
            "10",
            "--add-applies-to",
            "windows-processes",
        ])
        .unwrap();
        let Rules::Update(cmd) = cmd else {
            panic!("expected update command")
        };
        assert_eq!(cmd.rule, UUID);
        assert_eq!(cmd.opts.add_groups, vec!["a", "b"]);
        assert_eq!(cmd.opts.score, Some(10));
        assert_eq!(
            cmd.opts.add_applies_to,
            vec![RuleAppliesTo::WindowsProcesses]
        );
    }

    /// Delete must parse multiple targets and the confirm skip
    #[test]
    fn delete() {
        let cmd = parse(&["delete", UUID, UUID, "-y"]).unwrap();
        let Rules::Delete(cmd) = cmd else {
            panic!("expected delete command")
        };
        assert_eq!(cmd.rules.len(), 2);
        assert!(cmd.skip_confirm);
    }

    /// Import must parse its targets, groups, and filters
    #[test]
    fn import() {
        let cmd = parse(&[
            "import",
            "rules/",
            "extra.yar",
            "-g",
            "demolab",
            "-e",
            "sigma",
            "--filter",
            r"\.yml$",
            "--skip",
            "^draft",
            "--include-hidden",
            "--dry-run",
            "--report",
            "/tmp/report.json",
        ])
        .unwrap();
        let Rules::Import(cmd) = cmd else {
            panic!("expected import command")
        };
        assert_eq!(
            cmd.targets,
            vec![PathBuf::from("rules/"), PathBuf::from("extra.yar")]
        );
        assert_eq!(cmd.groups, vec!["demolab"]);
        assert_eq!(cmd.engine, Some(RuleEngine::Sigma));
        assert_eq!(cmd.filter, vec![r"\.yml$"]);
        assert_eq!(cmd.skip, vec!["^draft"]);
        assert!(cmd.include_hidden);
        assert!(cmd.dry_run);
        assert_eq!(cmd.report, Some(PathBuf::from("/tmp/report.json")));
    }

    /// Notifications get must parse its rule target
    #[test]
    fn notifications_get() {
        let cmd = parse(&["notifications", "get", UUID, "--ids"]).unwrap();
        let Rules::Notifications(RuleNotifications::Get(cmd)) = cmd else {
            panic!("expected notifications get command")
        };
        assert_eq!(cmd.rule, UUID);
        assert!(cmd.opts.ids);
    }

    /// Notifications create must parse its level and message
    #[test]
    fn notifications_create() {
        let cmd = parse(&[
            "notifications",
            "create",
            UUID,
            "--msg",
            "scheduled for retirement",
            "--level",
            "warn",
            "--expire",
            "false",
        ])
        .unwrap();
        let Rules::Notifications(RuleNotifications::Create(cmd)) = cmd else {
            panic!("expected notifications create command")
        };
        assert_eq!(cmd.rule, UUID);
        assert_eq!(cmd.notification.msg, "scheduled for retirement");
        assert_eq!(cmd.notification.expire, Some(false));
    }

    /// Notifications delete must parse its notification id
    #[test]
    fn notifications_delete() {
        let cmd = parse(&["notifications", "delete", UUID, UUID]).unwrap();
        let Rules::Notifications(RuleNotifications::Delete(cmd)) = cmd else {
            panic!("expected notifications delete command")
        };
        assert_eq!(cmd.rule, UUID);
        assert_eq!(cmd.notification_id, Uuid::parse_str(UUID).unwrap());
    }

    /// Edit must parse its target and editor override
    #[test]
    fn edit() {
        // edit parses its id with no editor override
        let cmd = parse(&["edit", UUID]).unwrap();
        let Rules::Edit(cmd) = cmd else {
            panic!("expected edit command")
        };
        assert_eq!(cmd.rule, UUID);
        assert_eq!(cmd.editor, None);
        // edit parses an explicit editor override
        let cmd = parse(&["edit", UUID, "--editor", "hx"]).unwrap();
        let Rules::Edit(cmd) = cmd else {
            panic!("expected edit command")
        };
        assert_eq!(cmd.editor.as_deref(), Some("hx"));
    }

    /// Unquarantine must parse its rule target
    #[test]
    fn unquarantine() {
        let cmd = parse(&["unquarantine", UUID]).unwrap();
        let Rules::Unquarantine(cmd) = cmd else {
            panic!("expected unquarantine command")
        };
        assert_eq!(cmd.rule, UUID);
    }

    /// Exempt must parse its set and clear forms
    #[test]
    fn exempt() {
        // exempt parses its id and defaults to setting the exemption
        let cmd = parse(&["exempt", UUID]).unwrap();
        let Rules::Exempt(cmd) = cmd else {
            panic!("expected exempt command")
        };
        assert_eq!(cmd.rule, UUID);
        assert!(!cmd.clear);
        // exempt parses the clear flag
        let cmd = parse(&["exempt", UUID, "--clear"]).unwrap();
        let Rules::Exempt(cmd) = cmd else {
            panic!("expected exempt command")
        };
        assert!(cmd.clear);
    }

    /// Invalid command shapes must be rejected with parse errors
    #[test]
    fn rejects_invalid() {
        // create missing the engine
        assert!(parse(&["create", "-n", "x", "-f", "a.yar", "-g", "a"]).is_err());
        // create with an unknown engine
        assert!(parse(&["create", "-n", "x", "-e", "snort", "-f", "a.yar", "-g", "a"]).is_err());
        // create missing its groups
        assert!(parse(&["create", "-n", "x", "-e", "yara", "-f", "a.yar"]).is_err());
        // create with a bad applies to value
        assert!(
            parse(&[
                "create",
                "-n",
                "x",
                "-e",
                "yara",
                "-f",
                "a.yar",
                "-g",
                "a",
                "--applies-to",
                "nope"
            ])
            .is_err()
        );
        // update with no update flags set
        assert!(parse(&["update", UUID]).is_err());
        // update cannot set and clear the description at once
        assert!(parse(&["update", UUID, "-d", "x", "--clear-description"]).is_err());
        // delete with no targets
        assert!(parse(&["delete"]).is_err());
        // import with no groups
        assert!(parse(&["import", "rules/"]).is_err());
        // import with no targets
        assert!(parse(&["import", "-g", "a"]).is_err());
        // list cannot mix a limit with no limit
        assert!(parse(&["list", "-l", "5", "--no-limit"]).is_err());
        // the old flat notifications shape is gone: a subcommand is required
        assert!(parse(&["notifications", UUID]).is_err());
        // notifications create requires a message and level
        assert!(parse(&["notifications", "create", UUID]).is_err());
        // notifications delete requires the notification id
        assert!(parse(&["notifications", "delete", UUID]).is_err());
        // notifications delete still requires a real uuid for the notification
        assert!(parse(&["notifications", "delete", UUID, "nope"]).is_err());
        // unquarantine requires a target
        assert!(parse(&["unquarantine"]).is_err());
    }

    /// Rule targets must parse as names as well as ids
    #[test]
    fn accepts_names() {
        // get accepts a rule name target
        let cmd = parse(&["get", "demo_rule"]).unwrap();
        let Rules::Get(cmd) = cmd else {
            panic!("expected get command")
        };
        assert_eq!(cmd.rule, "demo_rule");
        // update accepts a rule name target
        let cmd = parse(&["update", "demo_rule", "--score", "1"]).unwrap();
        let Rules::Update(cmd) = cmd else {
            panic!("expected update command")
        };
        assert_eq!(cmd.rule, "demo_rule");
        // delete accepts several rule name targets
        let cmd = parse(&["delete", "demo_a", "demo_b"]).unwrap();
        let Rules::Delete(cmd) = cmd else {
            panic!("expected delete command")
        };
        assert_eq!(cmd.rules, vec!["demo_a", "demo_b"]);
        // edit accepts a rule name target
        let cmd = parse(&["edit", "demo_rule"]).unwrap();
        let Rules::Edit(cmd) = cmd else {
            panic!("expected edit command")
        };
        assert_eq!(cmd.rule, "demo_rule");
        // unquarantine accepts a rule name target
        let cmd = parse(&["unquarantine", "demo_rule"]).unwrap();
        let Rules::Unquarantine(cmd) = cmd else {
            panic!("expected unquarantine command")
        };
        assert_eq!(cmd.rule, "demo_rule");
        // exempt accepts a rule name target
        let cmd = parse(&["exempt", "demo_rule"]).unwrap();
        let Rules::Exempt(cmd) = cmd else {
            panic!("expected exempt command")
        };
        assert_eq!(cmd.rule, "demo_rule");
        // notifications get accepts a rule name target
        let cmd = parse(&["notifications", "get", "demo_rule"]).unwrap();
        let Rules::Notifications(RuleNotifications::Get(cmd)) = cmd else {
            panic!("expected notifications get command")
        };
        assert_eq!(cmd.rule, "demo_rule");
    }

    /// Applies-to values must map onto the model enum
    #[test]
    fn applies_to_maps() {
        // samples maps to the yara variant and is rejected for sigma
        assert!(RuleAppliesTo::Samples.to_yara().is_ok());
        assert!(RuleAppliesTo::Samples.to_sigma().is_err());
        // windows processes maps to the sigma variant and is rejected for yara
        assert!(RuleAppliesTo::WindowsProcesses.to_sigma().is_ok());
        assert!(RuleAppliesTo::WindowsProcesses.to_yara().is_err());
        // every sigma value maps cleanly
        assert!(RuleAppliesTo::NetworkConnections.to_sigma().is_ok());
        assert!(RuleAppliesTo::CompiledFunctions.to_sigma().is_ok());
        assert!(RuleAppliesTo::DecompiledFunctions.to_sigma().is_ok());
    }

    /// Tags must parse as KEY=VALUE with '=' allowed in values
    #[test]
    fn tag_parsing() {
        // a simple tag splits into its key and value
        assert_eq!(
            parse_tag("Family=Demo").unwrap(),
            ("Family".to_owned(), "Demo".to_owned())
        );
        // values keep any extra '='
        assert_eq!(
            parse_tag("k=a=b").unwrap(),
            ("k".to_owned(), "a=b".to_owned())
        );
        // tags without a '=' or with empty parts are rejected
        assert!(parse_tag("NoEquals").is_err());
        assert!(parse_tag("=value").is_err());
        assert!(parse_tag("key=").is_err());
    }

    /// Scores are i64s so the natural `--score -10` spelling must parse
    #[test]
    fn negative_scores() {
        // a negative score must parse on create without `=` spelling tricks
        let cmd = parse(&[
            "create", "-n", "x", "-e", "yara", "-f", "a.yar", "-g", "corn", "--score", "-10",
        ])
        .expect("negative create score failed to parse");
        if let Rules::Create(create) = cmd {
            assert_eq!(create.score, Some(-10));
        } else {
            panic!("expected a create command");
        }
        // a negative score must parse on update too
        let cmd =
            parse(&["update", "demo", "--score", "-5"]).expect("negative update score failed");
        if let Rules::Update(update) = cmd {
            assert_eq!(update.opts.score, Some(-5));
        } else {
            panic!("expected an update command");
        }
    }

    /// Comma separated list values must trim stray whitespace and reject empties
    #[test]
    fn list_values_trim() {
        // a spaced comma list must trim each value
        let cmd = parse(&[
            "create",
            "-n",
            "x",
            "-e",
            "yara",
            "-f",
            "a.yar",
            "-g",
            "corn, beans",
        ])
        .expect("spaced group list failed to parse");
        if let Rules::Create(create) = cmd {
            assert_eq!(create.groups, vec!["corn".to_owned(), "beans".to_owned()]);
        } else {
            panic!("expected a create command");
        }
        // empty values from trailing commas are rejected
        assert!(
            parse(&[
                "create", "-n", "x", "-e", "yara", "-f", "a.yar", "-g", "corn,"
            ])
            .is_err()
        );
    }
}
