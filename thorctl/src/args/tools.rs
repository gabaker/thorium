//! Arguments for tool-related Thorctl commands

use clap::Parser;
use std::path::PathBuf;

use super::helpers::{ShortFormatArgs, WaitForArgs, WatchArgs};

/// The commands to send to the tools task handler
#[derive(Parser, Debug)]
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
    /// Deprecate a tool version so the tool's latest skips it
    #[clap(version, author)]
    Deprecate(DeprecateTool),
    /// Set or clear the model values a harness tool's runs get as env vars, without publishing
    #[clap(version, author)]
    HarnessEnv(HarnessEnvCmd),
    /// Disable or enable model ids for every run of a harness tool, without publishing
    #[clap(version, author)]
    HarnessModels(HarnessModels),
}

/// A command to list tools
#[derive(Parser, Debug)]
pub struct GetTools {
    /// The groups to list tools from (all of your groups by default)
    #[clap(short, long)]
    pub group: Vec<String>,
    /// The most tools to list
    #[clap(short, long, default_value_t = 50)]
    pub limit: usize,
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
    /// The tool's spec file, as YAML or JSON (`Agentic:`, `Image:`, or `Harness:` at the top)
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
}

/// A command to publish a new version of a tool
#[derive(Parser, Debug)]
pub struct PublishTool {
    /// The tool to publish a version of, by ID or name
    pub tool: String,
    /// The version label of the new version
    #[clap(short, long)]
    pub version: String,
    /// The path to the new version's spec, as YAML or JSON
    #[clap(long)]
    pub spec: PathBuf,
    /// A native tool version (tool instance ID) a new harness version provides (repeatable)
    #[clap(long)]
    pub provides: Vec<uuid::Uuid>,
}

/// A command to deprecate a tool version
#[derive(Parser, Debug)]
pub struct DeprecateTool {
    /// The ID of the tool version (tool instance) to deprecate
    pub tool_instance: uuid::Uuid,
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

    use super::Tools;
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
        assert_eq!(cmd.version, "2.0.0");
        assert_eq!(cmd.spec.to_string_lossy(), "s.json");
        // deprecate takes a tool instance id
        let id = "69ff8820-f688-4968-bd6f-45a4cb0f78a9";
        let Tools::Deprecate(cmd) = parse(&["deprecate", id]) else {
            panic!("expected tools deprecate");
        };
        assert_eq!(cmd.tool_instance.to_string(), id);
        // an id that isn't a UUID is refused
        assert!(Args::try_parse_from(["thorctl", "tools", "deprecate", "nope"]).is_err());
    }
}
