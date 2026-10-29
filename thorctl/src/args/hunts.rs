//! Arguments for hunt-related Thorctl commands

#![allow(clippy::module_name_repetitions)]

use chrono::{DateTime, NaiveDateTime, Utc};
use clap::Parser;
use std::collections::HashMap;
use std::path::PathBuf;
use thorium::Error;
use thorium::models::{
    HuntCorpusBoundsRequest, HuntCorpusRequest, HuntCreateRequest, HuntEngine, HuntSelector,
};
use uuid::Uuid;

use super::rules::{parse_tag, trim_arg};

/// The commands to send to the hunts task handler
#[derive(Parser, Debug)]
pub enum Hunts {
    /// Launch a hunt: a bounded, paced pass of a frozen set of rules over historical files
    #[clap(version, author)]
    Create(CreateHunt),
    /// Show a hunt's status, per-lane progress, and scan counters
    #[clap(version, author)]
    Status(StatusHunt),
    /// List hunts
    #[clap(version, author)]
    List(ListHunts),
    /// List the hits a hunt has written
    #[clap(version, author)]
    Hits(HuntHits),
    /// List the frozen rules a hunt scans with
    #[clap(version, author)]
    Rules(HuntRules),
    /// Cancel a hunt
    ///
    /// Only the hunt's submitter or an admin can cancel a hunt. Running chunks
    /// stop within one chunk and the hunt cannot be resumed.
    #[clap(version, author)]
    Cancel(CancelHunt),
    /// Delete hunts
    ///
    /// Deleting a hunt removes only the hunt entity itself; the hits it wrote
    /// and their HuntId tags are not deleted with the hunt.
    #[clap(version, author)]
    Delete(DeleteHunts),
}

/// Parse a datetime in the format `%Y-%m-%dT%H:%M:%S`, interpreted as UTC
///
/// # Arguments
///
/// * `raw` - The raw datetime to parse
fn parse_datetime(raw: &str) -> Result<DateTime<Utc>, Error> {
    // parse this datetime with the same format the other search commands use
    match NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S") {
        Ok(naive) => Ok(naive.and_utc()),
        Err(err) => Err(Error::new(format!(
            "'{raw}' is not a valid datetime (expected the UTC format \
            %Y-%m-%dT%H:%M:%S, e.g. 2026-10-20T14:30:00): {err}"
        ))),
    }
}

/// The rule engines a hunt can run
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum HuntEngineArg {
    /// Scan sample bytes with yara rules
    #[default]
    Yara,
}

impl From<HuntEngineArg> for HuntEngine {
    /// Convert an engine arg into the wire [`HuntEngine`]
    ///
    /// # Arguments
    ///
    /// * `arg` - The engine arg to convert
    fn from(arg: HuntEngineArg) -> Self {
        match arg {
            HuntEngineArg::Yara => HuntEngine::Yara,
        }
    }
}

/// How a hunt chooses the rules it freezes (set exactly one)
#[derive(clap::Args, Debug, Clone)]
#[group(required = true, multiple = false)]
pub struct SelectorArgs {
    /// An explicit list of rule entity ids or names to freeze
    #[clap(short, long, value_delimiter = ',', value_parser = trim_arg)]
    pub rules: Vec<String>,
    /// Freeze every rule carrying this entity tag, as KEY=VALUE (split on the first '=')
    #[clap(short, long, value_parser = parse_tag, value_name = "KEY=VALUE")]
    pub tag: Option<(String, String)>,
    /// Freeze every rule of the engine's kind you can see
    #[clap(long)]
    pub all: bool,
    /// Freeze the members of this rule set entity, given as an id or name
    #[clap(short = 'R', long)]
    pub ruleset: Option<String>,
}

impl SelectorArgs {
    /// Build the hunt selector from whichever flag was set
    ///
    /// Rule and rule set targets may be given as names, so the handler
    /// resolves them to ids before this runs and passes the results in.
    ///
    /// # Arguments
    ///
    /// * `rule_ids` - The resolved ids of this selector's rule targets
    /// * `ruleset_id` - The resolved id of this selector's rule set target
    ///
    /// # Panics
    ///
    /// Panics if no selector was set (which clap prevents) or if a set
    /// `--ruleset` target was not resolved to an id
    #[must_use]
    pub fn to_selector(&self, rule_ids: Vec<Uuid>, ruleset_id: Option<Uuid>) -> HuntSelector {
        // build the selector matching the one flag that was set
        if !self.rules.is_empty() {
            HuntSelector::Rules(rule_ids)
        } else if let Some((key, value)) = &self.tag {
            HuntSelector::Tag {
                key: key.clone(),
                value: value.clone(),
            }
        } else if self.all {
            HuntSelector::All
        } else if self.ruleset.is_some() {
            // the handler resolves the rule set target before building the selector
            HuntSelector::RuleSet(
                ruleset_id.expect("a set --ruleset target must be resolved to an id"),
            )
        } else {
            unreachable!("clap requires a selector to be set")
        }
    }
}

/// Launch a hunt in Thorium
#[derive(Parser, Debug, Clone)]
pub struct CreateHunt {
    /// The name for the hunt entity
    #[clap(short, long)]
    pub name: String,
    /// The groups this hunt, and everything it writes, is bounded to
    #[clap(short, long, required = true, value_delimiter = ',', value_parser = trim_arg)]
    pub groups: Vec<String>,
    /// The rule engine this hunt runs
    #[clap(short, long, value_enum, default_value_t, ignore_case = true)]
    pub engine: HuntEngineArg,
    /// How to choose the rules this hunt freezes
    #[command(flatten)]
    pub selector: SelectorArgs,
    /// The groups to list corpus files from (defaults to the hunt's groups)
    #[clap(long, value_delimiter = ',', value_parser = trim_arg)]
    pub corpus_groups: Vec<String>,
    /// The newest file upload datetime to scan, as %Y-%m-%dT%H:%M:%S in UTC
    /// (e.g. 2026-10-20T14:30:00); defaults to the hunt's creation instant
    #[clap(long, value_parser = parse_datetime)]
    pub corpus_start: Option<DateTime<Utc>>,
    /// The oldest file upload datetime to scan, as %Y-%m-%dT%H:%M:%S in UTC
    /// (e.g. 2020-01-01T00:00:00); defaults to the earliest the cluster allows
    #[clap(long, value_parser = parse_datetime)]
    pub corpus_end: Option<DateTime<Utc>>,
    /// Only scan files carrying this tag, as KEY=VALUE (repeatable; files must
    /// match every pair given)
    #[clap(long, value_parser = parse_tag, value_name = "KEY=VALUE")]
    pub corpus_tags: Vec<(String, String)>,
    /// Match corpus tag filters case-insensitively
    #[clap(long)]
    pub tags_case_insensitive: bool,
    /// A description for the hunt entity
    #[clap(short, long)]
    pub description: Option<String>,
}

impl CreateHunt {
    /// Check if any corpus bounding flag was set
    #[must_use]
    pub fn has_corpus(&self) -> bool {
        // any one corpus flag opts this hunt into explicit corpus bounds
        !self.corpus_groups.is_empty()
            || self.corpus_start.is_some()
            || self.corpus_end.is_some()
            || !self.corpus_tags.is_empty()
            || self.tags_case_insensitive
    }

    /// Build a hunt create request from this command
    ///
    /// Rule and rule set selector targets may be given as names, so the
    /// handler resolves them to ids before this runs and passes the results in.
    ///
    /// # Arguments
    ///
    /// * `rule_ids` - The resolved ids of the selector's rule targets
    /// * `ruleset_id` - The resolved id of the selector's rule set target
    #[must_use]
    pub fn to_req(&self, rule_ids: Vec<Uuid>, ruleset_id: Option<Uuid>) -> HuntCreateRequest {
        // only build corpus bounds if any corpus flag was set
        let corpus = self.has_corpus().then(|| {
            // collect our corpus tag filters into a key -> values map
            let mut tags: HashMap<String, Vec<String>> = HashMap::default();
            for (key, value) in &self.corpus_tags {
                tags.entry(key.clone()).or_default().push(value.clone());
            }
            // hunts only walk the file listing today
            HuntCorpusRequest::Files(HuntCorpusBoundsRequest {
                groups: self.corpus_groups.clone(),
                start: self.corpus_start,
                end: self.corpus_end,
                tags,
                tags_case_insensitive: self.tags_case_insensitive,
            })
        });
        // build the hunt create request
        HuntCreateRequest {
            name: self.name.clone(),
            groups: self.groups.clone(),
            engine: HuntEngine::from(self.engine),
            selector: self.selector.to_selector(rule_ids, ruleset_id),
            corpus,
            description: self.description.clone(),
        }
    }
}

/// Show the status, per-lane progress, and scan counters of a hunt
#[derive(Parser, Debug, Clone)]
pub struct StatusHunt {
    /// The id or name of the hunt to show status for
    pub hunt: String,
    /// Keep polling and reprinting the status block until the hunt reaches a
    /// terminal status (exits with code 1 if the hunt ends Failed)
    #[clap(long)]
    pub watch: bool,
    /// The number of seconds to wait between polls when watching
    #[clap(long, default_value_t = 2)]
    pub refresh: u64,
}

/// List hunts in Thorium
#[derive(Parser, Debug, Clone)]
pub struct ListHunts {
    /// Only list active hunts (Created or Running)
    #[clap(short, long)]
    pub active: bool,
    /// The groups to list hunts from (defaults to all of your groups)
    #[clap(short, long, value_delimiter = ',', value_parser = trim_arg)]
    pub groups: Vec<String>,
    /// The cursor to continue a search with
    #[clap(long)]
    pub cursor: Option<Uuid>,
    /// The max number of hunts to list
    #[clap(short, long, default_value_t = 50, conflicts_with = "no_limit")]
    pub limit: usize,
    /// List all hunts
    #[clap(long)]
    pub no_limit: bool,
    /// The number of hunts to get per request
    #[clap(short, long, default_value_t = 50)]
    pub page_size: usize,
    /// Print the hunts as JSON instead of a table
    #[clap(long)]
    pub json: bool,
    /// Print condensed JSON (implies `--json`)
    #[clap(short, long)]
    pub condensed: bool,
    /// Write JSON to a file instead of stdout (implies `--json`)
    #[clap(short, long)]
    pub output: Option<PathBuf>,
}

/// List the hits a hunt has written
///
/// Hits are the exact record of what a hunt found: each one carries a HuntId
/// tag pointing back at the hunt that wrote it.
#[derive(Parser, Debug, Clone)]
pub struct HuntHits {
    /// The id or name of the hunt to list hits for (a deleted hunt's hits
    /// stay listable by its id)
    pub hunt: String,
    /// The groups to list hits from (defaults to all of your groups)
    #[clap(short, long, value_delimiter = ',', value_parser = trim_arg)]
    pub groups: Vec<String>,
    /// The cursor to continue a search with
    #[clap(long)]
    pub cursor: Option<Uuid>,
    /// The max number of hits to list
    #[clap(short, long, default_value_t = 50, conflicts_with = "no_limit")]
    pub limit: usize,
    /// List all hits
    #[clap(long)]
    pub no_limit: bool,
    /// The number of hits to get per request
    #[clap(short, long, default_value_t = 50)]
    pub page_size: usize,
    /// Print the hits as JSON instead of a table
    #[clap(long)]
    pub json: bool,
    /// Print condensed JSON (implies `--json`)
    #[clap(short, long)]
    pub condensed: bool,
    /// Write JSON to a file instead of stdout (implies `--json`)
    #[clap(short, long)]
    pub output: Option<PathBuf>,
}

/// List the frozen rules a hunt scans with
///
/// The frozen snapshot is immutable so the listing never changes after
/// creation; rules deleted after the freeze, or outside your groups, list as
/// id-only rows. The STATUS column shows each rule's current quarantine
/// status and exemption, not its state at freeze time.
#[derive(Parser, Debug, Clone)]
pub struct HuntRules {
    /// The id or name of the hunt to list frozen rules for
    pub hunt: String,
    /// The cursor to continue a listing with
    #[clap(long)]
    pub cursor: Option<Uuid>,
    /// The max number of frozen rules to list
    #[clap(short, long, default_value_t = 50, conflicts_with = "no_limit")]
    pub limit: usize,
    /// List all frozen rules
    #[clap(long)]
    pub no_limit: bool,
    /// The number of frozen rules to get per request
    #[clap(short, long, default_value_t = 50)]
    pub page_size: usize,
    /// Print the frozen rules as JSON instead of a table
    #[clap(long)]
    pub json: bool,
    /// Print condensed JSON (implies `--json`)
    #[clap(short, long)]
    pub condensed: bool,
    /// Write JSON to a file instead of stdout (implies `--json`)
    #[clap(short, long)]
    pub output: Option<PathBuf>,
}

/// Cancel a hunt in Thorium
///
/// Only the hunt's submitter or an admin can cancel a hunt. Running chunks
/// stop within one chunk and the hunt cannot be resumed.
#[derive(Parser, Debug, Clone)]
pub struct CancelHunt {
    /// The id or name of the hunt to cancel
    pub hunt: String,
    /// Skip the confirmation prompt before cancelling
    #[clap(short = 'y', long)]
    pub skip_confirm: bool,
}

/// Delete hunts in Thorium
///
/// Deleting a hunt removes only the hunt entity itself; the hits it wrote and
/// their HuntId tags are not deleted with the hunt.
#[derive(Parser, Debug, Clone)]
pub struct DeleteHunts {
    /// The ids or names of the hunts to delete
    #[clap(required = true)]
    pub hunts: Vec<String>,
    /// Skip the confirmation prompt before deleting
    #[clap(short = 'y', long)]
    pub skip_confirm: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::{Args, SubCommands};
    use clap::CommandFactory;

    /// A valid uuid for parse tests
    const UUID: &str = "bf5b4ea2-7a43-4a69-8c41-1f3b2c6e5d8a";
    /// A second valid uuid for parse tests
    const UUID2: &str = "11111111-2222-3333-4444-555555555555";

    /// Parse a hunts command
    ///
    /// # Arguments
    ///
    /// * `args` - The args to parse after the hunts subcommand
    fn parse(args: &[&str]) -> Result<Hunts, clap::Error> {
        // add our binary and subcommand names
        let full = ["thorctl", "hunts"].iter().chain(args.iter());
        match Args::try_parse_from(full)?.cmd {
            SubCommands::Hunts(cmd) => Ok(cmd),
            _ => unreachable!("we always parse a hunts command"),
        }
    }

    /// The hunts command tree must pass clap's own consistency checks
    #[test]
    fn debug_assert() {
        Hunts::command().debug_assert();
    }

    /// A create with an explicit rules selector and corpus flags must build
    /// the matching request
    #[test]
    fn create_rules_selector_with_corpus() {
        let cmd = parse(&[
            "create",
            "-n",
            "corn-hunt",
            "-g",
            "corn,beans",
            "-r",
            &format!("{UUID},{UUID2}"),
            "--corpus-groups",
            "corn",
            "--corpus-start",
            "2026-01-02T03:04:05",
            "--corpus-tags",
            "family=emotet",
            "--corpus-tags",
            "family=qbot",
            "--tags-case-insensitive",
            "-d",
            "hunt historical corn",
        ])
        .unwrap();
        let Hunts::Create(cmd) = cmd else {
            panic!("expected create command")
        };
        // the comma delimited rule targets stay raw strings for resolution
        assert_eq!(cmd.selector.rules, vec![UUID, UUID2]);
        // build the request this command describes from the resolved rule ids
        let rule_ids = vec![
            Uuid::parse_str(UUID).unwrap(),
            Uuid::parse_str(UUID2).unwrap(),
        ];
        let req = cmd.to_req(rule_ids.clone(), None);
        assert_eq!(req.name, "corn-hunt");
        assert_eq!(req.groups, vec!["corn", "beans"]);
        assert_eq!(req.engine, HuntEngine::Yara);
        // the resolved rule ids must become an explicit rules selector
        assert_eq!(req.selector, HuntSelector::Rules(rule_ids));
        // the corpus flags must build file corpus bounds
        let Some(HuntCorpusRequest::Files(bounds)) = req.corpus else {
            panic!("expected a files corpus")
        };
        assert_eq!(bounds.groups, vec!["corn"]);
        assert_eq!(
            bounds.start,
            Some("2026-01-02T03:04:05Z".parse::<DateTime<Utc>>().unwrap())
        );
        assert_eq!(bounds.end, None);
        // repeated corpus tags with one key must collect into one value list
        assert_eq!(
            bounds.tags.get("family"),
            Some(&vec!["emotet".to_owned(), "qbot".to_owned()])
        );
        assert!(bounds.tags_case_insensitive);
        assert_eq!(req.description, Some("hunt historical corn".to_owned()));
    }

    /// The tag selector must split on the first '=' only
    #[test]
    fn create_tag_selector_splits_on_first_equals() {
        let cmd = parse(&["create", "-n", "x", "-g", "corn", "-t", "note=a=b"]).unwrap();
        let Hunts::Create(cmd) = cmd else {
            panic!("expected create command")
        };
        // the value may itself contain '='
        assert_eq!(
            cmd.to_req(Vec::new(), None).selector,
            HuntSelector::Tag {
                key: "note".to_owned(),
                value: "a=b".to_owned(),
            }
        );
    }

    /// The engine must default to yara and omit the corpus when no corpus
    /// flags are set
    #[test]
    fn create_engine_defaults_to_yara() {
        let cmd = parse(&["create", "-n", "x", "-g", "corn", "--all"]).unwrap();
        let Hunts::Create(cmd) = cmd else {
            panic!("expected create command")
        };
        // the engine arg must default to yara
        assert_eq!(cmd.engine, HuntEngineArg::Yara);
        // build the request this command describes
        let req = cmd.to_req(Vec::new(), None);
        assert_eq!(req.engine, HuntEngine::Yara);
        assert_eq!(req.selector, HuntSelector::All);
        // no corpus flags means the server defaults the corpus
        assert_eq!(req.corpus, None);
    }

    /// The ruleset selector must carry its target and take the resolved id
    #[test]
    fn create_ruleset_selector() {
        let cmd = parse(&["create", "-n", "x", "-g", "corn", "-R", UUID]).unwrap();
        let Hunts::Create(cmd) = cmd else {
            panic!("expected create command")
        };
        // the ruleset target stays a raw string for resolution
        assert_eq!(cmd.selector.ruleset.as_deref(), Some(UUID));
        // the resolved rule set id must become a ruleset selector
        assert_eq!(
            cmd.to_req(Vec::new(), Some(Uuid::parse_str(UUID).unwrap()))
                .selector,
            HuntSelector::RuleSet(Uuid::parse_str(UUID).unwrap())
        );
    }

    /// Status must take a positional id with watch and refresh flags
    #[test]
    fn status_watch_refresh() {
        let cmd = parse(&["status", UUID, "--watch", "--refresh", "5"]).unwrap();
        let Hunts::Status(cmd) = cmd else {
            panic!("expected status command")
        };
        assert_eq!(cmd.hunt, UUID);
        assert!(cmd.watch);
        assert_eq!(cmd.refresh, 5);
        // the refresh interval must default to 2 seconds
        let cmd = parse(&["status", UUID]).unwrap();
        let Hunts::Status(cmd) = cmd else {
            panic!("expected status command")
        };
        assert!(!cmd.watch);
        assert_eq!(cmd.refresh, 2);
    }

    /// List must accept the active filter and the output matrix flags
    #[test]
    fn list_active() {
        let cmd = parse(&["list", "--active", "--no-limit", "-c"]).unwrap();
        let Hunts::List(cmd) = cmd else {
            panic!("expected list command")
        };
        assert!(cmd.active);
        assert!(cmd.no_limit);
        assert!(cmd.condensed);
    }

    /// Hits must take a positional hunt id plus list flags
    #[test]
    fn hits_by_hunt() {
        let cmd = parse(&["hits", UUID, "-l", "5", "-g", "corn"]).unwrap();
        let Hunts::Hits(cmd) = cmd else {
            panic!("expected hits command")
        };
        assert_eq!(cmd.hunt, UUID);
        assert_eq!(cmd.limit, 5);
        assert_eq!(cmd.groups, vec!["corn"]);
    }

    /// Rules must take a positional hunt id plus the list flag defaults
    #[test]
    fn rules_defaults() {
        let cmd = parse(&["rules", UUID]).unwrap();
        let Hunts::Rules(cmd) = cmd else {
            panic!("expected rules command")
        };
        assert_eq!(cmd.hunt, UUID);
        // the list flags must carry the shared defaults
        assert_eq!(cmd.limit, 50);
        assert_eq!(cmd.page_size, 50);
        assert!(!cmd.no_limit);
        assert_eq!(cmd.cursor, None);
    }

    /// Rules must accept the full list flag matrix
    #[test]
    fn rules_all_flags() {
        let cmd = parse(&[
            "rules",
            UUID,
            "--no-limit",
            "-p",
            "100",
            "--cursor",
            UUID2,
            "-c",
        ])
        .unwrap();
        let Hunts::Rules(cmd) = cmd else {
            panic!("expected rules command")
        };
        assert!(cmd.no_limit);
        assert_eq!(cmd.page_size, 100);
        assert_eq!(cmd.cursor, Some(Uuid::parse_str(UUID2).unwrap()));
        assert!(cmd.condensed);
        // a limit cannot be mixed with no limit
        assert!(parse(&["rules", UUID, "-l", "5", "--no-limit"]).is_err());
    }

    /// Cancel and delete must take ids and the skip confirm flag
    #[test]
    fn cancel_and_delete() {
        let cmd = parse(&["cancel", UUID, "-y"]).unwrap();
        let Hunts::Cancel(cmd) = cmd else {
            panic!("expected cancel command")
        };
        assert_eq!(cmd.hunt, UUID);
        assert!(cmd.skip_confirm);
        // delete must take multiple positional targets
        let cmd = parse(&["delete", UUID, UUID2, "-y"]).unwrap();
        let Hunts::Delete(cmd) = cmd else {
            panic!("expected delete command")
        };
        assert_eq!(cmd.hunts.len(), 2);
        assert!(cmd.skip_confirm);
    }

    /// Invalid flag combinations and values must be rejected
    #[test]
    fn rejects_invalid() {
        // two selectors
        assert!(parse(&["create", "-n", "x", "-g", "corn", "-r", UUID, "--all"]).is_err());
        assert!(parse(&["create", "-n", "x", "-g", "corn", "--all", "-R", UUID]).is_err());
        // no selector
        assert!(parse(&["create", "-n", "x", "-g", "corn"]).is_err());
        // no groups
        assert!(parse(&["create", "-n", "x", "--all"]).is_err());
        // malformed tag selectors
        assert!(parse(&["create", "-n", "x", "-g", "corn", "-t", "noequals"]).is_err());
        assert!(parse(&["create", "-n", "x", "-g", "corn", "-t", "=value"]).is_err());
        assert!(parse(&["create", "-n", "x", "-g", "corn", "-t", "key="]).is_err());
        // bad engine
        assert!(parse(&["create", "-n", "x", "-g", "corn", "--all", "-e", "sigma"]).is_err());
        // bad corpus date
        assert!(
            parse(&[
                "create",
                "-n",
                "x",
                "-g",
                "corn",
                "--all",
                "--corpus-start",
                "not-a-date",
            ])
            .is_err()
        );
        // malformed corpus tag
        assert!(
            parse(&[
                "create",
                "-n",
                "x",
                "-g",
                "corn",
                "--all",
                "--corpus-tags",
                "noequals",
            ])
            .is_err()
        );
        // limit and no limit
        assert!(parse(&["list", "-l", "5", "--no-limit"]).is_err());
        assert!(parse(&["hits", UUID, "-l", "5", "--no-limit"]).is_err());
        // delete with no targets
        assert!(parse(&["delete", "-y"]).is_err());
    }

    /// Hunt and selector targets must parse as names as well as ids
    #[test]
    fn accepts_names() {
        // create accepts rule names in its selector
        let cmd = parse(&["create", "-n", "x", "-g", "corn", "-r", "demo_a,demo_b"]).unwrap();
        let Hunts::Create(cmd) = cmd else {
            panic!("expected create command")
        };
        assert_eq!(cmd.selector.rules, vec!["demo_a", "demo_b"]);
        // create accepts a rule set name selector
        let cmd = parse(&["create", "-n", "x", "-g", "corn", "-R", "demo-pack"]).unwrap();
        let Hunts::Create(cmd) = cmd else {
            panic!("expected create command")
        };
        assert_eq!(cmd.selector.ruleset.as_deref(), Some("demo-pack"));
        // status accepts a hunt name target
        let cmd = parse(&["status", "corn-hunt"]).unwrap();
        let Hunts::Status(cmd) = cmd else {
            panic!("expected status command")
        };
        assert_eq!(cmd.hunt, "corn-hunt");
        // hits accepts a hunt name target
        let cmd = parse(&["hits", "corn-hunt"]).unwrap();
        let Hunts::Hits(cmd) = cmd else {
            panic!("expected hits command")
        };
        assert_eq!(cmd.hunt, "corn-hunt");
        // rules accepts a hunt name target
        let cmd = parse(&["rules", "corn-hunt"]).unwrap();
        let Hunts::Rules(cmd) = cmd else {
            panic!("expected rules command")
        };
        assert_eq!(cmd.hunt, "corn-hunt");
        // cancel accepts a hunt name target
        let cmd = parse(&["cancel", "corn-hunt", "-y"]).unwrap();
        let Hunts::Cancel(cmd) = cmd else {
            panic!("expected cancel command")
        };
        assert_eq!(cmd.hunt, "corn-hunt");
        // delete accepts hunt name targets
        let cmd = parse(&["delete", "corn-hunt", "bean-hunt", "-y"]).unwrap();
        let Hunts::Delete(cmd) = cmd else {
            panic!("expected delete command")
        };
        assert_eq!(cmd.hunts, vec!["corn-hunt", "bean-hunt"]);
    }
}
