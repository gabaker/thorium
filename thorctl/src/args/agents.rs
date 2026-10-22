//! Arguments for AI agent Thorctl commands

use clap::Parser;
use std::path::PathBuf;
use uuid::Uuid;

use super::helpers::{ShortFormatArgs, WaitForArgs, WatchArgs};

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

/// A command to create an AI agent
#[derive(Parser, Debug)]
pub struct CreateAgent {
    /// The path to the AI agent's config, as YAML or JSON
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

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::Agents;
    use crate::args::{Args, SubCommands};
    use crate::utils::output::OutputFormat;

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
