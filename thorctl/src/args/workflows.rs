//! Arguments for workflow Thorctl commands

use clap::{Parser, ValueEnum};
use thorium::models::ModelAuthLevel;

use super::helpers::{ShortFormatArgs, WaitForArgs, WatchArgs};

/// The commands to send to the workflows task handler
#[derive(Parser, Debug)]
pub enum Workflows {
    /// List the workflows in your groups
    #[clap(version, author)]
    Get(GetWorkflows),
    /// Describe a workflow: its agents, entry agent, settings, and latest instance
    #[clap(version, author)]
    Describe(DescribeWorkflow),
    /// Create a workflow that wires AI agents together
    #[clap(version, author)]
    Create(CreateWorkflow),
    /// Add an AI agent to a workflow
    #[clap(version, author)]
    AddAgent(AddAgent),
    /// Remove an AI agent from a workflow
    #[clap(version, author)]
    RemoveAgent(RemoveAgent),
    /// Set or clear the AI agent that receives a workflow's inputs first
    #[clap(version, author)]
    Entry(SetEntry),
    /// Show one AI agent of a workflow instance and the full spec of each of its tools
    #[clap(version, author)]
    Toolset(ShowToolset),
}

/// A command to list workflows
#[derive(Parser, Debug)]
pub struct GetWorkflows {
    /// The groups to list workflows from (all of your groups by default)
    #[clap(short, long)]
    pub group: Vec<String>,
    /// The most workflows to list
    #[clap(short, long, default_value_t = 50)]
    pub limit: usize,
    /// The format to print the workflows in (a table by default)
    #[clap(flatten)]
    pub format: ShortFormatArgs,
    /// Re-read and re-print the workflows until interrupted
    #[clap(flatten)]
    pub watch: WatchArgs,
    /// Wait until every listed workflow matches a condition
    #[clap(flatten)]
    pub wait: WaitForArgs,
}

/// A command to describe a workflow
#[derive(Parser, Debug)]
pub struct DescribeWorkflow {
    /// The workflow to describe, by ID or name
    pub workflow: String,
    /// The format to describe the workflow in (human readable by default)
    #[clap(flatten)]
    pub format: ShortFormatArgs,
    /// Re-read and re-describe the workflow until interrupted
    #[clap(flatten)]
    pub watch: WatchArgs,
    /// Wait until the workflow matches a condition
    #[clap(flatten)]
    pub wait: WaitForArgs,
}

/// A level that may supply model auth for a workflow's runs
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthLevel {
    /// Auth the harness supplies itself
    Harness,
    /// Auth configured on the workflow
    Workflow,
    /// The endpoints in the run's group with their group credentials
    Group,
    /// The runner's own credentials and endpoints
    User,
}

impl From<AuthLevel> for ModelAuthLevel {
    /// Convert a command line auth level to the API's
    ///
    /// # Arguments
    ///
    /// * `level` - The command line auth level
    fn from(level: AuthLevel) -> Self {
        match level {
            AuthLevel::Harness => ModelAuthLevel::Harness,
            AuthLevel::Workflow => ModelAuthLevel::Workflow,
            AuthLevel::Group => ModelAuthLevel::Group,
            AuthLevel::User => ModelAuthLevel::User,
        }
    }
}

/// A command to create a workflow
#[derive(Parser, Debug)]
pub struct CreateWorkflow {
    /// The name of the workflow
    pub name: String,
    /// The groups to create the workflow in
    #[clap(short, long, required = true)]
    pub group: Vec<String>,
    /// An AI agent in the workflow, by ID or name
    #[clap(long)]
    pub agent: Vec<String>,
    /// The AI agent that receives the workflow's inputs first, by ID or name (must be one of
    /// its agents)
    #[clap(long)]
    pub entry: Option<String>,
    /// A model id agents without their own models list accept, highest priority first
    #[clap(long)]
    pub default_model: Vec<String>,
    /// The most sub-agent invocations a run of the workflow has going at once
    #[clap(long)]
    pub max_alive_sub_agents: Option<u32>,
    /// A level that may supply model auth for the workflow's runs (every level by default)
    #[clap(long, value_enum)]
    pub model_auth: Vec<AuthLevel>,
    /// Don't build new workflow instances automatically when the workflow or its agents change
    #[clap(long)]
    pub no_auto_instance: bool,
    /// A description of the workflow
    #[clap(short, long)]
    pub description: Option<String>,
    /// Add the workflow's groups to agents that are missing some of them
    #[clap(long)]
    pub extend_target_groups: bool,
}

/// A command to add an AI agent to a workflow
#[derive(Parser, Debug)]
pub struct AddAgent {
    /// The workflow, by ID or name
    pub workflow: String,
    /// The AI agent to add, by ID or name
    pub agent: String,
    /// Add the workflow's groups to the agent when it's missing some of them
    #[clap(long)]
    pub extend_target_groups: bool,
}

/// A command to remove an AI agent from a workflow
#[derive(Parser, Debug)]
pub struct RemoveAgent {
    /// The workflow, by ID or name
    pub workflow: String,
    /// The AI agent to remove, by ID or name
    pub agent: String,
}

/// A command to set or clear a workflow's entry agent
#[derive(Parser, Debug)]
pub struct SetEntry {
    /// The workflow, by ID or name
    pub workflow: String,
    /// The AI agent that receives the workflow's inputs first, by ID or name
    #[clap(required_unless_present = "clear", conflicts_with = "clear")]
    pub agent: Option<String>,
    /// Clear the workflow's entry agent
    #[clap(long)]
    pub clear: bool,
}

/// A command to show one AI agent of a workflow instance and its tools' specs
#[derive(Parser, Debug)]
pub struct ShowToolset {
    /// The workflow instance, by ID or name
    pub instance: String,
    /// The AI agent in the instance, by ID or name
    #[clap(long, required = true)]
    pub agent: String,
    /// The format to print the toolset in (human readable by default)
    #[clap(flatten)]
    pub format: ShortFormatArgs,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{AuthLevel, Workflows};
    use crate::args::{Args, SubCommands};
    use crate::utils::output::OutputFormat;

    /// Parse thorctl arguments into a workflows command
    ///
    /// # Arguments
    ///
    /// * `args` - The arguments after `thorctl workflows`
    fn parse(args: &[&str]) -> Result<Workflows, clap::Error> {
        // prepend the binary and subcommand names
        let mut full = vec!["thorctl", "workflows"];
        full.extend_from_slice(args);
        // parse them into a workflows command
        match Args::try_parse_from(full)?.cmd {
            SubCommands::Workflows(workflows) => Ok(workflows),
            _ => panic!("expected a workflows command"),
        }
    }

    /// `workflows create` takes a name, groups, agents, an entry agent, and the settings
    #[test]
    fn create_args() {
        // every create flag parses
        let Workflows::Create(cmd) = parse(&[
            "create",
            "triage",
            "-g",
            "minerva",
            "--agent",
            "triage",
            "--agent",
            "reverser",
            "--entry",
            "triage",
            "--default-model",
            "claude-sonnet",
            "--default-model",
            "gpt-5",
            "--max-alive-sub-agents",
            "4",
            "--model-auth",
            "user",
            "--no-auto-instance",
        ])
        .unwrap() else {
            panic!("expected workflows create");
        };
        assert_eq!(cmd.name, "triage");
        assert_eq!(cmd.group, vec!["minerva"]);
        assert_eq!(cmd.agent, vec!["triage", "reverser"]);
        assert_eq!(cmd.entry.as_deref(), Some("triage"));
        assert_eq!(cmd.default_model, vec!["claude-sonnet", "gpt-5"]);
        assert_eq!(cmd.max_alive_sub_agents, Some(4));
        assert_eq!(cmd.model_auth, vec![AuthLevel::User]);
        assert!(cmd.no_auto_instance);
        // a group is required and model auth names a level
        assert!(parse(&["create", "x"]).is_err());
        assert!(
            parse(&[
                "create",
                "triage",
                "-g",
                "minerva",
                "--model-auth",
                "nobody"
            ])
            .is_err()
        );
    }

    /// The read and edit commands take exactly what they need
    #[test]
    fn read_and_edit_args() {
        // get takes groups and a format, and describe takes a workflow
        let Workflows::Get(cmd) = parse(&["get", "-g", "minerva", "-o", "json"]).unwrap() else {
            panic!("expected workflows get");
        };
        assert_eq!(cmd.format.format, Some(OutputFormat::Json));
        let Workflows::Describe(cmd) = parse(&["describe", "triage"]).unwrap() else {
            panic!("expected workflows describe");
        };
        assert_eq!(cmd.workflow, "triage");
        // add-agent and remove-agent name a workflow and an agent
        let Workflows::AddAgent(cmd) = parse(&["add-agent", "triage", "late"]).unwrap() else {
            panic!("expected workflows add-agent");
        };
        assert_eq!(cmd.agent, "late");
        assert!(parse(&["remove-agent", "triage"]).is_err());
        // entry takes an agent or --clear, but not both and not neither
        let Workflows::Entry(cmd) = parse(&["entry", "triage", "late"]).unwrap() else {
            panic!("expected workflows entry");
        };
        assert_eq!(cmd.agent.as_deref(), Some("late"));
        assert!(parse(&["entry", "triage", "--clear"]).is_ok());
        assert!(parse(&["entry", "triage"]).is_err());
        assert!(parse(&["entry", "triage", "late", "--clear"]).is_err());
        // toolset needs an agent
        let Workflows::Toolset(cmd) =
            parse(&["toolset", "triage#r1", "--agent", "triage"]).unwrap()
        else {
            panic!("expected workflows toolset");
        };
        assert_eq!(cmd.instance, "triage#r1");
        assert!(parse(&["toolset", "triage#r1"]).is_err());
    }

    /// The commands left out of thorctl don't exist: no direct instance requests, no resolver
    /// dry-runs, and no event or job commands
    #[test]
    fn excluded_commands_are_rejected() {
        // each excluded command fails to parse
        for args in [
            vec!["thorctl", "workflows", "instance", "triage"],
            vec!["thorctl", "toolsets", "instance", "kit"],
            vec!["thorctl", "workflows", "dry-run", "triage"],
            vec!["thorctl", "toolsets", "dry-run", "kit"],
            vec!["thorctl", "events", "peek"],
            vec!["thorctl", "events", "get"],
            vec!["thorctl", "jobs", "get"],
            vec!["thorctl", "jobs", "list"],
        ] {
            assert!(Args::try_parse_from(&args).is_err(), "{args:?}");
        }
    }
}
