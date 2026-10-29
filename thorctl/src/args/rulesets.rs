//! Arguments for rule set related Thorctl commands

#![allow(clippy::module_name_repetitions)]

use clap::Parser;
use std::path::PathBuf;
use uuid::Uuid;

use super::rules::{parse_tag, trim_arg};

/// The commands to send to the rule sets task handler
#[derive(Parser, Debug)]
pub enum Rulesets {
    /// Create a rule set in Thorium
    #[clap(version, author)]
    Create(CreateRuleSet),
    /// Get a rule set's details, including its member rules (human readable
    /// by default; pass --json for JSON)
    #[clap(version, author)]
    Get(GetRuleSet),
    /// List the rule sets you can view
    #[clap(version, author)]
    List(ListRuleSets),
    /// Update a rule set, including adding or removing member rules
    #[clap(version, author)]
    Update(UpdateRuleSet),
    /// Delete one or more rule sets from Thorium
    ///
    /// Deleting a rule set does not affect existing hunts: hunts freeze a
    /// snapshot of their rules at creation.
    #[clap(version, author)]
    Delete(DeleteRuleSets),
}

/// Create a rule set in Thorium
#[derive(Parser, Debug, Clone)]
pub struct CreateRuleSet {
    /// The name of the rule set to create
    #[clap(short, long)]
    pub name: String,
    /// The groups to create this rule set in
    #[clap(short, long, required = true, value_delimiter = ',', value_parser = trim_arg)]
    pub groups: Vec<String>,
    /// The ids or names of the rule entities to add as members of this rule set
    ///
    /// Members may be rules of any engine (e.g. YARA or Sigma); hunts skip
    /// and count members their engine cannot scan with.
    #[clap(short, long, value_delimiter = ',', value_parser = trim_arg)]
    pub rules: Vec<String>,
    /// The tags to set on this rule set, formatted as `KEY=VALUE` (repeatable)
    #[clap(short = 'T', long = "tags", value_parser = parse_tag, value_name = "KEY=VALUE")]
    pub tags: Vec<(String, String)>,
    /// A description for this rule set
    #[clap(short, long)]
    pub description: Option<String>,
}

/// Get a rule set's details from Thorium
#[derive(Parser, Debug, Clone)]
pub struct GetRuleSet {
    /// The id or name of the rule set to get
    pub ruleset: String,
    /// Print this rule set as JSON instead of the human-readable summary
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

/// List the rule sets in Thorium
#[derive(Parser, Debug, Clone)]
pub struct ListRuleSets {
    /// Any groups to filter by when listing rule sets
    ///
    /// If no groups are given, the search will include all groups you are a part of
    #[clap(short, long, value_delimiter = ',', value_parser = trim_arg)]
    pub groups: Vec<String>,
    /// Only list rule sets with these tags, formatted as `KEY=VALUE` (repeatable)
    #[clap(short = 't', long = "tags", value_parser = parse_tag, value_name = "KEY=VALUE")]
    pub tags: Vec<(String, String)>,
    /// The cursor to continue a search with
    #[clap(long)]
    pub cursor: Option<Uuid>,
    /// The max number of total rule sets to find in the search
    #[clap(short, long, default_value_t = 50)]
    pub limit: usize,
    /// Refrain from setting a limit when listing rule sets
    #[clap(long, conflicts_with = "limit")]
    pub no_limit: bool,
    /// The number of rule sets to find in one request
    #[clap(short, long, default_value_t = 50)]
    pub page_size: usize,
    /// Print the rule sets as JSON instead of a table
    #[clap(long)]
    pub json: bool,
    /// Print condensed JSON (implies `--json`)
    #[clap(short, long)]
    pub condensed: bool,
    /// Write JSON to a file instead of stdout (implies `--json`)
    #[clap(short, long)]
    pub output: Option<PathBuf>,
}

/// Update a rule set in Thorium
#[derive(Parser, Debug, Clone)]
pub struct UpdateRuleSet {
    /// The id or name of the rule set to update
    pub ruleset: String,
    /// The updates to apply to this rule set
    #[clap(flatten)]
    pub opts: RuleSetUpdateOpts,
}

/// The set of possible updates to a rule set where at least one must be set
#[derive(clap::Args, Debug, Clone)]
#[group(required = true, multiple = true)]
pub struct RuleSetUpdateOpts {
    /// The ids or names of the rule entities to add as members of this rule set
    #[clap(long, value_delimiter = ',', value_parser = trim_arg)]
    pub add_rules: Vec<String>,
    /// The ids or names of the rule entities to remove from this rule set
    ///
    /// Removing a rule does not affect existing hunts: hunts freeze a
    /// snapshot of their rules at creation.
    #[clap(long, value_delimiter = ',', value_parser = trim_arg)]
    pub remove_rules: Vec<String>,
    /// The new name to set for this rule set
    #[clap(long)]
    pub name: Option<String>,
    /// A list of groups to add this rule set to
    #[clap(long, value_delimiter = ',', value_parser = trim_arg)]
    pub add_groups: Vec<String>,
    /// A list of groups to remove this rule set from
    #[clap(long, value_delimiter = ',', value_parser = trim_arg)]
    pub remove_groups: Vec<String>,
    /// The new description to set for this rule set
    #[clap(long)]
    pub description: Option<String>,
    /// Clear this rule set's description
    #[clap(long, conflicts_with = "description")]
    pub clear_description: bool,
}

/// Delete one or more rule sets from Thorium
#[derive(Parser, Debug, Clone)]
pub struct DeleteRuleSets {
    /// The ids or names of the rule sets to delete
    ///
    /// Deleting a rule set does not affect existing hunts: hunts freeze a
    /// snapshot of their rules at creation.
    #[clap(required = true)]
    pub rulesets: Vec<String>,
    /// Skip the confirmation prompt before deleting
    #[clap(short = 'y', long)]
    pub skip_confirm: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::{Args, SubCommands};
    use clap::CommandFactory;

    /// A valid uuid for rule set targets in tests
    const UUID: &str = "bf5b4ea2-7a43-4a69-8c41-1f3b2c6e5d8a";
    /// A second valid uuid for member rules in tests
    const UUID2: &str = "0a632e3a-3c90-45a8-9b2f-6f6dc44f8af0";

    /// Parse a rulesets command
    ///
    /// # Arguments
    ///
    /// * `args` - The command line args to parse
    fn parse(args: &[&str]) -> Result<Rulesets, clap::Error> {
        // add our binary and subcommand names
        let full = ["thorctl", "rulesets"].iter().chain(args.iter());
        match Args::try_parse_from(full)?.cmd {
            SubCommands::Rulesets(cmd) => Ok(cmd),
            _ => unreachable!("we always parse a rulesets command"),
        }
    }

    /// Make sure our command tree is valid
    #[test]
    fn debug_assert() {
        Rulesets::command().debug_assert();
    }

    /// Make sure a fully loaded create command parses
    #[test]
    fn create() {
        // parse a create command with every flag set
        let cmd = parse(&[
            "create",
            "-n",
            "demo-triage-rules",
            "-g",
            "demolab,corn",
            "-r",
            &format!("{UUID},{UUID2}"),
            "-T",
            "team=triage",
            "-T",
            "env=prod",
            "-d",
            "rules for triage",
        ])
        .unwrap();
        // make sure we parsed a create command
        let Rulesets::Create(cmd) = cmd else {
            panic!("expected create command")
        };
        // check every parsed flag
        assert_eq!(cmd.name, "demo-triage-rules");
        assert_eq!(cmd.groups, vec!["demolab", "corn"]);
        // member targets stay raw strings so the handler can resolve names
        assert_eq!(cmd.rules, vec![UUID, UUID2]);
        assert_eq!(
            cmd.tags,
            vec![
                ("team".to_owned(), "triage".to_owned()),
                ("env".to_owned(), "prod".to_owned()),
            ]
        );
        assert_eq!(cmd.description.as_deref(), Some("rules for triage"));
    }

    /// Make sure a rule set with no members can be created
    #[test]
    fn create_minimal() {
        // parse a create command with only the required flags
        let cmd = parse(&["create", "-n", "empty", "-g", "demolab"]).unwrap();
        // make sure we parsed a create command
        let Rulesets::Create(cmd) = cmd else {
            panic!("expected create command")
        };
        // check that all optional flags default to empty
        assert!(cmd.rules.is_empty());
        assert!(cmd.tags.is_empty());
        assert!(cmd.description.is_none());
    }

    /// Make sure a get command parses
    #[test]
    fn get() {
        // parse a get command with output flags set
        let cmd = parse(&["get", UUID, "-c", "-o", "out.json"]).unwrap();
        // make sure we parsed a get command
        let Rulesets::Get(cmd) = cmd else {
            panic!("expected get command")
        };
        // check every parsed flag
        assert_eq!(cmd.ruleset, UUID);
        assert!(cmd.condensed);
        assert_eq!(
            cmd.output.as_deref(),
            Some(std::path::Path::new("out.json"))
        );
        // get defaults to the human readable summary with --json opting out
        let cmd = parse(&["get", UUID, "--json"]).unwrap();
        let Rulesets::Get(cmd) = cmd else {
            panic!("expected get command")
        };
        assert!(cmd.json);
        assert!(!cmd.condensed);
    }

    /// Make sure list defaults are sane
    #[test]
    fn list_defaults() {
        // parse a bare list command
        let cmd = parse(&["list"]).unwrap();
        // make sure we parsed a list command
        let Rulesets::List(cmd) = cmd else {
            panic!("expected list command")
        };
        // check our list defaults
        assert_eq!(cmd.limit, 50);
        assert_eq!(cmd.page_size, 50);
        assert!(!cmd.no_limit);
        assert!(cmd.groups.is_empty());
        assert!(cmd.tags.is_empty());
        assert!(cmd.cursor.is_none());
        assert!(!cmd.json);
        assert!(!cmd.condensed);
        assert!(cmd.output.is_none());
    }

    /// Make sure a fully loaded list command parses
    #[test]
    fn list_flags() {
        // parse a list command with every flag set
        let cmd = parse(&[
            "list",
            "-g",
            "demolab,corn",
            "-t",
            "team=triage",
            "--cursor",
            UUID,
            "--no-limit",
            "--json",
            "-p",
            "100",
        ])
        .unwrap();
        // make sure we parsed a list command
        let Rulesets::List(cmd) = cmd else {
            panic!("expected list command")
        };
        // check every parsed flag
        assert_eq!(cmd.groups, vec!["demolab", "corn"]);
        assert_eq!(cmd.tags, vec![("team".to_owned(), "triage".to_owned())]);
        assert_eq!(cmd.cursor, Some(Uuid::parse_str(UUID).unwrap()));
        assert!(cmd.no_limit);
        assert!(cmd.json);
        assert_eq!(cmd.page_size, 100);
    }

    /// Make sure a fully loaded update command parses
    #[test]
    fn update() {
        // parse an update command with every flag set except clear description
        let cmd = parse(&[
            "update",
            UUID,
            "--add-rules",
            &format!("{UUID2},{UUID}"),
            "--remove-rules",
            UUID2,
            "--name",
            "renamed",
            "--add-groups",
            "a,b",
            "--remove-groups",
            "c",
            "--description",
            "new description",
        ])
        .unwrap();
        // make sure we parsed an update command
        let Rulesets::Update(cmd) = cmd else {
            panic!("expected update command")
        };
        // check every parsed flag
        assert_eq!(cmd.ruleset, UUID);
        // member targets stay raw strings so the handler can resolve names
        assert_eq!(cmd.opts.add_rules, vec![UUID2, UUID]);
        assert_eq!(cmd.opts.remove_rules, vec![UUID2]);
        assert_eq!(cmd.opts.name.as_deref(), Some("renamed"));
        assert_eq!(cmd.opts.add_groups, vec!["a", "b"]);
        assert_eq!(cmd.opts.remove_groups, vec!["c"]);
        assert_eq!(cmd.opts.description.as_deref(), Some("new description"));
        assert!(!cmd.opts.clear_description);
    }

    /// Make sure a single update flag satisfies the required update group
    #[test]
    fn update_single_flag() {
        // parse an update command with only one update flag set
        let cmd = parse(&["update", UUID, "--clear-description"]).unwrap();
        // make sure we parsed an update command
        let Rulesets::Update(cmd) = cmd else {
            panic!("expected update command")
        };
        // check that only the clear description flag is set
        assert!(cmd.opts.clear_description);
        assert!(cmd.opts.add_rules.is_empty());
        assert!(cmd.opts.name.is_none());
    }

    /// Make sure a delete command parses
    #[test]
    fn delete() {
        // parse a delete command with multiple targets
        let cmd = parse(&["delete", UUID, UUID2, "-y"]).unwrap();
        // make sure we parsed a delete command
        let Rulesets::Delete(cmd) = cmd else {
            panic!("expected delete command")
        };
        // check every parsed flag
        assert_eq!(cmd.rulesets, vec![UUID, UUID2]);
        assert!(cmd.skip_confirm);
    }

    /// Make sure invalid commands are rejected
    #[test]
    fn rejects_invalid() {
        // create without a name
        assert!(parse(&["create", "-g", "demolab"]).is_err());
        // create without groups
        assert!(parse(&["create", "-n", "x"]).is_err());
        // create with malformed tags
        assert!(parse(&["create", "-n", "x", "-g", "a", "-T", "noequals"]).is_err());
        assert!(parse(&["create", "-n", "x", "-g", "a", "-T", "=value"]).is_err());
        assert!(parse(&["create", "-n", "x", "-g", "a", "-T", "key="]).is_err());
        // list with a limit and no limit
        assert!(parse(&["list", "-l", "5", "--no-limit"]).is_err());
        // update without any update flag
        assert!(parse(&["update", UUID]).is_err());
        // update without a target
        assert!(parse(&["update", "--name", "x"]).is_err());
        // update with a description and clear description
        assert!(parse(&["update", UUID, "--description", "x", "--clear-description"]).is_err());
        // delete without any targets
        assert!(parse(&["delete"]).is_err());
    }

    /// Rule set and member targets must parse as names as well as ids
    #[test]
    fn accepts_names() {
        // create accepts member rule names
        let cmd = parse(&["create", "-n", "x", "-g", "a", "-r", "demo_a,demo_b"]).unwrap();
        let Rulesets::Create(cmd) = cmd else {
            panic!("expected create command")
        };
        assert_eq!(cmd.rules, vec!["demo_a", "demo_b"]);
        // get accepts a rule set name target
        let cmd = parse(&["get", "demo-pack"]).unwrap();
        let Rulesets::Get(cmd) = cmd else {
            panic!("expected get command")
        };
        assert_eq!(cmd.ruleset, "demo-pack");
        // update accepts a rule set name target and member rule names
        let cmd = parse(&["update", "demo-pack", "--add-rules", "demo_a"]).unwrap();
        let Rulesets::Update(cmd) = cmd else {
            panic!("expected update command")
        };
        assert_eq!(cmd.ruleset, "demo-pack");
        assert_eq!(cmd.opts.add_rules, vec!["demo_a"]);
        // delete accepts rule set name targets
        let cmd = parse(&["delete", "demo-pack", "other-pack"]).unwrap();
        let Rulesets::Delete(cmd) = cmd else {
            panic!("expected delete command")
        };
        assert_eq!(cmd.rulesets, vec!["demo-pack", "other-pack"]);
    }

    /// Make sure tags split on only the first equals sign
    #[test]
    fn tag_values_keep_equals() {
        // a value containing '=' passes through intact
        assert_eq!(
            parse_tag("key=a=b").unwrap(),
            ("key".to_owned(), "a=b".to_owned())
        );
        // tags without a separator, key, or value are rejected
        assert!(parse_tag("noequals").is_err());
        assert!(parse_tag("=value").is_err());
        assert!(parse_tag("key=").is_err());
    }
}
