//! Arguments for tool-related Thorctl commands

use clap::Parser;
use std::path::PathBuf;
use uuid::Uuid;

use crate::utils::patch::{self, EditOp};

use super::helpers::{EditArgs, FormatArgs, ShortFormatArgs, WaitForArgs, WatchArgs};
pub use super::runs::{LogsRun, RunTool};

/// The commands to send to the tools task handler
#[derive(Parser, Debug)]
// the arguments are parsed once per invocation, so the larger run variant isn't boxed
#[allow(clippy::large_enum_variant)]
pub enum Tools {
    /// List the tools in your groups
    #[clap(version, author)]
    Get(GetTools),
    /// Describe a tool: its kind, groups, latest version, and number of versions
    #[clap(version, author)]
    Describe(DescribeTool),
    /// List every version of a tool, newest first, marking the latest one
    #[clap(version, author)]
    Versions(ToolVersions),
    /// Create a tool and its first version from a YAML or JSON spec
    #[clap(author)]
    Create(CreateTool),
    /// Publish a new version of a tool from a YAML or JSON spec
    #[clap(author)]
    Publish(PublishTool),
    /// Deprecate a tool version so the tool's latest skips it, or restore one with --undo
    #[clap(version, author)]
    Deprecate(DeprecateTool),
    /// Change a tool's name, description, groups or tags without opening an editor
    #[clap(author)]
    Update(UpdateTool),
    /// Edit a tool version's spec in your editor and publish it as a new version, or edit the
    /// tool itself with --root
    #[clap(author)]
    Edit(EditTool),
    /// Create a tool from a stored image, converting it the way the image migration does
    #[clap(author)]
    Snapshot(SnapshotTool),
    /// Print a tool version's spec in the form `create` and `publish` read
    #[clap(author)]
    Spec(SpecTool),
    /// Delete a tool version, or a tool and all of its versions
    #[clap(author)]
    Delete(DeleteTool),
    /// Set or clear the model values a harness tool's runs get as env vars, without publishing
    #[clap(version, author)]
    HarnessEnv(HarnessEnvCmd),
    /// Disable or enable model ids for every run of a harness tool, without publishing
    #[clap(version, author)]
    HarnessModels(HarnessModels),
    /// Run a tool instance on some files or repos, printing the run as one JSON object
    #[clap(version, author)]
    Run(RunTool),
    /// Read the status or results of a tool run
    #[clap(subcommand)]
    Runs(ToolRuns),
}

/// The commands that read a tool run
#[derive(Parser, Debug)]
pub enum ToolRuns {
    /// Print a tool run's status, with a hint of when to check it again while it runs
    #[clap(version, author)]
    Status(ToolRunStatus),
    /// Download a finished tool run's results and print them
    #[clap(version, author)]
    Results(ToolRunResults),
    /// Print a tool run's status updates and stage logs
    #[clap(version, author)]
    Logs(LogsRun),
}

/// A command to print a tool run's status
#[derive(Parser, Debug)]
pub struct ToolRunStatus {
    /// The tool run's reaction id
    pub reaction: Uuid,
    /// The group the tool run is in (searched for in all of your groups by default)
    #[clap(short, long)]
    pub group: Option<String>,
    /// The format to print the run in; with a format, --watch or --wait-for the run is printed
    /// as the run commands print it, instead of as the one JSON object
    #[clap(flatten)]
    pub format: ShortFormatArgs,
    /// Re-read and re-print the run until interrupted
    #[clap(flatten)]
    pub watch: WatchArgs,
    /// Wait until the run matches a condition (e.g. status=Completed); exits 3 on timeout
    #[clap(flatten)]
    pub wait: WaitForArgs,
}

impl ToolRunStatus {
    /// Whether the run is printed through the shared run view instead of as one JSON object
    #[must_use]
    pub fn uses_run_view(&self) -> bool {
        // a format, watching and waiting on a condition only exist in the shared view
        self.format.format.is_some() || self.watch.watch || self.wait.wait_for.is_some()
    }
}

/// A command to download a tool run's results
#[derive(Parser, Debug)]
pub struct ToolRunResults {
    /// The tool run's reaction id
    pub reaction: Uuid,
    /// The group the tool run is in (searched for in all of your groups by default)
    #[clap(short, long)]
    pub group: Option<String>,
    /// The directory to download results into (default: /tmp/thorium/tool-results/<REACTION>)
    #[clap(long)]
    pub output: Option<PathBuf>,
}

/// The kinds of tool `tools get` can filter on
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKindArg {
    /// Tools exposed to AI agents and called dynamically by a model
    Agentic,
    /// Tools run as Thorium jobs in any workload environment
    Image,
    /// Swappable agent harnesses that run an agent's model and tool loop
    Harness,
}

/// A command to list tools
#[derive(Parser, Debug)]
pub struct GetTools {
    /// The groups to list tools from (all of your groups by default)
    #[clap(short, long)]
    pub group: Vec<String>,
    /// The most tools to list
    #[clap(short, long, default_value_t = 50, conflicts_with = "no_limit")]
    pub limit: usize,
    /// List every tool instead of stopping at the limit
    #[clap(long)]
    pub no_limit: bool,
    /// Only list tools of this kind
    #[clap(long, value_enum)]
    pub kind: Option<ToolKindArg>,
    /// Only list tools with this tag, as KEY=VALUE (repeatable; a tool needs every tag given)
    #[clap(long, value_name = "KEY=VALUE")]
    pub tag: Vec<String>,
    /// The format to print the tools in (a table by default)
    #[clap(flatten)]
    pub format: ShortFormatArgs,
    /// Re-read and re-print the tools until interrupted
    #[clap(flatten)]
    pub watch: WatchArgs,
    /// Wait until every listed tool matches a condition
    #[clap(flatten)]
    pub wait: WaitForArgs,
}

/// A command to describe a tool
#[derive(Parser, Debug)]
pub struct DescribeTool {
    /// The tool to describe, by ID or name
    pub tool: String,
    /// The format to describe the tool in (human readable by default)
    #[clap(flatten)]
    pub format: ShortFormatArgs,
    /// Re-read and re-describe the tool until interrupted
    #[clap(flatten)]
    pub watch: WatchArgs,
    /// Wait until the tool matches a condition
    #[clap(flatten)]
    pub wait: WaitForArgs,
}

/// A command to list a tool's versions
#[derive(Parser, Debug)]
pub struct ToolVersions {
    /// The tool to list versions of, by ID or name
    pub tool: String,
    /// The format to print the versions in (a table by default)
    #[clap(flatten)]
    pub format: ShortFormatArgs,
}

/// A command to create a tool and its first version
#[derive(Parser, Debug)]
pub struct CreateTool {
    /// The tool's spec file, as YAML or JSON (`Agentic:`, `Image:`, or `Harness:` at the top);
    /// - reads stdin
    #[clap(long)]
    pub spec: PathBuf,
    /// The name of the tool
    #[clap(short, long)]
    pub name: String,
    /// The groups to create the tool in
    #[clap(short, long, required = true)]
    pub group: Vec<String>,
    /// The version label of the first version
    #[clap(short, long)]
    pub version: String,
    /// A description of the tool
    #[clap(short, long)]
    pub description: Option<String>,
    /// A tag to put on the tool, as KEY=VALUE (repeatable)
    #[clap(long, value_name = "KEY=VALUE")]
    pub tag: Vec<String>,
    /// A native tool version (tool instance ID) a harness's first version provides (repeatable)
    #[clap(long, value_name = "INSTANCE_ID")]
    pub provides: Vec<Uuid>,
    /// A model value a harness exports, optionally renamed (`OpenAiApiKey` or
    /// `OpenAiApiKey=LITELLM_API_KEY`); repeatable
    #[clap(long, value_name = "VALUE[=VAR]")]
    pub harness_env: Vec<String>,
    /// An exact model id no run of a harness may resolve to (repeatable)
    #[clap(long, value_name = "MODEL_ID")]
    pub disable_model: Vec<String>,
    /// Check the spec and print what would be created, creating nothing
    #[clap(long)]
    pub dry_run: bool,
}

/// How to raise a semantic version label
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bump {
    /// Raise the patch number (1.2.3 becomes 1.2.4)
    Patch,
    /// Raise the minor number and reset the patch number (1.2.3 becomes 1.3.0)
    Minor,
    /// Raise the major number and reset the others (1.2.3 becomes 2.0.0)
    Major,
}

/// A command to publish a new version of a tool
#[derive(Parser, Debug)]
pub struct PublishTool {
    /// The tool to publish a version of, by ID or name
    pub tool: String,
    /// The version label of the new version
    #[clap(short, long, required_unless_present = "bump", conflicts_with = "bump")]
    pub version: Option<String>,
    /// Label the new version by raising the highest semantic version of the tool
    #[clap(long, value_enum)]
    pub bump: Option<Bump>,
    /// The path to the new version's spec, as YAML or JSON (- reads stdin)
    #[clap(
        long,
        required_unless_present = "from_latest",
        conflicts_with = "from_latest"
    )]
    pub spec: Option<PathBuf>,
    /// Start from the latest version's spec instead of a file, applying any --set
    #[clap(long)]
    pub from_latest: bool,
    /// Set one field of the latest version's spec (`--set Agentic.timeout=120`); repeatable, and
    /// only with --from-latest
    #[clap(long = "set", value_name = "PATH=VALUE", value_parser = patch::parse_set)]
    pub set: Vec<EditOp>,
    /// A native tool version (tool instance ID) a new harness version provides (repeatable)
    #[clap(long)]
    pub provides: Vec<uuid::Uuid>,
    /// Print what would be published and publish nothing
    #[clap(long)]
    pub dry_run: bool,
}

/// A command to change a tool's root without opening an editor
///
/// Use either the flags that change one setting each, or `--file`, `--patch`, `--set` and
/// `--unset` on the tool's root document (see `thorctl tools edit --root`).
#[derive(Parser, Debug)]
pub struct UpdateTool {
    /// The tool to update, by ID or name
    pub tool: String,
    /// Rename the tool
    #[clap(long, value_name = "NAME")]
    pub rename: Option<String>,
    /// Set the tool's description
    #[clap(short, long, conflicts_with = "clear_description")]
    pub description: Option<String>,
    /// Remove the tool's description
    #[clap(long)]
    pub clear_description: bool,
    /// Add the tool to a group (repeatable)
    #[clap(long, value_name = "GROUP")]
    pub add_group: Vec<String>,
    /// Remove the tool from a group (repeatable)
    #[clap(long, value_name = "GROUP")]
    pub remove_group: Vec<String>,
    /// Add a tag, as KEY=VALUE (repeatable)
    #[clap(long, value_name = "KEY=VALUE")]
    pub tag: Vec<String>,
    /// Remove a tag, as KEY=VALUE (repeatable)
    #[clap(long, value_name = "KEY=VALUE")]
    pub remove_tag: Vec<String>,
    /// How to change the root document, and what to do with the change
    #[clap(flatten)]
    pub edit: EditArgs,
}

impl UpdateTool {
    /// Whether any flag that changes one setting was given
    #[must_use]
    pub fn has_flag_changes(&self) -> bool {
        self.rename.is_some()
            || self.description.is_some()
            || self.clear_description
            || !self.add_group.is_empty()
            || !self.remove_group.is_empty()
            || !self.tag.is_empty()
            || !self.remove_tag.is_empty()
    }
}

/// A command to edit a tool version's spec, or the tool itself
///
/// Without `--file`, `--patch`, `--set` or `--unset` the document opens in your editor; with any
/// of them the change is applied without opening one. Saving a changed spec publishes it as a
/// new version, labeled with `--version`, or `--bump` applied to the highest semantic version.
#[derive(Parser, Debug)]
pub struct EditTool {
    /// The tool to edit, by ID or name, optionally followed by `@VERSION` (the latest version
    /// by default)
    pub tool: String,
    /// Edit the tool's own settings (name, description, groups, tags) instead of a spec
    #[clap(long, conflicts_with_all = ["version", "bump"])]
    pub root: bool,
    /// The label of the new version
    #[clap(long, conflicts_with = "bump")]
    pub version: Option<String>,
    /// Label the new version by raising the highest semantic version of the tool (patch by default)
    #[clap(long, value_enum)]
    pub bump: Option<Bump>,
    /// How to change the document, and what to do with the change
    #[clap(flatten)]
    pub edit: EditArgs,
}

/// A command to deprecate a tool version
#[derive(Parser, Debug)]
pub struct DeprecateTool {
    /// The ID of the tool version (tool instance) to deprecate
    pub tool_instance: uuid::Uuid,
    /// Restore a deprecated version so the tool's latest can be it again
    #[clap(long)]
    pub undo: bool,
}

/// A command to create a tool from a stored image
#[derive(Parser, Debug)]
pub struct SnapshotTool {
    /// The image to convert, as GROUP/IMAGE
    pub image: String,
    /// The name of the tool (the image's name by default)
    #[clap(short, long)]
    pub name: Option<String>,
    /// The version label of the tool's first version (the image's version by default)
    #[clap(short, long)]
    pub version: Option<String>,
    /// The groups to create the tool in (the image's group by default)
    #[clap(short, long)]
    pub group: Vec<String>,
    /// Convert the image and print what would be created, creating nothing
    #[clap(long)]
    pub dry_run: bool,
}

/// A command to print a tool version's spec
#[derive(Parser, Debug)]
pub struct SpecTool {
    /// The tool to print, by ID or name, optionally followed by `@VERSION` (the latest by default)
    pub tool: String,
    /// Write the spec to this file instead of printing it
    #[clap(short, long)]
    pub output: Option<PathBuf>,
    /// The format of the spec: yaml by default, or json
    #[clap(flatten)]
    pub format: FormatArgs,
}

/// A command to delete a tool or one of its versions
#[derive(Parser, Debug)]
pub struct DeleteTool {
    /// The tool to delete, by ID or name; `TOOL@VERSION` deletes only that version
    pub tool: String,
    /// Delete every version of the tool first (a tool with versions can't be deleted otherwise)
    #[clap(long)]
    pub all_versions: bool,
    /// Delete without asking for confirmation
    #[clap(long, visible_alias = "yes")]
    pub skip_confirm: bool,
    /// Print what would be deleted and delete nothing
    #[clap(long)]
    pub dry_run: bool,
}

/// A command to set or clear a harness tool's env values
#[derive(Parser, Debug)]
#[clap(group = clap::ArgGroup::new("change").required(true).args(["set", "clear"]))]
pub struct HarnessEnvCmd {
    /// The harness tool, by ID or name
    pub harness: String,
    /// A value to export, optionally renamed (`OpenAiApiKey` or `OpenAiApiKey=LITELLM_API_KEY`);
    /// the values given replace the whole env (repeatable)
    #[clap(long)]
    pub set: Vec<String>,
    /// Clear the env so the harness's runs get none of the values
    #[clap(long)]
    pub clear: bool,
}

/// A command to disable or enable model ids on a harness tool
#[derive(Parser, Debug)]
#[clap(group = clap::ArgGroup::new("change").required(true).multiple(true).args(["disable_model", "enable_model"]))]
pub struct HarnessModels {
    /// The harness tool, by ID or name
    pub harness: String,
    /// An exact model id no run of this harness may resolve to (repeatable)
    #[clap(long)]
    pub disable_model: Vec<String>,
    /// A disabled model id to allow again (repeatable)
    #[clap(long)]
    pub enable_model: Vec<String>,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Bump, ToolKindArg, ToolRuns, Tools};
    use crate::args::{Args, SubCommands};
    use crate::utils::output::OutputFormat;

    /// Parse thorctl arguments into a tools command
    ///
    /// # Arguments
    ///
    /// * `args` - The arguments after `thorctl tools`
    fn parse(args: &[&str]) -> Tools {
        // prepend the binary and subcommand names
        let mut full = vec!["thorctl", "tools"];
        full.extend_from_slice(args);
        // parse them into a tools command
        let SubCommands::Tools(tools) = Args::try_parse_from(full).unwrap().cmd else {
            panic!("expected a tools command");
        };
        tools
    }

    /// `tools publish` takes repeated native tools, `harness-env` needs values or a clear, and
    /// `harness-models` needs a model id to disable or enable
    #[test]
    fn harness_args() {
        // publish takes each native tool a harness version provides
        let (first, second) = (
            "69ff8820-f688-4968-bd6f-45a4cb0f78a9",
            "0b5a5f43-7d55-4ad6-8f6b-1a8c4e1fbd10",
        );
        let Tools::Publish(cmd) = parse(&[
            "publish",
            "pal-harness",
            "-v",
            "0.0.2",
            "--spec",
            "h.yaml",
            "--provides",
            first,
            "--provides",
            second,
        ]) else {
            panic!("expected tools publish");
        };
        let provides = cmd
            .provides
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<String>>();
        assert_eq!(provides, vec![first, second]);
        // harness-env sets values or clears them, but not neither
        let Tools::HarnessEnv(cmd) = parse(&[
            "harness-env",
            "pal-harness",
            "--set",
            "OpenAiApiKey=LITELLM_API_KEY",
            "--set",
            "ModelId",
        ]) else {
            panic!("expected tools harness-env");
        };
        assert_eq!(cmd.harness, "pal-harness");
        assert_eq!(cmd.set, vec!["OpenAiApiKey=LITELLM_API_KEY", "ModelId"]);
        assert!(!cmd.clear);
        let Tools::HarnessEnv(cmd) = parse(&["harness-env", "pal-harness", "--clear"]) else {
            panic!("expected tools harness-env");
        };
        assert!(cmd.clear && cmd.set.is_empty());
        assert!(Args::try_parse_from(["thorctl", "tools", "harness-env", "pal-harness"]).is_err());
        // harness-models takes model ids to disable and enable, but not none
        let Tools::HarnessModels(cmd) = parse(&[
            "harness-models",
            "pal-harness",
            "--disable-model",
            "gpt-5",
            "--enable-model",
            "claude-opus",
        ]) else {
            panic!("expected tools harness-models");
        };
        assert_eq!(cmd.disable_model, vec!["gpt-5"]);
        assert_eq!(cmd.enable_model, vec!["claude-opus"]);
        assert!(
            Args::try_parse_from(["thorctl", "tools", "harness-models", "pal-harness"]).is_err()
        );
    }

    /// `tools get` takes groups, a limit, and every output helper flag
    #[test]
    fn get_args() {
        // groups repeat and -o sets the format
        let Tools::Get(cmd) = parse(&[
            "get", "-g", "a", "--group", "b", "-o", "json", "--limit", "5",
        ]) else {
            panic!("expected tools get");
        };
        assert_eq!(cmd.group, vec!["a", "b"]);
        assert_eq!(cmd.limit, 5);
        assert_eq!(cmd.format.format, Some(OutputFormat::Json));
        // the defaults list every group in a table
        let Tools::Get(cmd) = parse(&["get", "--watch"]) else {
            panic!("expected tools get");
        };
        assert_eq!(cmd.group.len(), 0);
        assert_eq!(cmd.limit, 50);
        assert!(cmd.watch.watch);
    }

    /// `tools describe` and `tools versions` take a tool and a format
    #[test]
    fn describe_and_versions_args() {
        // describe takes a tool and a wait condition
        let Tools::Describe(cmd) = parse(&["describe", "scan", "--wait-for", "latest"]) else {
            panic!("expected tools describe");
        };
        assert_eq!(cmd.tool, "scan");
        assert!(cmd.wait.wait_for.is_some());
        // versions takes a tool and a format
        let Tools::Versions(cmd) = parse(&["versions", "scan", "--format", "yaml"]) else {
            panic!("expected tools versions");
        };
        assert_eq!(cmd.tool, "scan");
        assert_eq!(cmd.format.format, Some(OutputFormat::Yaml));
        // a tool is required
        let full = ["thorctl", "tools", "describe"];
        assert!(Args::try_parse_from(full).is_err());
    }

    /// `tools create` needs a spec, a name, a group, and a version
    #[test]
    fn create_args() {
        // every create flag parses
        let Tools::Create(cmd) = parse(&[
            "create",
            "--spec",
            "spec.yaml",
            "-n",
            "scan",
            "-g",
            "a",
            "-g",
            "b",
            "-v",
            "1.0.0",
            "-d",
            "Scans files",
        ]) else {
            panic!("expected tools create");
        };
        assert_eq!(cmd.spec.to_string_lossy(), "spec.yaml");
        assert_eq!(cmd.name, "scan");
        assert_eq!(cmd.group, vec!["a", "b"]);
        assert_eq!(cmd.version, "1.0.0");
        assert_eq!(cmd.description.as_deref(), Some("Scans files"));
        // a group and a version are required
        for missing in [
            [
                "thorctl", "tools", "create", "--spec", "s", "-n", "x", "-v", "1",
            ]
            .as_slice(),
            [
                "thorctl", "tools", "create", "--spec", "s", "-n", "x", "-g", "a",
            ]
            .as_slice(),
        ] {
            assert!(Args::try_parse_from(missing).is_err());
        }
    }

    /// `tools publish` and `tools deprecate` take their targets
    #[test]
    fn publish_and_deprecate_args() {
        // publish takes a tool, a version, and a spec
        let Tools::Publish(cmd) = parse(&["publish", "scan", "-v", "2.0.0", "--spec", "s.json"])
        else {
            panic!("expected tools publish");
        };
        assert_eq!(cmd.tool, "scan");
        assert_eq!(cmd.version.as_deref(), Some("2.0.0"));
        assert_eq!(cmd.spec.unwrap().to_string_lossy(), "s.json");
        // deprecate takes a tool instance id
        let id = "69ff8820-f688-4968-bd6f-45a4cb0f78a9";
        let Tools::Deprecate(cmd) = parse(&["deprecate", id]) else {
            panic!("expected tools deprecate");
        };
        assert_eq!(cmd.tool_instance.to_string(), id);
        // an id that isn't a UUID is refused
        assert!(Args::try_parse_from(["thorctl", "tools", "deprecate", "nope"]).is_err());
    }

    /// `tools spec` takes a tool with an optional version, an output file, and a format
    #[test]
    fn spec_args() {
        // a tool with a version, written to a file as json
        let Tools::Spec(cmd) =
            parse(&["spec", "scan@1.2.0", "-o", "scan.json", "--format", "json"])
        else {
            panic!("expected tools spec");
        };
        assert_eq!(cmd.tool, "scan@1.2.0");
        assert_eq!(cmd.output.unwrap().to_string_lossy(), "scan.json");
        assert_eq!(cmd.format.format, Some(OutputFormat::Json));
        // a bare tool prints to stdout
        let Tools::Spec(cmd) = parse(&["spec", "scan"]) else {
            panic!("expected tools spec");
        };
        assert!(cmd.output.is_none() && cmd.format.format.is_none());
        // a tool is required
        assert!(Args::try_parse_from(["thorctl", "tools", "spec"]).is_err());
    }

    /// `tools delete` takes a target, `--all-versions`, `--skip-confirm` (or `--yes`), and
    /// `--dry-run`
    #[test]
    fn delete_args() {
        // every delete flag parses
        let Tools::Delete(cmd) = parse(&["delete", "scan", "--all-versions", "--yes", "--dry-run"])
        else {
            panic!("expected tools delete");
        };
        assert_eq!(cmd.tool, "scan");
        assert!(cmd.all_versions && cmd.skip_confirm && cmd.dry_run);
        // the long name of the confirmation flag works too
        let Tools::Delete(cmd) = parse(&["delete", "scan@1.0.0", "--skip-confirm"]) else {
            panic!("expected tools delete");
        };
        assert!(cmd.skip_confirm && !cmd.all_versions && !cmd.dry_run);
        // a target is required
        assert!(Args::try_parse_from(["thorctl", "tools", "delete"]).is_err());
    }

    /// `tools publish` takes a version or a bump, a spec or the latest spec with sets, and a
    /// dry run
    #[test]
    fn publish_new_flags() {
        // a bump and the latest spec need no version and no file
        let Tools::Publish(cmd) = parse(&[
            "publish",
            "scan",
            "--bump",
            "minor",
            "--from-latest",
            "--set",
            "Agentic.timeout=120",
            "--dry-run",
        ]) else {
            panic!("expected tools publish");
        };
        assert_eq!(cmd.bump, Some(Bump::Minor));
        assert!(cmd.from_latest && cmd.dry_run && cmd.spec.is_none() && cmd.version.is_none());
        assert_eq!(cmd.set.len(), 1);
        // a spec can come from stdin
        let Tools::Publish(cmd) = parse(&["publish", "scan", "-v", "2", "--spec", "-"]) else {
            panic!("expected tools publish");
        };
        assert_eq!(cmd.spec.unwrap().to_string_lossy(), "-");
        // a version or a bump is needed, and not both
        for bad in [
            ["thorctl", "tools", "publish", "scan", "--spec", "s"].as_slice(),
            [
                "thorctl", "tools", "publish", "scan", "-v", "1", "--bump", "patch", "--spec", "s",
            ]
            .as_slice(),
            // a spec or the latest spec is needed, and not both
            ["thorctl", "tools", "publish", "scan", "-v", "1"].as_slice(),
            [
                "thorctl",
                "tools",
                "publish",
                "scan",
                "-v",
                "1",
                "--spec",
                "s",
                "--from-latest",
            ]
            .as_slice(),
        ] {
            assert!(Args::try_parse_from(bad).is_err(), "{bad:?}");
        }
    }

    /// `tools update` takes the per-setting flags beside the shared edit flags
    #[test]
    fn update_args() {
        // every setting flag parses
        let Tools::Update(cmd) = parse(&[
            "update",
            "scan",
            "--rename",
            "scan2",
            "-d",
            "text",
            "--add-group",
            "a",
            "--add-group",
            "b",
            "--remove-group",
            "c",
            "--tag",
            "k=v",
            "--remove-tag",
            "x=y",
            "--set",
            "name=z",
            "--dry-run",
            "-o",
            "json",
        ]) else {
            panic!("expected tools update");
        };
        assert_eq!(cmd.rename.as_deref(), Some("scan2"));
        assert_eq!(cmd.description.as_deref(), Some("text"));
        assert_eq!(cmd.add_group, vec!["a", "b"]);
        assert_eq!(cmd.remove_group, vec!["c"]);
        assert_eq!(
            (cmd.tag.as_slice(), cmd.remove_tag.as_slice()),
            (["k=v".to_owned()].as_slice(), ["x=y".to_owned()].as_slice())
        );
        assert!(cmd.has_flag_changes() && cmd.edit.has_changes() && cmd.edit.dry_run);
        assert_eq!(cmd.edit.format, Some(OutputFormat::Json));
        // a description and clearing it conflict
        let both = [
            "thorctl",
            "tools",
            "update",
            "scan",
            "-d",
            "x",
            "--clear-description",
        ];
        assert!(Args::try_parse_from(both).is_err());
        // with no flags nothing is asked for
        let Tools::Update(cmd) = parse(&["update", "scan"]) else {
            panic!("expected tools update");
        };
        assert!(!cmd.has_flag_changes() && !cmd.edit.has_changes());
    }

    /// `tools edit` takes a version label or a bump for specs, `--root` for the tool itself,
    /// and the shared edit flags in the order typed
    #[test]
    fn edit_args() {
        // a spec edit with a bump and a set
        let Tools::Edit(cmd) = parse(&[
            "edit",
            "scan@1.0.0",
            "--bump",
            "major",
            "--set",
            "Agentic.timeout=120",
            "--unset",
            "Agentic.title",
            "--editor",
            "vim -n",
        ]) else {
            panic!("expected tools edit");
        };
        assert_eq!(cmd.tool, "scan@1.0.0");
        assert_eq!(cmd.bump, Some(Bump::Major));
        assert!(!cmd.root && cmd.version.is_none());
        assert_eq!(cmd.edit.ops.len(), 2);
        assert_eq!(cmd.edit.editor.as_deref(), Some("vim -n"));
        // a root edit
        let Tools::Edit(cmd) = parse(&["edit", "scan", "--root", "--file", "-"]) else {
            panic!("expected tools edit");
        };
        assert!(cmd.root && cmd.edit.file.as_deref() == Some("-"));
        // a root edit has no version label, and a version and a bump conflict
        for bad in [
            [
                "thorctl", "tools", "edit", "scan", "--root", "--bump", "patch",
            ]
            .as_slice(),
            [
                "thorctl",
                "tools",
                "edit",
                "scan",
                "--version",
                "2",
                "--bump",
                "patch",
            ]
            .as_slice(),
            ["thorctl", "tools", "edit"].as_slice(),
        ] {
            assert!(Args::try_parse_from(bad).is_err(), "{bad:?}");
        }
        // the version label parses
        let Tools::Edit(cmd) = parse(&["edit", "scan", "--version", "2.0.0"]) else {
            panic!("expected tools edit");
        };
        assert_eq!(cmd.version.as_deref(), Some("2.0.0"));
    }

    /// `tools create` takes tags, native tools, harness settings, a dry run, and a spec on stdin
    #[test]
    fn create_extra_flags() {
        // every new flag parses
        let native = "69ff8820-f688-4968-bd6f-45a4cb0f78a9";
        let Tools::Create(cmd) = parse(&[
            "create",
            "--spec",
            "-",
            "-n",
            "h",
            "-g",
            "a",
            "-v",
            "1",
            "--tag",
            "env=prod",
            "--tag",
            "team=a",
            "--provides",
            native,
            "--harness-env",
            "ModelId",
            "--harness-env",
            "OpenAiApiKey=KEY",
            "--disable-model",
            "gpt-5",
            "--dry-run",
        ]) else {
            panic!("expected tools create");
        };
        assert_eq!(cmd.spec.to_string_lossy(), "-");
        assert_eq!(cmd.tag, vec!["env=prod", "team=a"]);
        assert_eq!(cmd.provides.len(), 1);
        assert_eq!(cmd.harness_env, vec!["ModelId", "OpenAiApiKey=KEY"]);
        assert_eq!(cmd.disable_model, vec!["gpt-5"]);
        assert!(cmd.dry_run);
    }

    /// `tools get` filters on kind and tags and can list without a limit
    #[test]
    fn get_filters() {
        // the filters parse
        let Tools::Get(cmd) = parse(&[
            "get",
            "--kind",
            "image",
            "--tag",
            "env=prod",
            "--tag",
            "team=a",
            "--no-limit",
        ]) else {
            panic!("expected tools get");
        };
        assert_eq!(cmd.kind, Some(ToolKindArg::Image));
        assert_eq!(cmd.tag, vec!["env=prod", "team=a"]);
        assert!(cmd.no_limit);
        // a limit and no limit conflict, and a kind must be a known one
        let both = ["thorctl", "tools", "get", "--limit", "5", "--no-limit"];
        assert!(Args::try_parse_from(both).is_err());
        let kind = ["thorctl", "tools", "get", "--kind", "pipeline"];
        assert!(Args::try_parse_from(kind).is_err());
    }

    /// `tools deprecate --undo` restores a version
    #[test]
    fn deprecate_undo() {
        // the flag parses and defaults to deprecating
        let id = "69ff8820-f688-4968-bd6f-45a4cb0f78a9";
        let Tools::Deprecate(cmd) = parse(&["deprecate", id, "--undo"]) else {
            panic!("expected tools deprecate");
        };
        assert!(cmd.undo);
        let Tools::Deprecate(cmd) = parse(&["deprecate", id]) else {
            panic!("expected tools deprecate");
        };
        assert!(!cmd.undo);
    }

    /// `tools snapshot` takes an image and the tool's name, version and groups
    #[test]
    fn snapshot_args() {
        // every snapshot flag parses
        let Tools::Snapshot(cmd) = parse(&[
            "snapshot",
            "corn/scan",
            "-n",
            "scan2",
            "-v",
            "2.0.0",
            "-g",
            "a",
            "-g",
            "b",
            "--dry-run",
        ]) else {
            panic!("expected tools snapshot");
        };
        assert_eq!(cmd.image, "corn/scan");
        assert_eq!(cmd.name.as_deref(), Some("scan2"));
        assert_eq!(cmd.version.as_deref(), Some("2.0.0"));
        assert_eq!(cmd.group, vec!["a", "b"]);
        assert!(cmd.dry_run);
        // an image is required
        assert!(Args::try_parse_from(["thorctl", "tools", "snapshot"]).is_err());
    }

    /// `tools runs status` takes the watch, wait, and format flags, and `tools runs logs` the
    /// run logs flags
    #[test]
    fn runs_status_and_logs_args() {
        let id = "69ff8820-f688-4968-bd6f-45a4cb0f78a9";
        // a plain status keeps the one JSON object
        let Tools::Runs(ToolRuns::Status(cmd)) = parse(&["runs", "status", id]) else {
            panic!("expected tools runs status");
        };
        assert!(!cmd.uses_run_view());
        // each flag moves it to the run view
        for flags in [
            vec!["-o", "json"],
            vec!["--watch"],
            vec!["--wait-for", "status=Completed", "--timeout", "5s"],
        ] {
            let mut args = vec!["runs", "status", id];
            args.extend(flags);
            let Tools::Runs(ToolRuns::Status(cmd)) = parse(&args) else {
                panic!("expected tools runs status");
            };
            assert!(cmd.uses_run_view(), "{args:?}");
        }
        // a watch and a wait condition conflict
        let both = [
            "thorctl",
            "tools",
            "runs",
            "status",
            id,
            "--watch",
            "--wait-for",
            "a",
        ];
        assert!(Args::try_parse_from(both).is_err());
        // logs take a stage and a format
        let Tools::Runs(ToolRuns::Logs(cmd)) = parse(&[
            "runs", "logs", id, "--stage", "scan", "-g", "corn", "--watch",
        ]) else {
            panic!("expected tools runs logs");
        };
        assert_eq!(cmd.stage.as_deref(), Some("scan"));
        assert_eq!(cmd.group.as_deref(), Some("corn"));
        assert!(cmd.watch.watch);
    }
}
