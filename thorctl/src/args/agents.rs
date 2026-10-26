//! Arguments for AI agent Thorctl commands

use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thorium::models::ModelAuthLevel;
use uuid::Uuid;

use super::helpers::{EditArgs, FormatArgs, ShortFormatArgs, WaitForArgs, WatchArgs};

/// The commands to send to the AI agents task handler
#[derive(Parser, Debug)]
pub enum Agents {
    /// List the AI agents in your groups
    #[clap(version, author)]
    Get(GetAgents),
    /// Describe an AI agent: its models, limits, tools, toolsets, sub-agents, and harness
    #[clap(version, author)]
    Describe(DescribeAgent),
    /// Create an AI agent from a YAML or JSON config file
    #[clap(version, author)]
    Create(CreateAgent),
    /// Let an AI agent call a tool, at its latest version or pinned to one version
    #[clap(version, author)]
    LinkTool(LinkTool),
    /// Let an AI agent delegate to another AI agent
    #[clap(version, author)]
    Delegate(Delegate),
    /// Let an AI agent call every tool in a toolset, at its latest instance or pinned to one
    #[clap(version, author)]
    LinkToolset(LinkToolset),
    /// Run an AI agent in a harness, at its latest version or pinned to one version
    #[clap(version, author)]
    SetHarness(SetHarness),
    /// Stop an AI agent calling a tool
    #[clap(version, author)]
    UnlinkTool(UnlinkTool),
    /// Stop an AI agent calling a toolset
    #[clap(version, author)]
    UnlinkToolset(UnlinkToolset),
    /// Stop an AI agent delegating to another AI agent
    #[clap(version, author)]
    Undelegate(Undelegate),
    /// Stop an AI agent running in its harness
    #[clap(version, author)]
    ClearHarness(ClearHarness),
    /// Change an AI agent with flags, a spec file, or `--set`, `--unset` and `--patch`
    #[clap(version, author)]
    Update(UpdateAgent),
    /// Edit an AI agent in your editor, or with `--file`, `--set`, `--unset` and `--patch`
    #[clap(version, author)]
    Edit(EditAgent),
    /// Print an AI agent as a spec that `agents update --file` reads back
    #[clap(version, author)]
    Spec(SpecAgent),
    /// Delete an AI agent
    #[clap(version, author)]
    Delete(DeleteAgent),
}

/// A command to list AI agents
#[derive(Parser, Debug)]
pub struct GetAgents {
    /// The groups to list AI agents from (all of your groups by default)
    #[clap(short, long)]
    pub group: Vec<String>,
    /// The most AI agents to list
    #[clap(short, long, default_value_t = 50)]
    pub limit: usize,
    /// The format to print the AI agents in (a table by default)
    #[clap(flatten)]
    pub format: ShortFormatArgs,
    /// Re-read and re-print the AI agents until interrupted
    #[clap(flatten)]
    pub watch: WatchArgs,
    /// Wait until every listed AI agent matches a condition
    #[clap(flatten)]
    pub wait: WaitForArgs,
}

/// A command to describe an AI agent
#[derive(Parser, Debug)]
pub struct DescribeAgent {
    /// The AI agent to describe, by ID or name
    pub agent: String,
    /// The format to describe the AI agent in (human readable by default)
    #[clap(flatten)]
    pub format: ShortFormatArgs,
    /// Re-read and re-describe the AI agent until interrupted
    #[clap(flatten)]
    pub watch: WatchArgs,
    /// Wait until the AI agent matches a condition
    #[clap(flatten)]
    pub wait: WaitForArgs,
}

/// A level that may supply model auth for an AI agent
#[derive(ValueEnum, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum ModelAuthArg {
    /// Auth the harness supplies itself
    #[serde(alias = "Harness")]
    Harness,
    /// Auth configured on the workflow
    #[serde(alias = "Workflow")]
    Workflow,
    /// The endpoints and credentials of the run's group
    #[serde(alias = "Group")]
    Group,
    /// The runner's own endpoints and credentials
    #[serde(alias = "User")]
    User,
}

impl From<ModelAuthLevel> for ModelAuthArg {
    /// Convert a level the API uses into its name
    ///
    /// # Arguments
    ///
    /// * `level` - The level to convert
    fn from(level: ModelAuthLevel) -> Self {
        // each level has the name of the same name
        match level {
            ModelAuthLevel::Harness => ModelAuthArg::Harness,
            ModelAuthLevel::Workflow => ModelAuthArg::Workflow,
            ModelAuthLevel::Group => ModelAuthArg::Group,
            ModelAuthLevel::User => ModelAuthArg::User,
        }
    }
}

impl From<ModelAuthArg> for ModelAuthLevel {
    /// Convert a model auth name into the level the API uses
    ///
    /// # Arguments
    ///
    /// * `arg` - The name to convert
    fn from(arg: ModelAuthArg) -> Self {
        // each name has the level of the same name
        match arg {
            ModelAuthArg::Harness => ModelAuthLevel::Harness,
            ModelAuthArg::Workflow => ModelAuthLevel::Workflow,
            ModelAuthArg::Group => ModelAuthLevel::Group,
            ModelAuthArg::User => ModelAuthLevel::User,
        }
    }
}

/// A command to create an AI agent
#[derive(Parser, Debug)]
pub struct CreateAgent {
    /// The path to the AI agent's config, as YAML or JSON (- reads stdin)
    #[clap(long)]
    pub config: PathBuf,
    /// The name of the AI agent
    #[clap(short, long)]
    pub name: String,
    /// The groups to create the AI agent in
    #[clap(short, long, required = true)]
    pub group: Vec<String>,
    /// A description of the AI agent
    #[clap(short, long)]
    pub description: Option<String>,
    /// A tool the AI agent calls, as NAME (latest version) or NAME@VERSION (pinned); repeatable
    #[clap(long)]
    pub tool: Vec<String>,
    /// A toolset the AI agent calls, as NAME (latest instance) or NAME#rN (pinned); repeatable
    #[clap(long)]
    pub toolset: Vec<String>,
    /// An AI agent this AI agent can delegate to, by ID or name; repeatable
    #[clap(long)]
    pub sub_agent: Vec<String>,
    /// The harness tool to run in, as NAME (latest version) or NAME@VERSION (pinned)
    #[clap(long)]
    pub harness: Option<String>,
    /// A level that may supply model auth for the AI agent (every level by default); repeatable
    #[clap(long, value_enum, ignore_case = true)]
    pub model_auth: Vec<ModelAuthArg>,
    /// Add the AI agent's groups to the tools, toolsets, and sub-agents that miss some of them
    #[clap(long)]
    pub extend_target_groups: bool,
}

/// A command to let an AI agent call a tool
#[derive(Parser, Debug)]
pub struct LinkTool {
    /// The AI agent, by ID or name
    pub agent: String,
    /// The tool to call, by ID or name
    pub tool: String,
    /// Pin the AI agent to this version (tool instance) of the tool instead of its latest
    #[clap(long)]
    pub pin: Option<Uuid>,
    /// Add the AI agent's groups to the tool when it's missing some of them
    #[clap(long)]
    pub extend_target_groups: bool,
}

/// A command to let an AI agent call a toolset
#[derive(Parser, Debug)]
pub struct LinkToolset {
    /// The AI agent, by ID or name
    pub agent: String,
    /// The toolset to call, by ID or name
    pub toolset: String,
    /// Pin the AI agent to this instance of the toolset instead of its latest
    #[clap(long)]
    pub pin: Option<Uuid>,
    /// Add the AI agent's groups to the toolset when it's missing some of them
    #[clap(long)]
    pub extend_target_groups: bool,
}

/// A command to let an AI agent delegate to another AI agent
#[derive(Parser, Debug)]
pub struct Delegate {
    /// The AI agent that delegates, by ID or name
    pub agent: String,
    /// The AI agent to delegate to, by ID or name
    pub sub_agent: String,
    /// Add the AI agent's groups to the sub-agent when it's missing some of them
    #[clap(long)]
    pub extend_target_groups: bool,
}

/// A command to run an AI agent in a harness
#[derive(Parser, Debug)]
pub struct SetHarness {
    /// The AI agent, by ID or name
    pub agent: String,
    /// The harness tool to run in, by ID or name
    pub harness: String,
    /// Pin the AI agent to this version (tool instance) of the harness instead of its latest
    #[clap(long)]
    pub pin: Option<Uuid>,
    /// Add the AI agent's groups to the harness when it's missing some of them
    #[clap(long)]
    pub extend_target_groups: bool,
}

/// A command to stop an AI agent calling a tool
#[derive(Parser, Debug)]
pub struct UnlinkTool {
    /// The AI agent, by ID or name
    pub agent: String,
    /// The tool to stop calling, by ID or name
    pub tool: String,
    /// Remove the pin to this version (tool instance) instead of the latest edge
    #[clap(long)]
    pub pin: Option<Uuid>,
}

/// A command to stop an AI agent calling a toolset
#[derive(Parser, Debug)]
pub struct UnlinkToolset {
    /// The AI agent, by ID or name
    pub agent: String,
    /// The toolset to stop calling, by ID or name
    pub toolset: String,
    /// Remove the pin to this instance of the toolset instead of the latest edge
    #[clap(long)]
    pub pin: Option<Uuid>,
}

/// A command to stop an AI agent delegating to another AI agent
#[derive(Parser, Debug)]
pub struct Undelegate {
    /// The AI agent that delegates, by ID or name
    pub agent: String,
    /// The AI agent to stop delegating to, by ID or name
    pub sub_agent: String,
}

/// A command to stop an AI agent running in its harness
#[derive(Parser, Debug)]
pub struct ClearHarness {
    /// The AI agent, by ID or name
    pub agent: String,
}

/// A command to change an AI agent without an editor
#[derive(Parser, Debug)]
pub struct UpdateAgent {
    /// The AI agent to update, by ID or name
    pub agent: String,
    /// Rename the AI agent
    #[clap(long)]
    pub rename: Option<String>,
    /// Set the AI agent's description
    #[clap(short, long, conflicts_with = "clear_description")]
    pub description: Option<String>,
    /// Remove the AI agent's description
    #[clap(long)]
    pub clear_description: bool,
    /// Add the AI agent to a group; repeatable
    #[clap(long, conflicts_with = "file")]
    pub add_group: Vec<String>,
    /// Remove the AI agent from a group; repeatable
    #[clap(long, conflicts_with = "file")]
    pub remove_group: Vec<String>,
    /// Replace the AI agent's config with a YAML or JSON file (- reads stdin)
    #[clap(long)]
    pub config: Option<PathBuf>,
    /// Limit the levels that may supply model auth for the AI agent; repeatable
    #[clap(
        long,
        value_enum,
        ignore_case = true,
        conflicts_with = "clear_model_auth"
    )]
    pub model_auth: Vec<ModelAuthArg>,
    /// Allow every level to supply model auth for the AI agent
    #[clap(long)]
    pub clear_model_auth: bool,
    /// Add the AI agent's groups to the tools, toolsets, and sub-agents that miss some of them
    #[clap(long)]
    pub extend_target_groups: bool,
    /// The shared edit flags
    #[clap(flatten)]
    pub edit: EditArgs,
}

/// A command to edit an AI agent
#[derive(Parser, Debug)]
pub struct EditAgent {
    /// The AI agent to edit, by ID or name
    pub agent: String,
    /// Add the AI agent's groups to the tools, toolsets, and sub-agents that miss some of them
    #[clap(long)]
    pub extend_target_groups: bool,
    /// The shared edit flags
    #[clap(flatten)]
    pub edit: EditArgs,
}

/// A command to print an AI agent as a spec
#[derive(Parser, Debug)]
pub struct SpecAgent {
    /// The AI agent to print, by ID or name
    pub agent: String,
    /// Write the spec to this file instead of stdout
    #[clap(short, long)]
    pub output: Option<PathBuf>,
    /// The format to print the spec in: yaml (the default) or json
    #[clap(flatten)]
    pub format: FormatArgs,
}

/// A command to delete an AI agent
#[derive(Parser, Debug)]
pub struct DeleteAgent {
    /// The AI agent to delete, by ID or name
    pub agent: String,
    /// Delete without asking for confirmation
    #[clap(long, visible_alias = "yes")]
    pub skip_confirm: bool,
    /// Print what would be deleted and stop
    #[clap(long)]
    pub dry_run: bool,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Agents, ModelAuthArg};
    use crate::args::{Args, SubCommands};
    use crate::utils::output::OutputFormat;
    use thorium::models::ModelAuthLevel;

    /// Parse thorctl arguments into an agents command
    ///
    /// # Arguments
    ///
    /// * `args` - The arguments after `thorctl agents`
    fn parse(args: &[&str]) -> Agents {
        // prepend the binary and subcommand names
        let mut full = vec!["thorctl", "agents"];
        full.extend_from_slice(args);
        // parse them into an agents command
        let SubCommands::Agents(agents) = Args::try_parse_from(full).unwrap().cmd else {
            panic!("expected an agents command");
        };
        agents
    }

    /// `agents get` and `describe` take groups, an agent, and the output flags
    #[test]
    fn get_and_describe_args() {
        // get takes groups and a format
        let Agents::Get(cmd) = parse(&["get", "-g", "minerva", "-o", "json"]) else {
            panic!("expected agents get");
        };
        assert_eq!(cmd.group, vec!["minerva"]);
        assert_eq!(cmd.format.format, Some(OutputFormat::Json));
        assert_eq!(cmd.limit, 50);
        // describe takes an agent
        let Agents::Describe(cmd) = parse(&["describe", "triage", "--format", "yaml"]) else {
            panic!("expected agents describe");
        };
        assert_eq!(cmd.agent, "triage");
        assert_eq!(cmd.format.format, Some(OutputFormat::Yaml));
    }

    /// `agents create` needs a config, a name, and a group
    #[test]
    fn create_args() {
        // every create flag parses
        let Agents::Create(cmd) = parse(&[
            "create", "--config", "a.yaml", "-n", "triage", "-g", "minerva", "-g", "mars",
        ]) else {
            panic!("expected agents create");
        };
        assert_eq!(cmd.config.to_string_lossy(), "a.yaml");
        assert_eq!(cmd.name, "triage");
        assert_eq!(cmd.group, vec!["minerva", "mars"]);
        // a group is required
        let missing = ["thorctl", "agents", "create", "--config", "a", "-n", "x"];
        assert!(Args::try_parse_from(missing).is_err());
    }

    /// `agents create` takes the AI agent's edges, harness, model auth, and `-` for stdin
    #[test]
    fn create_edge_args() {
        // every edge flag repeats and the harness takes a pin
        let Agents::Create(cmd) = parse(&[
            "create",
            "--config",
            "-",
            "-n",
            "triage",
            "-g",
            "minerva",
            "--tool",
            "yara",
            "--tool",
            "capa@2.0.0",
            "--toolset",
            "ghidra#r2",
            "--sub-agent",
            "research",
            "--harness",
            "pal@1.0.0",
            "--model-auth",
            "group",
            "--model-auth",
            "USER",
            "--extend-target-groups",
        ]) else {
            panic!("expected agents create");
        };
        assert_eq!(cmd.config.to_string_lossy(), "-");
        assert_eq!(cmd.tool, vec!["yara", "capa@2.0.0"]);
        assert_eq!(cmd.toolset, vec!["ghidra#r2"]);
        assert_eq!(cmd.sub_agent, vec!["research"]);
        assert_eq!(cmd.harness.as_deref(), Some("pal@1.0.0"));
        assert_eq!(
            cmd.model_auth,
            vec![ModelAuthArg::Group, ModelAuthArg::User]
        );
        assert!(cmd.extend_target_groups);
        // an unknown model auth level is refused
        let bad = [
            "thorctl",
            "agents",
            "create",
            "--config",
            "a",
            "-n",
            "x",
            "-g",
            "g",
            "--model-auth",
            "everyone",
        ];
        assert!(Args::try_parse_from(bad).is_err());
    }

    /// The model auth names map to the levels the API uses
    #[test]
    fn model_auth_levels() {
        // each name maps to its level
        assert_eq!(
            ModelAuthLevel::from(ModelAuthArg::Harness),
            ModelAuthLevel::Harness
        );
        assert_eq!(
            ModelAuthLevel::from(ModelAuthArg::Workflow),
            ModelAuthLevel::Workflow
        );
        assert_eq!(
            ModelAuthLevel::from(ModelAuthArg::Group),
            ModelAuthLevel::Group
        );
        assert_eq!(
            ModelAuthLevel::from(ModelAuthArg::User),
            ModelAuthLevel::User
        );
    }

    /// The unlink commands mirror the link commands
    #[test]
    fn unlink_args() {
        // unlink-tool takes an agent, a tool, and an optional pin
        let id = "69ff8820-f688-4968-bd6f-45a4cb0f78a9";
        let Agents::UnlinkTool(cmd) = parse(&["unlink-tool", "triage", "yara", "--pin", id]) else {
            panic!("expected agents unlink-tool");
        };
        assert_eq!((cmd.agent.as_str(), cmd.tool.as_str()), ("triage", "yara"));
        assert_eq!(cmd.pin.map(|pin| pin.to_string()), Some(id.to_owned()));
        // unlink-toolset takes an agent, a toolset, and an optional pin
        let Agents::UnlinkToolset(cmd) = parse(&["unlink-toolset", "triage", "ghidra"]) else {
            panic!("expected agents unlink-toolset");
        };
        assert_eq!(cmd.toolset, "ghidra");
        assert!(cmd.pin.is_none());
        // undelegate takes an agent and its sub-agent
        let Agents::Undelegate(cmd) = parse(&["undelegate", "triage", "research"]) else {
            panic!("expected agents undelegate");
        };
        assert_eq!(cmd.sub_agent, "research");
        // clear-harness takes an agent
        let Agents::ClearHarness(cmd) = parse(&["clear-harness", "triage"]) else {
            panic!("expected agents clear-harness");
        };
        assert_eq!(cmd.agent, "triage");
        // a pin must be an instance id and a sub-agent is required
        assert!(
            Args::try_parse_from(["thorctl", "agents", "unlink-tool", "a", "b", "--pin", "x"])
                .is_err()
        );
        assert!(Args::try_parse_from(["thorctl", "agents", "undelegate", "a"]).is_err());
    }

    /// `agents update` takes the typed flags and the shared edit flags
    #[test]
    fn update_args() {
        // the typed flags and the edit flags parse together
        let Agents::Update(cmd) = parse(&[
            "update",
            "triage",
            "--rename",
            "triage2",
            "-d",
            "checks files",
            "--add-group",
            "mars",
            "--remove-group",
            "minerva",
            "--config",
            "c.yaml",
            "--model-auth",
            "group",
            "--set",
            "config.max_turns=5",
            "--unset",
            "description",
            "--dry-run",
            "--yes",
            "-o",
            "json",
            "--extend-target-groups",
        ]) else {
            panic!("expected agents update");
        };
        assert_eq!(cmd.agent, "triage");
        assert_eq!(cmd.rename.as_deref(), Some("triage2"));
        assert_eq!(cmd.description.as_deref(), Some("checks files"));
        assert_eq!(cmd.add_group, vec!["mars"]);
        assert_eq!(cmd.remove_group, vec!["minerva"]);
        assert_eq!(
            cmd.config
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            Some("c.yaml".to_owned())
        );
        assert_eq!(cmd.model_auth, vec![ModelAuthArg::Group]);
        assert_eq!(cmd.edit.ops.len(), 2);
        assert!(cmd.edit.dry_run && cmd.edit.skip_confirm);
        assert_eq!(cmd.edit.format, Some(OutputFormat::Json));
        assert!(cmd.extend_target_groups);
        // the clear flags parse
        let Agents::Update(cmd) = parse(&[
            "update",
            "triage",
            "--clear-description",
            "--clear-model-auth",
        ]) else {
            panic!("expected agents update");
        };
        assert!(cmd.clear_description && cmd.clear_model_auth);
    }

    /// The typed update flags that contradict each other or a whole-document source are refused
    #[test]
    fn update_args_conflict() {
        let base = ["thorctl", "agents", "update", "triage"];
        let with = |extra: &[&str]| {
            Args::try_parse_from(base.iter().copied().chain(extra.iter().copied()))
        };
        // a description cannot be set and cleared
        assert!(with(&["-d", "x", "--clear-description"]).is_err());
        // model auth cannot be set and cleared
        assert!(with(&["--model-auth", "user", "--clear-model-auth"]).is_err());
        // group changes are relative to the current groups, which --file replaces
        assert!(with(&["--add-group", "g", "--file", "f.yaml"]).is_err());
        assert!(with(&["--remove-group", "g", "--file", "f.yaml"]).is_err());
    }

    /// `agents edit` takes the shared edit flags, and `spec` takes an output path and a format
    #[test]
    fn edit_and_spec_args() {
        // edit takes the shared edit flags
        let Agents::Edit(cmd) = parse(&[
            "edit",
            "triage",
            "--editor",
            "vim",
            "--set",
            "a=b",
            "--force",
            "--extend-target-groups",
        ]) else {
            panic!("expected agents edit");
        };
        assert_eq!(cmd.agent, "triage");
        assert_eq!(cmd.edit.editor.as_deref(), Some("vim"));
        assert!(cmd.edit.force && cmd.extend_target_groups);
        assert_eq!(cmd.edit.ops.len(), 1);
        // spec takes an output path and a format
        let Agents::Spec(cmd) = parse(&["spec", "triage", "-o", "a.yaml", "--format", "json"])
        else {
            panic!("expected agents spec");
        };
        assert_eq!(cmd.agent, "triage");
        assert_eq!(
            cmd.output.map(|path| path.to_string_lossy().into_owned()),
            Some("a.yaml".to_owned())
        );
        assert_eq!(cmd.format.format, Some(OutputFormat::Json));
    }

    /// `agents delete` takes an agent and the usual confirmation flags
    #[test]
    fn delete_args() {
        // the agent alone asks for confirmation
        let Agents::Delete(cmd) = parse(&["delete", "triage"]) else {
            panic!("expected agents delete");
        };
        assert_eq!(cmd.agent, "triage");
        assert!(!cmd.skip_confirm && !cmd.dry_run);
        // --yes is an alias of --skip-confirm
        let Agents::Delete(cmd) = parse(&["delete", "triage", "--yes", "--dry-run"]) else {
            panic!("expected agents delete");
        };
        assert!(cmd.skip_confirm && cmd.dry_run);
        let Agents::Delete(cmd) = parse(&["delete", "triage", "--skip-confirm"]) else {
            panic!("expected agents delete");
        };
        assert!(cmd.skip_confirm);
        // an agent is required
        assert!(Args::try_parse_from(["thorctl", "agents", "delete"]).is_err());
    }

    /// `agents set-harness` pins a harness instance only with `--pin`
    #[test]
    fn set_harness_args() {
        // without a pin the agent follows the harness's latest version
        let Agents::SetHarness(cmd) = parse(&["set-harness", "triage", "pal-harness"]) else {
            panic!("expected agents set-harness");
        };
        assert_eq!(
            (cmd.agent.as_str(), cmd.harness.as_str()),
            ("triage", "pal-harness")
        );
        assert!(cmd.pin.is_none() && !cmd.extend_target_groups);
        // a pin must be a tool instance id
        let id = "69ff8820-f688-4968-bd6f-45a4cb0f78a9";
        let Agents::SetHarness(cmd) = parse(&[
            "set-harness",
            "triage",
            "pal-harness",
            "--pin",
            id,
            "--extend-target-groups",
        ]) else {
            panic!("expected agents set-harness");
        };
        assert_eq!(cmd.pin.map(|pin| pin.to_string()), Some(id.to_owned()));
        assert!(cmd.extend_target_groups);
        let bad = [
            "thorctl",
            "agents",
            "set-harness",
            "a",
            "b",
            "--pin",
            "nope",
        ];
        assert!(Args::try_parse_from(bad).is_err());
        let missing = ["thorctl", "agents", "set-harness", "a"];
        assert!(Args::try_parse_from(missing).is_err());
    }

    /// `agents link-tool` pins an instance only with `--pin`, and `delegate` takes two agents
    #[test]
    fn link_and_delegate_args() {
        // link-tool without a pin follows the latest version
        let Agents::LinkTool(cmd) = parse(&["link-tool", "triage", "yara"]) else {
            panic!("expected agents link-tool");
        };
        assert_eq!((cmd.agent.as_str(), cmd.tool.as_str()), ("triage", "yara"));
        assert!(cmd.pin.is_none() && !cmd.extend_target_groups);
        // a pin must be a tool instance id
        let id = "69ff8820-f688-4968-bd6f-45a4cb0f78a9";
        let Agents::LinkTool(cmd) = parse(&[
            "link-tool",
            "triage",
            "yara",
            "--pin",
            id,
            "--extend-target-groups",
        ]) else {
            panic!("expected agents link-tool");
        };
        assert_eq!(cmd.pin.map(|pin| pin.to_string()), Some(id.to_owned()));
        assert!(cmd.extend_target_groups);
        let bad = ["thorctl", "agents", "link-tool", "a", "b", "--pin", "nope"];
        assert!(Args::try_parse_from(bad).is_err());
        // delegate takes the agent and its sub-agent
        let Agents::Delegate(cmd) = parse(&["delegate", "triage", "research"]) else {
            panic!("expected agents delegate");
        };
        assert_eq!(cmd.sub_agent, "research");
    }

    /// `agents link-toolset` pins a toolset instance only with `--pin`
    #[test]
    fn link_toolset_args() {
        // without a pin the agent follows the toolset's latest instance
        let Agents::LinkToolset(cmd) = parse(&["link-toolset", "triage", "ghidra"]) else {
            panic!("expected agents link-toolset");
        };
        assert_eq!(
            (cmd.agent.as_str(), cmd.toolset.as_str()),
            ("triage", "ghidra")
        );
        assert!(cmd.pin.is_none() && !cmd.extend_target_groups);
        // a pin must be a toolset instance id
        let id = "69ff8820-f688-4968-bd6f-45a4cb0f78a9";
        let Agents::LinkToolset(cmd) = parse(&["link-toolset", "triage", "ghidra", "--pin", id])
        else {
            panic!("expected agents link-toolset");
        };
        assert_eq!(cmd.pin.map(|pin| pin.to_string()), Some(id.to_owned()));
        let bad = [
            "thorctl",
            "agents",
            "link-toolset",
            "a",
            "b",
            "--pin",
            "nope",
        ];
        assert!(Args::try_parse_from(bad).is_err());
    }
}
