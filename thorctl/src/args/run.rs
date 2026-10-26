//! Arguments for run-related Thorctl commands

use clap::Parser;
use std::path::PathBuf;

use super::runs::{RunPipeline, RunTool, RunWorkflow};

/// The commands to start a run: a tool, workflow, or pipeline run, or the older single pipeline run
///
/// `thorctl run tool|workflow|pipeline <NAME>` starts a run of that kind with the flags every run
/// command takes. The older `thorctl run <PIPELINE> <SHA256|REPO>` form keeps working; it is
/// chosen when the first word is not a kind, so a pipeline named like a kind must be run with
/// `thorctl pipelines run`.
#[derive(Parser, Debug)]
#[clap(
    args_conflicts_with_subcommands = true,
    override_usage = "thorctl run <tool|workflow|pipeline> <NAME> [OPTIONS]\n       thorctl run <PIPELINE> <SHA256_OR_REPO> [OPTIONS]",
    after_help = "Older form: thorctl run <PIPELINE> <SHA256|REPO> [--group <GROUP>] [--sla <SECONDS>] \
[--output <PATH>] creates one reaction and prints each stage's output as it runs.\n\nFor many inputs \
use `--one-per-input` (one run per file or repo, tagged with a shared `--batch` name); `thorctl runs \
get --batch <NAME>` lists a batch and `thorctl runs cancel --batch <NAME>` cancels it."
)]
pub struct RootRun {
    /// The kind of run to start
    #[clap(subcommand)]
    pub kind: Option<RunKinds>,
    /// The older single pipeline run
    #[clap(flatten)]
    pub legacy: Option<Run>,
}

/// The kinds of run `thorctl run` starts
///
/// The structs are the ones `tools run`, `workflows run`, and `pipelines run` take, so both
/// spellings of a command parse and run the same way.
#[derive(Parser, Debug)]
pub enum RunKinds {
    /// Run a tool instance, or a tool's latest version (the same as `thorctl tools run`)
    #[clap(version, author)]
    Tool(RunTool),
    /// Run a workflow instance, or a workflow's latest (the same as `thorctl workflows run`)
    #[clap(version, author)]
    Workflow(RunWorkflow),
    /// Run a pipeline (legacy: the same as `thorctl pipelines run`; prefer tools and workflows)
    #[clap(version, author)]
    Pipeline(RunPipeline),
}

/// The older command to run one pipeline on one file or repo
#[derive(clap::Args, Debug)]
pub struct Run {
    /// The pipeline to run
    #[clap(value_name = "PIPELINE")]
    pub pipeline: String,
    /// The sample SHA256 or repo to run the pipeline on
    #[clap(value_name = "SHA256_OR_REPO")]
    pub sha256_or_repo: String,
    /// The group that the pipeline is in (required if a pipeline with the same
    /// name exists in another group)
    #[clap(long, help_heading = "Older form")]
    pub group: Option<String>,
    /// How many seconds allowed to wait before starting the job
    #[clap(short, long, default_value_t = 1, help_heading = "Older form")]
    pub sla: u64,
    /// The path to save the results to [default: `<SHA256/REPO>_<PIPELINE>`]
    #[clap(short, long, help_heading = "Older form")]
    pub output: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};
    use std::time::Duration;

    use super::{RootRun, RunKinds};
    use crate::args::{Args, SubCommands};

    /// Parse a `thorctl run` command line into the root run command
    ///
    /// # Arguments
    ///
    /// * `argv` - The arguments after `thorctl run`
    fn parse(argv: &[&str]) -> Result<RootRun, String> {
        let mut full = vec!["thorctl", "run"];
        full.extend_from_slice(argv);
        match Args::try_parse_from(full) {
            Ok(Args {
                cmd: SubCommands::Run(run),
                ..
            }) => Ok(run),
            Ok(_) => Err("not a run".to_owned()),
            Err(err) => Err(err.to_string()),
        }
    }

    /// Render the help of a command found by its path below the root
    ///
    /// # Arguments
    ///
    /// * `path` - The subcommand names to walk down from `thorctl`
    fn help(path: &[&str]) -> String {
        // walk down to the command and render its help
        let mut command = Args::command();
        let mut found = &mut command;
        for name in path {
            found = found.find_subcommand_mut(name).expect("a subcommand");
        }
        found.render_help().to_string()
    }

    /// The legacy `run` help points at the commands that take the common run flags
    #[test]
    fn run_help_points_at_the_run_commands() {
        // render the root command's help
        let help = help(&["run"]);
        // it names each kind and how the older form still works
        for expected in [
            "tool",
            "workflow",
            "pipeline",
            "Older form",
            "<SHA256|REPO>",
        ] {
            assert!(help.contains(expected), "{expected} missing from {help}");
        }
        // every flag the older form had keeps working
        for flag in ["--group", "--sla", "--output"] {
            assert!(help.contains(flag), "{flag} missing from {help}");
        }
    }

    /// The help of `run` is the same as the golden text: a changed help is reviewed on purpose
    #[test]
    fn run_help_matches_the_golden() {
        // the kinds are listed with what each does, and the pipeline kind says it is legacy
        let help = help(&["run"]);
        let golden = include_str!("../../tests/data/run_help.golden");
        assert_eq!(help.trim_end(), golden.trim_end(), "{help}");
    }

    /// The top-level help names real commands, including the ones for agentic workflows
    #[test]
    fn top_level_help_names_real_commands() {
        let help = help(&[]);
        // the commands that exist are listed
        for command in [
            "tools",
            "workflows",
            "runs",
            "run",
            "models",
            "reactions",
            "files",
        ] {
            assert!(
                help.contains(&format!("  {command}")),
                "{command} missing from {help}"
            );
        }
        // the flag's own text names no command that doesn't exist
        for gone in ["install", "admins"] {
            assert!(!help.contains(gone), "{gone} should not be named in {help}");
        }
    }

    /// No top-level command or alias is named `analyze`, `si`, or `ai`, which other builds reserve
    #[test]
    fn run_commands_do_not_collide_with_reserved_names() {
        let command = Args::command();
        // collect every name and alias at the top level
        let mut names = Vec::new();
        for sub in command.get_subcommands() {
            names.push(sub.get_name().to_owned());
            names.extend(sub.get_all_aliases().map(ToOwned::to_owned));
        }
        // `analyze`, `si`, and `ai` are reserved, so no command here may use them
        for taken in ["analyze", "si"] {
            assert!(!names.iter().any(|name| name == taken), "{taken} is used");
        }
        // none of the run kinds or their aliases are those names either
        let run = command.find_subcommand("run").expect("a run command");
        for sub in run.get_subcommands() {
            for name in std::iter::once(sub.get_name()).chain(sub.get_all_aliases()) {
                assert!(!["analyze", "ai", "si"].contains(&name), "{name} is used");
            }
        }
    }

    /// Each kind parses into the structs its own command takes, with every shared flag
    #[test]
    fn each_kind_parses_with_the_shared_flags() {
        // a tool with its own flags and the batch flags
        let run = parse(&[
            "tool",
            "scan@1.0.0",
            "--sample",
            "aa",
            "--one-per-input",
            "--batch",
            "nightly",
            "--dry-run",
            "--arg=-v",
            "--wait",
        ])
        .unwrap();
        let Some(RunKinds::Tool(tool)) = run.kind else {
            panic!("expected a tool run");
        };
        assert_eq!(tool.tool_instance, "scan@1.0.0");
        assert_eq!(tool.args, ["-v"]);
        let common = tool.common;
        assert!(common.one_per_input && common.dry_run && common.wait);
        assert_eq!(common.batch.as_deref(), Some("nightly"));
        assert_eq!(common.batch_name().as_deref(), Some("nightly"));
        // a workflow with the shared flags
        let run = parse(&["workflow", "triage#r2", "--sample-list", "-", "-g", "acme"]).unwrap();
        let Some(RunKinds::Workflow(workflow)) = run.kind else {
            panic!("expected a workflow run");
        };
        assert_eq!(workflow.instance, "triage#r2");
        assert_eq!(
            workflow.common.inputs.sample_list.as_deref(),
            Some(std::path::Path::new("-"))
        );
        assert_eq!(workflow.common.group.as_deref(), Some("acme"));
        // a pipeline with its parent
        let parent = "6a3e2f9c-1b7d-4a58-9c0e-3d5f7a1b2c4d";
        let run = parse(&[
            "pipeline",
            "static/harvest",
            "--repo-list",
            "-",
            "--parent",
            parent,
        ])
        .unwrap();
        let Some(RunKinds::Pipeline(pipeline)) = run.kind else {
            panic!("expected a pipeline run");
        };
        assert_eq!(pipeline.pipeline, "static/harvest");
        assert_eq!(pipeline.parent.unwrap().to_string(), parent);
    }

    /// A kind needs its name, and kind-only flags stay on their kind
    #[test]
    fn a_kind_needs_its_name_and_its_own_flags() {
        // a kind without a name is refused
        for kind in ["tool", "workflow", "pipeline"] {
            assert!(parse(&[kind]).is_err(), "{kind} without a name");
        }
        // tool flags are not workflow flags
        assert!(parse(&["workflow", "wf", "--arg", "x"]).is_err());
        assert!(
            parse(&[
                "workflow",
                "wf",
                "--parent",
                "6a3e2f9c-1b7d-4a58-9c0e-3d5f7a1b2c4d"
            ])
            .is_err()
        );
    }

    /// The older two word form still parses, and a bare `run` parses to nothing to run
    #[test]
    fn the_older_form_still_parses() {
        // a pipeline and a target with the flags it always had
        let run = parse(&[
            "harvest", "abc", "--group", "corn", "--sla", "5", "-o", "out",
        ])
        .unwrap();
        assert!(run.kind.is_none());
        let legacy = run.legacy.expect("the older form");
        assert_eq!(legacy.pipeline, "harvest");
        assert_eq!(legacy.sha256_or_repo, "abc");
        assert_eq!(legacy.group.as_deref(), Some("corn"));
        assert_eq!(legacy.sla, 5);
        // a lone word is neither form, and nothing at all names no kind: both are refused with
        // a usage that names the kinds
        for argv in [&["harvest"][..], &[]] {
            let err = parse(argv).unwrap_err();
            assert!(err.contains("<tool|workflow|pipeline> <NAME>"), "{err}");
        }
    }

    /// The shared flags parse the same way after `run <kind>` as after the older spellings
    #[test]
    fn every_spelling_takes_the_batch_flags() {
        let flags = [
            "--sample",
            "aa",
            "--one-per-input",
            "--batch",
            "b1",
            "--dry-run",
        ];
        let spellings: [&[&str]; 6] = [
            &["run", "tool", "scan@1.0.0"],
            &["tools", "run", "scan@1.0.0"],
            &["run", "workflow", "triage"],
            &["workflows", "run", "triage"],
            &["run", "pipeline", "harvest"],
            &["pipelines", "run", "harvest"],
        ];
        for spelling in spellings {
            let mut argv = vec!["thorctl"];
            argv.extend_from_slice(spelling);
            argv.extend_from_slice(&flags);
            Args::try_parse_from(argv).unwrap_or_else(|err| panic!("{spelling:?}: {err}"));
        }
    }

    /// A fan-out follows the whole batch, and flags that need one run are refused with it
    #[test]
    fn fan_out_flag_rules() {
        // waiting and watching follow a batch
        for flag in ["--wait", "--watch"] {
            let run =
                parse(&["workflow", "wf", "--sample", "aa", "--one-per-input", flag]).unwrap();
            let Some(RunKinds::Workflow(workflow)) = run.kind else {
                panic!("expected a workflow run");
            };
            assert!(workflow.common.follows_batch(), "{flag}");
        }
        // --output implies waiting for the batch
        let run = parse(&["workflow", "wf", "--one-per-input", "--output", "d"]).unwrap();
        let Some(RunKinds::Workflow(workflow)) = run.kind else {
            panic!("expected a workflow run");
        };
        assert!(workflow.common.follows_batch());
        // a run's logs and a wait-for condition are for one run
        for flags in [&["--logs"][..], &["--wait-for", "status=Completed"]] {
            let mut argv = vec!["workflow", "wf", "--one-per-input"];
            argv.extend_from_slice(flags);
            assert!(parse(&argv).is_err(), "{flags:?}");
        }
        // a batch name is generated for a fan-out only
        let common = |flags: &[&str]| {
            let mut argv = vec!["workflow", "wf"];
            argv.extend_from_slice(flags);
            let Some(RunKinds::Workflow(workflow)) = parse(&argv).unwrap().kind else {
                panic!("expected a workflow run");
            };
            workflow.common
        };
        assert!(common(&[]).batch_name().is_none());
        let generated = common(&["--one-per-input"]).batch_name().unwrap();
        assert!(uuid::Uuid::parse_str(&generated).is_ok(), "{generated}");
        // a timeout and an interval keep their defaults
        let defaults = common(&["--one-per-input", "--wait"]);
        assert_eq!(defaults.wait_for.timeout, Duration::from_secs(600));
        assert_eq!(defaults.watch.interval, Duration::from_secs(2));
    }

    /// The help of each run command says `--timeout` and `--interval` also bound and pace `--wait`
    #[test]
    fn run_help_says_the_timeout_bounds_every_wait() {
        // check the help of each kind of run
        for path in [
            &["run", "tool"][..],
            &["run", "workflow"],
            &["run", "pipeline"],
        ] {
            // render the command's help
            let help = help(path);
            // the timeout line names the waits it bounds, not only --wait-for
            let timeout = help
                .lines()
                .skip_while(|line| !line.contains("--timeout"))
                .take(3)
                .collect::<String>();
            assert!(timeout.contains("--wait`"), "{path:?}: {timeout}");
            // the interval line names --wait too
            let interval = help
                .lines()
                .skip_while(|line| !line.contains("--interval"))
                .take(3)
                .collect::<String>();
            assert!(interval.contains("--wait`"), "{path:?}: {interval}");
        }
    }

    /// The help of `tools runs results` names the directory results go to without `--output`
    #[test]
    fn tool_run_results_help_names_the_default_directory() {
        // render the command's help
        let help = help(&["tools", "runs", "results"]);
        // the output flag names the default directory
        assert!(
            help.contains("/tmp/thorium/tool-results/<REACTION>"),
            "{help}"
        );
    }
}
