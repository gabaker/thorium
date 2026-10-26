//! Arguments for `thorctl toolbox import`

use clap::Parser;

use super::ManifestLocation;
use crate::args::helpers::ShortFormatArgs;

/// The kinds of item `--only` can name
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum OnlyKind {
    /// A tool
    Tool,
    /// A toolset
    Toolset,
    /// An agent
    Agent,
    /// A workflow
    Workflow,
    /// A pipeline
    Pipeline,
}

/// One item named by `--only`, written `KIND/NAME`
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct OnlyItem {
    /// The kind of the item
    pub kind: OnlyKind,
    /// The name of the item
    pub name: String,
}

impl std::str::FromStr for OnlyItem {
    type Err = String;

    /// Parse `KIND/NAME` where the kind is tool, toolset, agent, workflow, or pipeline
    ///
    /// # Arguments
    ///
    /// * `raw` - The text to parse
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        // split the kind from the name
        let (kind, name) = raw
            .split_once('/')
            .ok_or_else(|| format!("'{raw}' is not of the form KIND/NAME"))?;
        // a name is required
        if name.is_empty() {
            return Err(format!("'{raw}' names no item after the '/'"));
        }
        // the kind is one of the five kinds of item a toolbox holds
        let kind = match kind.to_ascii_lowercase().as_str() {
            "tool" => OnlyKind::Tool,
            "toolset" => OnlyKind::Toolset,
            "agent" => OnlyKind::Agent,
            "workflow" => OnlyKind::Workflow,
            "pipeline" => OnlyKind::Pipeline,
            other => {
                return Err(format!(
                    "'{other}' is not a kind of item; use tool, toolset, agent, workflow, or pipeline"
                ));
            }
        };
        Ok(OnlyItem {
            kind,
            name: name.to_owned(),
        })
    }
}

/// The settings `toolbox import` and `toolbox update` have in common
#[derive(clap::Args, Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct ImportOpts {
    /// The toolbox manifest to use: a toolbox.json path, an http(s) URL, or a toolbox directory.
    /// Format: `path | url | dir`
    ///
    /// Legacy toolboxes (images and pipelines) are converted in memory first, exactly as
    /// `toolbox convert` would, and the conversion's issues are reported.
    #[clap(value_name = "PATH | URL | DIR")]
    pub manifest: ManifestLocation,
    /// Force every item to be imported into a specific group
    ///
    /// The group is created if it doesn't already exist.
    #[clap(long, value_name = "GROUP")]
    pub group_override: Option<String>,
    /// Only import these tools (and what they need), comma separated
    ///
    /// Anything a selected item depends on is imported with it.
    #[clap(
        short = 't',
        long = "tools",
        alias = "images",
        short_alias = 'i',
        value_delimiter = ',',
        value_name = "NAME"
    )]
    pub tools: Vec<String>,
    /// Only import these pipelines (and the tools they need), comma separated
    #[clap(
        short = 'p',
        long = "pipelines",
        value_delimiter = ',',
        value_name = "NAME"
    )]
    pub pipelines: Vec<String>,
    /// Only import this item (and what it needs), written KIND/NAME; repeat for several items
    ///
    /// The kind is tool, toolset, agent, workflow, or pipeline.
    #[clap(long = "only", value_name = "KIND/NAME")]
    pub only: Vec<OnlyItem>,
    /// Apply every difference without asking
    ///
    /// A changed tool version is published under its label, or under `LABEL+HASH` when the label
    /// is taken by a different spec, since published versions never change. Toolsets, agents,
    /// workflows, and pipelines that differ are updated in place.
    #[clap(long, conflicts_with = "skip_conflicts")]
    pub overwrite: bool,
    /// Create what is missing but never change what already exists
    ///
    /// Each existing item that differs from the toolbox is left as it is and listed. This is
    /// safe for non-interactive (CI or agent) imports that must never overwrite local changes.
    #[clap(long)]
    pub skip_conflicts: bool,
    /// Show what would be created, published, and updated, and write nothing
    #[clap(long)]
    pub dry_run: bool,
    /// Don't ask for confirmation before writing
    #[clap(long, visible_alias = "yes")]
    pub skip_confirm: bool,
    /// The format to print the plan and the result in
    #[clap(flatten)]
    pub format: ShortFormatArgs,
    /// Automatically roll back what was applied if the import stops early
    ///
    /// Without this flag a session with a terminal is asked, and any other session keeps what
    /// was applied and reports it.
    #[clap(long)]
    pub rollback_on_failure: bool,
    /// The editor to use when reviewing a difference
    ///
    /// Accepted for compatibility; differences are decided with Apply, Skip, or Quit.
    #[clap(long, value_name = "EDITOR")]
    pub editor: Option<String>,
    /// Update existing Thorium network policies to match the toolbox
    ///
    /// Without this, an existing policy that differs is left untouched and only a warning is
    /// logged. With it, the toolbox's definition is authoritative: rule/flag differences
    /// overwrite the policy, and a groups-only difference adds the toolbox's groups to the
    /// existing policy. Group coverage is only ever added, never removed.
    ///
    /// This overwrites cluster network-security state, and it applies WITHOUT a prompt when the
    /// session can't ask.
    #[clap(long)]
    pub update_network_policy: bool,
    /// Target registry base path for images bundled in the toolbox
    ///
    /// Only used when the toolbox bundles container images (exported with `--with-images`):
    /// each tool's container image is rewritten to `<image-path-prefix>/<group>/<name>:<tag>`.
    /// Importing does not push container images; push them to that registry first.
    #[clap(long, value_name = "REGISTRY/BASE")]
    pub image_path_prefix: Option<String>,
    /// Make pipeline stages of a legacy toolbox follow their tool's latest version instead of
    /// pinning the version the legacy pipeline named
    #[clap(long)]
    pub follow_latest: bool,
    /// Don't import pipelines
    #[clap(long)]
    pub no_pipelines: bool,
    /// Stop before any write if converting a legacy toolbox raised an issue or the manifest has
    /// a warning
    #[clap(long)]
    pub strict: bool,
}

/// Import a toolbox manifest into Thorium
#[derive(Parser, Debug)]
pub struct ImportToolbox {
    /// What to import and how
    #[clap(flatten)]
    pub opts: ImportOpts,
    /// Create the groups the toolbox names that don't exist yet, owned by you
    #[clap(long)]
    pub create_groups: bool,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::utils::output::OutputFormat;

    /// Parse an import command line
    ///
    /// # Arguments
    ///
    /// * `args` - The arguments after `import`
    fn parse(args: &[&str]) -> Result<ImportToolbox, clap::Error> {
        // the command name stands in for argv[0]
        ImportToolbox::try_parse_from(std::iter::once("import").chain(args.iter().copied()))
    }

    /// A manifest alone parses with every flag off
    #[test]
    fn manifest_alone_parses_with_defaults() {
        // only the positional is given
        let cmd = parse(&["toolbox.json"]).unwrap();
        assert!(!cmd.opts.overwrite && !cmd.opts.skip_conflicts && !cmd.opts.dry_run);
        assert!(!cmd.create_groups && !cmd.opts.strict && !cmd.opts.no_pipelines);
        assert!(cmd.opts.tools.is_empty() && cmd.opts.only.is_empty());
        assert_eq!(cmd.opts.format.format, None);
        // the manifest is required
        assert!(parse(&[]).is_err());
    }

    /// The selection flags take lists, and the old image names still work
    #[test]
    fn selection_flags_parse() {
        // tools and pipelines are comma separated, and --only repeats
        let cmd = parse(&[
            "toolbox.json",
            "-t",
            "a,b",
            "-p",
            "c",
            "--only",
            "agent/triage",
            "--only",
            "Toolset/kit",
        ])
        .unwrap();
        assert_eq!(cmd.opts.tools, ["a", "b"]);
        assert_eq!(cmd.opts.pipelines, ["c"]);
        assert_eq!(
            cmd.opts.only,
            [
                OnlyItem {
                    kind: OnlyKind::Agent,
                    name: "triage".to_owned()
                },
                OnlyItem {
                    kind: OnlyKind::Toolset,
                    name: "kit".to_owned()
                }
            ]
        );
        // the legacy spellings select tools too
        let cmd = parse(&["toolbox.json", "--images", "x"]).unwrap();
        assert_eq!(cmd.opts.tools, ["x"]);
        let cmd = parse(&["toolbox.json", "-i", "y"]).unwrap();
        assert_eq!(cmd.opts.tools, ["y"]);
        // a malformed selector is a usage error
        assert!(parse(&["toolbox.json", "--only", "tool"]).is_err());
        assert!(parse(&["toolbox.json", "--only", "image/x"]).is_err());
        assert!(parse(&["toolbox.json", "--only", "tool/"]).is_err());
    }

    /// Overwrite and skip-conflicts exclude each other, and --yes is --skip-confirm
    #[test]
    fn conflict_and_confirm_flags_parse() {
        // each conflict flag alone is fine
        assert!(
            parse(&["toolbox.json", "--overwrite"])
                .unwrap()
                .opts
                .overwrite
        );
        assert!(
            parse(&["toolbox.json", "--skip-conflicts"])
                .unwrap()
                .opts
                .skip_conflicts
        );
        // both together are refused
        assert!(parse(&["toolbox.json", "--overwrite", "--skip-conflicts"]).is_err());
        // --yes is another name for --skip-confirm
        assert!(parse(&["toolbox.json", "--yes"]).unwrap().opts.skip_confirm);
        assert!(
            parse(&["toolbox.json", "--skip-confirm"])
                .unwrap()
                .opts
                .skip_confirm
        );
    }

    /// The remaining flags parse, including the short format flag
    #[test]
    fn other_flags_parse() {
        // every other flag in one command line
        let cmd = parse(&[
            "toolbox.json",
            "--group-override",
            "g",
            "--create-groups",
            "--dry-run",
            "-o",
            "json",
            "--rollback-on-failure",
            "--update-network-policy",
            "--image-path-prefix",
            "reg/base",
            "--follow-latest",
            "--no-pipelines",
            "--strict",
            "--editor",
            "vi",
        ])
        .unwrap();
        assert_eq!(cmd.opts.group_override.as_deref(), Some("g"));
        assert!(cmd.create_groups && cmd.opts.dry_run && cmd.opts.rollback_on_failure);
        assert!(cmd.opts.update_network_policy && cmd.opts.follow_latest);
        assert!(cmd.opts.no_pipelines && cmd.opts.strict);
        assert_eq!(cmd.opts.format.format, Some(OutputFormat::Json));
        assert_eq!(cmd.opts.image_path_prefix.as_deref(), Some("reg/base"));
        assert_eq!(cmd.opts.editor.as_deref(), Some("vi"));
    }
}
