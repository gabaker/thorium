//! Arguments for workflow Thorctl commands

use clap::{Parser, ValueEnum};
use std::path::PathBuf;
use thorium::models::ModelAuthLevel;

use super::agents::ModelAuthArg;
use super::helpers::{EditArgs, FormatArgs, ShortFormatArgs, WaitForArgs, WatchArgs};
pub use super::runs::RunWorkflow;
use super::toolsets::Switch;

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
    /// Change a workflow from flags, a file, or `--set`, `--unset` and `--patch` edits
    #[clap(version, author)]
    Update(UpdateWorkflow),
    /// Edit a workflow in your editor, or with `--file`, `--set`, `--unset` and `--patch`
    #[clap(version, author)]
    Edit(EditWorkflow),
    /// Print a workflow as a spec that `workflows update --file` reads back
    #[clap(version, author)]
    Spec(SpecWorkflow),
    /// Delete a workflow
    #[clap(version, author)]
    Delete(DeleteWorkflow),
    /// Run a workflow on files or repos: by name for its latest instance, as WORKFLOW#rN for one
    /// revision, or by instance ID
    #[clap(version, author)]
    Run(RunWorkflow),
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
    #[clap(required_unless_present = "spec")]
    pub name: Option<String>,
    /// The groups to create the workflow in
    #[clap(short, long, required_unless_present = "spec")]
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
    /// Create the workflow from a YAML or JSON spec, as `workflows spec` prints it (- reads
    /// stdin); flags replace what the spec sets
    #[clap(long, value_name = "FILE|-")]
    pub spec: Option<PathBuf>,
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

/// A command to change a workflow without an editor
#[derive(Parser, Debug)]
// each flag is its own switch on the command line
#[allow(clippy::struct_excessive_bools)]
pub struct UpdateWorkflow {
    /// The workflow to update, by ID or name
    pub workflow: String,
    /// Rename the workflow
    #[clap(long)]
    pub rename: Option<String>,
    /// Set the workflow's description
    #[clap(short, long, conflicts_with = "clear_description")]
    pub description: Option<String>,
    /// Remove the workflow's description
    #[clap(long)]
    pub clear_description: bool,
    /// Add the workflow to a group; repeatable
    #[clap(long, conflicts_with = "file")]
    pub add_group: Vec<String>,
    /// Remove the workflow from a group; repeatable
    #[clap(long, conflicts_with = "file")]
    pub remove_group: Vec<String>,
    /// The most sub-agent invocations a run of the workflow has going at once
    #[clap(long)]
    pub max_alive_sub_agents: Option<u32>,
    /// Replace the default models with these model ids, highest priority first; repeatable
    #[clap(long, conflicts_with = "clear_default_models")]
    pub default_model: Vec<String>,
    /// Remove every default model
    #[clap(long)]
    pub clear_default_models: bool,
    /// Limit the levels that may supply model auth for the workflow's runs; repeatable
    #[clap(
        long,
        value_enum,
        ignore_case = true,
        conflicts_with = "clear_model_auth"
    )]
    pub model_auth: Vec<ModelAuthArg>,
    /// Allow every level to supply model auth for the workflow's runs
    #[clap(long)]
    pub clear_model_auth: bool,
    /// Build new workflow instances automatically when the workflow or its agents change
    #[clap(long, value_enum, value_name = "on|off")]
    pub auto_instance: Option<Switch>,
    /// Add the workflow's groups to the agents that miss some of them
    #[clap(long)]
    pub extend_target_groups: bool,
    /// The shared edit flags
    #[clap(flatten)]
    pub edit: EditArgs,
}

/// A command to edit a workflow
#[derive(Parser, Debug)]
pub struct EditWorkflow {
    /// The workflow to edit, by ID or name
    pub workflow: String,
    /// Add the workflow's groups to the agents that miss some of them
    #[clap(long)]
    pub extend_target_groups: bool,
    /// The shared edit flags
    #[clap(flatten)]
    pub edit: EditArgs,
}

/// A command to print a workflow as a spec
#[derive(Parser, Debug)]
pub struct SpecWorkflow {
    /// The workflow to print, by ID or name
    pub workflow: String,
    /// Write the spec to this file instead of stdout
    #[clap(short, long)]
    pub output: Option<PathBuf>,
    /// The format to print the spec in: yaml (the default) or json
    #[clap(flatten)]
    pub format: FormatArgs,
}

/// A command to delete a workflow
#[derive(Parser, Debug)]
pub struct DeleteWorkflow {
    /// The workflow to delete, by ID or name
    pub workflow: String,
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

    use super::{AuthLevel, Workflows};
    use crate::args::agents::ModelAuthArg;
    use crate::args::toolsets::Switch;
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
        assert_eq!(cmd.name.as_deref(), Some("triage"));
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

    /// `workflows create` takes a spec instead of a name and groups, and flags beside it
    #[test]
    fn create_from_spec_args() {
        // a spec alone is enough
        let Workflows::Create(cmd) = parse(&["create", "--spec", "-"]).unwrap() else {
            panic!("expected workflows create");
        };
        assert_eq!(cmd.spec.as_deref(), Some(std::path::Path::new("-")));
        assert_eq!(cmd.name, None);
        assert_eq!(cmd.group, Vec::<String>::new());
        // flags go beside a spec
        let Workflows::Create(cmd) = parse(&[
            "create", "copy", "--spec", "w.yaml", "-g", "mars", "--agent", "late",
        ])
        .unwrap() else {
            panic!("expected workflows create");
        };
        assert_eq!(cmd.name.as_deref(), Some("copy"));
        assert_eq!(cmd.group, vec!["mars"]);
        assert_eq!(cmd.agent, vec!["late"]);
        // without a spec a name and a group are still required
        assert!(parse(&["create"]).is_err());
        assert!(parse(&["create", "x"]).is_err());
        assert!(parse(&["create", "-g", "minerva"]).is_err());
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

    /// `workflows update` takes typed flags and the shared edit flags, and refuses contradictions
    #[test]
    fn update_args() {
        // every typed flag and the shared edit flags parse
        let Workflows::Update(cmd) = parse(&[
            "update",
            "triage",
            "--rename",
            "sorting",
            "-d",
            "sorts files",
            "--add-group",
            "mars",
            "--remove-group",
            "minerva",
            "--max-alive-sub-agents",
            "4",
            "--default-model",
            "claude-sonnet",
            "--default-model",
            "gpt-5",
            "--model-auth",
            "user",
            "--auto-instance",
            "off",
            "--extend-target-groups",
            "--set",
            "limits.max_alive_sub_agents=9",
            "--unset",
            "entry_agent",
            "--dry-run",
            "--skip-confirm",
            "-o",
            "json",
        ])
        .unwrap() else {
            panic!("expected workflows update");
        };
        assert_eq!(cmd.workflow, "triage");
        assert_eq!(cmd.rename.as_deref(), Some("sorting"));
        assert_eq!(cmd.description.as_deref(), Some("sorts files"));
        assert_eq!(cmd.add_group, vec!["mars"]);
        assert_eq!(cmd.remove_group, vec!["minerva"]);
        assert_eq!(cmd.max_alive_sub_agents, Some(4));
        assert_eq!(cmd.default_model, vec!["claude-sonnet", "gpt-5"]);
        assert_eq!(cmd.model_auth, vec![ModelAuthArg::User]);
        assert_eq!(cmd.auto_instance, Some(Switch::Off));
        assert!(cmd.extend_target_groups);
        assert_eq!(cmd.edit.ops.len(), 2);
        assert!(cmd.edit.dry_run && cmd.edit.skip_confirm);
        assert_eq!(cmd.edit.format, Some(OutputFormat::Json));
        // the clearing flags parse on their own
        let Workflows::Update(cmd) = parse(&[
            "update",
            "triage",
            "--clear-description",
            "--clear-default-models",
            "--clear-model-auth",
        ])
        .unwrap() else {
            panic!("expected workflows update");
        };
        assert!(cmd.clear_description && cmd.clear_default_models && cmd.clear_model_auth);
        // contradictions are refused
        assert!(parse(&["update", "triage", "-d", "x", "--clear-description"]).is_err());
        assert!(
            parse(&[
                "update",
                "triage",
                "--default-model",
                "m",
                "--clear-default-models"
            ])
            .is_err()
        );
        assert!(
            parse(&[
                "update",
                "triage",
                "--model-auth",
                "user",
                "--clear-model-auth"
            ])
            .is_err()
        );
        // group changes are relative to the current groups, which --file replaces
        assert!(parse(&["update", "triage", "--add-group", "g", "--file", "f.yaml"]).is_err());
        // a workflow is required, and an unknown level is refused
        assert!(parse(&["update"]).is_err());
        assert!(parse(&["update", "triage", "--model-auth", "nobody"]).is_err());
        assert!(parse(&["update", "triage", "--auto-instance", "maybe"]).is_err());
    }

    /// `workflows edit` takes the shared edit flags, and `workflows spec` a file and a format
    #[test]
    fn edit_and_spec_args() {
        // edit takes the editor, the widening flag and the shared flags
        let Workflows::Edit(cmd) = parse(&[
            "edit",
            "triage",
            "--editor",
            "nano",
            "--extend-target-groups",
            "--patch",
            "{\"description\": null}",
            "--force",
        ])
        .unwrap() else {
            panic!("expected workflows edit");
        };
        assert_eq!(cmd.workflow, "triage");
        assert_eq!(cmd.edit.editor.as_deref(), Some("nano"));
        assert!(cmd.extend_target_groups && cmd.edit.force);
        assert_eq!(cmd.edit.patch.len(), 1);
        assert!(parse(&["edit"]).is_err());
        // spec takes an output file and a format
        let Workflows::Spec(cmd) =
            parse(&["spec", "triage", "-o", "a.yaml", "--format", "json"]).unwrap()
        else {
            panic!("expected workflows spec");
        };
        assert_eq!(cmd.workflow, "triage");
        assert_eq!(cmd.output.as_deref(), Some(std::path::Path::new("a.yaml")));
        assert_eq!(cmd.format.format, Some(OutputFormat::Json));
        assert!(parse(&["spec"]).is_err());
    }

    /// `workflows delete` takes a workflow, a dry run, and `--skip-confirm` or its `--yes` alias
    #[test]
    fn delete_args() {
        // a workflow alone asks before deleting
        let Workflows::Delete(cmd) = parse(&["delete", "triage"]).unwrap() else {
            panic!("expected workflows delete");
        };
        assert_eq!(cmd.workflow, "triage");
        assert!(!cmd.skip_confirm && !cmd.dry_run);
        // the flag and its alias skip the question, and a dry run stops before deleting
        let Workflows::Delete(cmd) = parse(&["delete", "triage", "--yes", "--dry-run"]).unwrap()
        else {
            panic!("expected workflows delete");
        };
        assert!(cmd.skip_confirm && cmd.dry_run);
        let Workflows::Delete(cmd) = parse(&["delete", "triage", "--skip-confirm"]).unwrap() else {
            panic!("expected workflows delete");
        };
        assert!(cmd.skip_confirm);
        // a workflow is required
        assert!(parse(&["delete"]).is_err());
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
