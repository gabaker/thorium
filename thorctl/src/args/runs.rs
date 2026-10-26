//! Arguments for the run Thorctl commands

use clap::Parser;
use std::path::PathBuf;
use thorium::models::ReactionStatus;
use uuid::Uuid;

use super::helpers::{ShortFormatArgs, WaitForArgs, WatchArgs};

/// The commands to send to the runs task handler
#[derive(Parser, Debug)]
pub enum Runs {
    /// List the tool, workflow, or pipeline runs in your groups
    #[clap(version, author)]
    Get(GetRuns),
    /// Describe a tool or workflow run: its source, plan, and result key
    #[clap(version, author)]
    Describe(DescribeRun),
    /// Retry a failed workflow run as a new run on the same inputs
    #[clap(version, author)]
    Retry(RetryRun),
    /// Print the status updates and stage logs of a run
    #[clap(version, author)]
    Logs(LogsRun),
    /// Cancel a run that has not finished by failing it
    #[clap(version, author)]
    Cancel(CancelRun),
}

/// The files and repos a run is given
#[derive(clap::Args, Debug, Clone, Default)]
pub struct RunInputArgs {
    /// A file in Thorium to run on, by SHA256
    #[clap(long = "sample")]
    pub samples: Vec<String>,
    /// A local file to upload and run on
    #[clap(long = "file")]
    pub files: Vec<PathBuf>,
    /// A repo to run on, optionally with a commitish after a colon (`<REPO-URL>[:<COMMITISH>]`)
    #[clap(long = "repo")]
    pub repos: Vec<String>,
    /// A file of SHA256s to run on, one per line (a line may hold several separated by commas);
    /// `-` reads the list from stdin
    #[clap(long, value_name = "FILE")]
    pub sample_list: Option<PathBuf>,
    /// A file of repos to run on, one `<REPO-URL>[:<COMMITISH>]` per line; `-` reads the list from
    /// stdin
    #[clap(long, value_name = "FILE")]
    pub repo_list: Option<PathBuf>,
}

/// The flags every command that starts a run takes: its inputs, its reaction's settings, and how
/// to follow it
///
/// Without `--wait`, `--wait-for`, `--watch`, `--output`, or `--logs` the run is started and its
/// reaction id is printed. With `--one-per-input` each input gets its own run and the flags follow
/// the whole batch instead.
#[derive(clap::Args, Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct RunCommonArgs {
    /// The group to run in (needed when the target has several groups you can run in)
    #[clap(short, long)]
    pub group: Option<String>,
    /// The files and repos to run on
    #[clap(flatten)]
    pub inputs: RunInputArgs,
    /// A tag to add to the run's reaction
    #[clap(long)]
    pub tag: Vec<String>,
    /// The run's SLA in seconds
    #[clap(long)]
    pub sla: Option<u64>,
    /// Block until the run is Completed or Failed; exits 0 only when it Completed
    #[clap(long, conflicts_with_all = ["wait_for", "watch"])]
    pub wait: bool,
    /// Poll the run until a condition holds (e.g. status=Completed), then print it
    #[clap(flatten)]
    pub wait_for: WaitForArgs,
    /// Re-read and re-print the run until interrupted
    #[clap(flatten)]
    pub watch: WatchArgs,
    /// Download the run's results into this directory once it finishes (implies --wait)
    #[clap(long, value_name = "DIR", conflicts_with_all = ["wait_for", "watch"])]
    pub output: Option<PathBuf>,
    /// Print the status updates and stage logs once the run finishes (implies --wait)
    #[clap(long, conflicts_with_all = ["wait_for", "watch"])]
    pub logs: bool,
    /// The format to print the run in (human readable by default)
    #[clap(flatten)]
    pub format: ShortFormatArgs,
    /// Start one run per file or repo instead of one run on all of them
    ///
    /// Every run is tagged with the batch name, at most `--workers` runs start at once, a run that
    /// fails to start doesn't stop the others, and `--wait`, `--watch`, and `--output` follow the
    /// whole batch (exit 0 when every run completed, 1 when any failed, 3 when some had not
    /// finished before `--timeout`). With `-o json` one JSON line per input is printed.
    #[clap(long, conflicts_with_all = ["logs", "wait_for"])]
    pub one_per_input: bool,
    /// Tag every run with this batch name so the runs can be listed, followed, and cancelled
    /// together (`runs get --batch`, `runs cancel --batch`); generated with `--one-per-input` when
    /// omitted
    #[clap(long, value_name = "NAME")]
    pub batch: Option<String>,
    /// Show what would start without creating or uploading anything
    #[clap(long)]
    pub dry_run: bool,
}

impl RunCommonArgs {
    /// Whether the run must be followed until it finishes
    ///
    /// `--output` and `--logs` need a finished run, so they imply `--wait`.
    #[must_use]
    pub fn waits(&self) -> bool {
        self.wait || self.output.is_some() || self.logs
    }

    /// Whether the run is followed at all after it is started
    #[must_use]
    pub fn follows(&self) -> bool {
        self.waits() || self.wait_for.wait_for.is_some() || self.watch.watch
    }

    /// Whether a batch of runs is followed after it is started
    ///
    /// `--watch` follows a batch without a timeout, like `--wait` does with one.
    #[must_use]
    pub fn follows_batch(&self) -> bool {
        self.waits() || self.watch.watch
    }

    /// Get the batch name to tag runs with, generating one for a fan-out that wasn't named
    #[must_use]
    pub fn batch_name(&self) -> Option<String> {
        // a given name is always used; a fan-out is generated a name so it can be found again
        match (&self.batch, self.one_per_input) {
            (Some(batch), _) => Some(batch.clone()),
            (None, true) => Some(Uuid::new_v4().to_string()),
            (None, false) => None,
        }
    }
}

/// A command to run a tool
///
/// Without `--logs`, `--watch`, `--wait-for`, or `-o` with a wait flag, the run prints one JSON
/// object (the one `thorium-agent tool run` prints), after waiting for the run to finish when
/// `--wait` or `--output` is given.
#[derive(Parser, Debug, Clone)]
pub struct RunTool {
    /// The tool to run: an instance by ID or `<tool>@<version>`, or a tool's name for its latest
    /// version
    pub tool_instance: String,
    /// The run's inputs, reaction settings, and how to follow it
    #[clap(flatten)]
    pub common: RunCommonArgs,
    /// Make the run a sub reaction of this reaction
    #[clap(long, value_name = "REACTION")]
    pub parent: Option<Uuid>,
    /// A tool call input as a JSON object, mapped onto an agentic container tool's job
    #[clap(long)]
    pub input: Option<String>,
    /// A positional arg to pass to the tool's job
    #[clap(long = "arg")]
    pub args: Vec<String>,
    /// A keyword arg to pass to the tool's job, as KEY=VALUE (repeatable)
    #[clap(long = "kwarg", value_name = "KEY=VALUE")]
    pub kwargs: Vec<String>,
    /// A switch to pass to the tool's job (repeatable)
    #[clap(long = "switch")]
    pub switches: Vec<String>,
}

/// A command to run a workflow instance
#[derive(Parser, Debug, Clone)]
pub struct RunWorkflow {
    /// What to run: a workflow (latest instance), `<workflow>#r<revision>`, or an instance ID
    pub instance: String,
    /// The run's inputs, reaction settings, and how to follow it
    #[clap(flatten)]
    pub common: RunCommonArgs,
}

/// A command to run a pipeline
#[derive(Parser, Debug, Clone)]
pub struct RunPipeline {
    /// The pipeline to run, by name (or `<group>/<name>`)
    pub pipeline: String,
    /// The run's inputs, reaction settings, and how to follow it
    #[clap(flatten)]
    pub common: RunCommonArgs,
    /// Make the run a sub reaction of this reaction
    #[clap(long, value_name = "REACTION")]
    pub parent: Option<Uuid>,
}

/// The kinds of run `runs get` can list
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunKind {
    /// Runs of a single tool instance
    Tool,
    /// Runs of a workflow instance
    Workflow,
    /// Reactions of a pipeline
    Pipeline,
}

/// A command to list runs
#[derive(Parser, Debug)]
pub struct GetRuns {
    /// Only list the runs of this workflow, by ID or name
    #[clap(short, long)]
    pub workflow: Option<String>,
    /// Only list this kind of run (workflow runs by default)
    #[clap(long, value_enum, ignore_case = true)]
    pub kind: Option<RunKind>,
    /// The groups to list runs from (all of your groups by default)
    #[clap(short, long)]
    pub group: Vec<String>,
    /// The most runs to list per group
    #[clap(short, long, default_value_t = 50)]
    pub limit: usize,
    /// Only list the runs tagged with this batch name (see `run --batch`)
    #[clap(long, value_name = "NAME", conflicts_with = "workflow")]
    pub batch: Option<String>,
    /// Only list the runs with this status
    #[clap(long, value_enum, ignore_case = true)]
    pub status: Option<ReactionStatus>,
    /// The format to print the runs in (a table by default)
    #[clap(flatten)]
    pub format: ShortFormatArgs,
    /// Re-read and re-print the runs until interrupted
    #[clap(flatten)]
    pub watch: WatchArgs,
    /// Wait until every listed run matches a condition
    #[clap(flatten)]
    pub wait: WaitForArgs,
}

/// A command to describe a run
#[derive(Parser, Debug)]
pub struct DescribeRun {
    /// The id of the run's reaction
    pub reaction: Uuid,
    /// The group the run is in (searched for in all of your groups by default)
    #[clap(short, long)]
    pub group: Option<String>,
    /// The format to describe the run in (human readable by default)
    #[clap(flatten)]
    pub format: ShortFormatArgs,
    /// Re-read and re-describe the run until interrupted
    #[clap(flatten)]
    pub watch: WatchArgs,
    /// Wait until the run matches a condition
    #[clap(flatten)]
    pub wait: WaitForArgs,
}

/// A command to retry a failed workflow run
#[derive(Parser, Debug)]
pub struct RetryRun {
    /// The id of the failed run's reaction
    pub reaction: Uuid,
    /// The group the failed run is in (searched for in all of your groups by default)
    #[clap(short, long)]
    pub group: Option<String>,
}

/// A command to print a run's logs
#[derive(Parser, Debug)]
pub struct LogsRun {
    /// The id of the run's reaction
    pub reaction: Uuid,
    /// The group the run is in (searched for in all of your groups by default)
    #[clap(short, long)]
    pub group: Option<String>,
    /// Only print this stage's logs
    #[clap(long)]
    pub stage: Option<String>,
    /// The format to print the logs in (human readable by default)
    #[clap(flatten)]
    pub format: ShortFormatArgs,
    /// Re-read and re-print the logs until interrupted
    #[clap(flatten)]
    pub watch: WatchArgs,
}

/// A command to cancel a run, or every unfinished run matching some filters, by failing it
#[derive(Parser, Debug)]
#[clap(group(
    clap::ArgGroup::new("target")
        .required(true)
        .multiple(true)
        .args(["reaction", "batch", "workflow"])
))]
pub struct CancelRun {
    /// The id of the run's reaction
    #[clap(conflicts_with_all = ["batch", "workflow"])]
    pub reaction: Option<Uuid>,
    /// Cancel every unfinished run tagged with this batch name (see `run --batch`)
    #[clap(long, value_name = "NAME")]
    pub batch: Option<String>,
    /// Cancel every unfinished run of this workflow, by ID or name
    #[clap(short, long)]
    pub workflow: Option<String>,
    /// Only cancel runs with this status (Created or Started) when cancelling by filter
    #[clap(long, value_enum, ignore_case = true, conflicts_with = "reaction")]
    pub status: Option<ReactionStatus>,
    /// The group the run is in (searched for in all of your groups by default)
    #[clap(short, long)]
    pub group: Option<String>,
    /// Fail the run or runs without asking first
    #[clap(long, visible_alias = "yes")]
    pub skip_confirm: bool,
    /// Show what would be failed without failing it
    #[clap(long)]
    pub dry_run: bool,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use std::time::Duration;

    use super::{CancelRun, GetRuns, LogsRun, RunKind, RunPipeline, RunTool, RunWorkflow, Runs};
    use crate::args::{Args, SubCommands, tools::Tools, workflows::Workflows};
    use crate::utils::output::OutputFormat;

    /// Parse a `workflows run` command line, returning the clap error text when it is refused
    ///
    /// # Arguments
    ///
    /// * `flags` - The arguments after `workflows run triage`
    fn workflow_run(flags: &[&str]) -> Result<RunWorkflow, String> {
        let mut argv = vec!["thorctl", "workflows", "run", "triage"];
        argv.extend_from_slice(flags);
        match Args::try_parse_from(argv) {
            Ok(Args {
                cmd: SubCommands::Workflows(Workflows::Run(cmd)),
                ..
            }) => Ok(cmd),
            Ok(_) => Err("not a workflows run".to_owned()),
            Err(err) => Err(err.to_string()),
        }
    }

    /// Every common run flag parses with its value
    #[test]
    fn common_run_flags_parse() {
        let cmd = workflow_run(&[
            "-g",
            "acme",
            "--sample",
            "aa",
            "--file",
            "a.bin",
            "--repo",
            "https://example.com/r",
            "--sample-list",
            "samples.txt",
            "--repo-list",
            "repos.txt",
            "--tag",
            "t1",
            "--tag",
            "t2",
            "--sla",
            "30",
            "--wait",
            "--timeout",
            "90s",
            "--interval",
            "500ms",
            "--output",
            "out",
            "--logs",
            "-o",
            "json",
        ])
        .unwrap();
        let common = cmd.common;
        assert_eq!(common.group.as_deref(), Some("acme"));
        assert_eq!(common.inputs.samples, ["aa"]);
        assert_eq!(
            common.inputs.sample_list.as_deref(),
            Some(std::path::Path::new("samples.txt"))
        );
        assert_eq!(
            common.inputs.repo_list.as_deref(),
            Some(std::path::Path::new("repos.txt"))
        );
        assert_eq!(common.tag, ["t1", "t2"]);
        assert_eq!(common.sla, Some(30));
        assert!(common.wait && common.logs && common.waits());
        assert_eq!(common.wait_for.timeout, Duration::from_secs(90));
        assert_eq!(common.watch.interval, Duration::from_millis(500));
        assert_eq!(common.output.as_deref(), Some(std::path::Path::new("out")));
        assert_eq!(common.format.format, Some(OutputFormat::Json));
    }

    /// Without flags a run only starts, and each follow flag turns following on
    #[test]
    fn run_follows_only_when_asked() {
        // defaults leave the run unfollowed
        let common = workflow_run(&[]).unwrap().common;
        assert!(!common.follows() && !common.waits());
        assert_eq!(common.wait_for.timeout, Duration::from_secs(600));
        assert_eq!(common.watch.interval, Duration::from_secs(2));
        // --output and --logs imply --wait
        assert!(workflow_run(&["--output", "d"]).unwrap().common.waits());
        assert!(workflow_run(&["--logs"]).unwrap().common.waits());
        // the other follow flags follow without waiting for the end
        assert!(workflow_run(&["--watch"]).unwrap().common.follows());
        assert!(
            workflow_run(&["--wait-for", "status=Completed"])
                .unwrap()
                .common
                .follows()
        );
    }

    /// Flags that cannot be combined are refused
    #[test]
    fn conflicting_run_flags_are_refused() {
        for flags in [
            &["--wait", "--wait-for", "status=Completed"][..],
            &["--wait", "--watch"],
            &["--wait-for", "status=Completed", "--watch"],
            &["--output", "d", "--watch"],
            &["--logs", "--wait-for", "status=Completed"],
            &["--output", "d", "--wait-for", "status=Completed"],
            &["--parent", "6a3e2f9c-1b7d-4a58-9c0e-3d5f7a1b2c4d"],
        ] {
            assert!(workflow_run(flags).is_err(), "{flags:?} should be refused");
        }
    }

    /// `pipelines run` and `tools run` style flags: a pipeline takes a parent reaction
    #[test]
    fn pipeline_run_parses_a_parent() {
        let cmd = RunPipeline::try_parse_from([
            "run",
            "static/harvest",
            "--sample",
            "aa",
            "--parent",
            "6a3e2f9c-1b7d-4a58-9c0e-3d5f7a1b2c4d",
            "--wait",
        ])
        .unwrap();
        assert_eq!(cmd.pipeline, "static/harvest");
        assert_eq!(cmd.common.inputs.samples, ["aa"]);
        assert_eq!(
            cmd.parent.unwrap().to_string(),
            "6a3e2f9c-1b7d-4a58-9c0e-3d5f7a1b2c4d"
        );
        assert!(cmd.common.wait);
        // a malformed parent is refused
        assert!(RunPipeline::try_parse_from(["run", "p", "--parent", "nope"]).is_err());
    }

    /// `runs get` takes a kind, `runs logs` a stage, and `runs cancel` the confirmation flags
    #[test]
    fn runs_subcommands_parse() {
        let id = "6a3e2f9c-1b7d-4a58-9c0e-3d5f7a1b2c4d";
        let parse = |argv: &[&str]| {
            let mut full = vec!["thorctl", "runs"];
            full.extend_from_slice(argv);
            match Args::try_parse_from(full) {
                Ok(Args {
                    cmd: SubCommands::Runs(cmd),
                    ..
                }) => cmd,
                other => panic!("expected runs, got {other:?}"),
            }
        };
        // a kind filter
        let Runs::Get(GetRuns { kind, .. }) = parse(&["get", "--kind", "tool"]) else {
            panic!("expected runs get");
        };
        assert_eq!(kind, Some(RunKind::Tool));
        // a stage's logs
        let Runs::Logs(LogsRun { stage, watch, .. }) =
            parse(&["logs", id, "--stage", "scan", "--watch"])
        else {
            panic!("expected runs logs");
        };
        assert_eq!(stage.as_deref(), Some("scan"));
        assert!(watch.watch);
        // cancel with its flags, and --yes as an alias of --skip-confirm
        let Runs::Cancel(CancelRun {
            skip_confirm,
            dry_run,
            ..
        }) = parse(&["cancel", id, "--yes", "--dry-run"])
        else {
            panic!("expected runs cancel");
        };
        assert!(skip_confirm && dry_run);
        // a run id must be a uuid
        assert!(Args::try_parse_from(["thorctl", "runs", "cancel", "nope"]).is_err());
    }

    /// `tools run` parses into the shared run args with every flag it had before
    #[test]
    fn tools_run_parses_into_run_tool() {
        // parse a tool run that uses every flag
        let args = Args::try_parse_from([
            "thorctl",
            "tools",
            "run",
            "scan@1.0.0",
            "-g",
            "acme",
            "--sample",
            "abc",
            "--file",
            "a.bin",
            "--repo",
            "https://example.com/r:main",
            "--input",
            "{}",
            "--arg=-v",
            "--wait",
            "--output",
            "out",
        ])
        .expect("tools run parses");
        // the command must come back as the struct defined beside the other run args
        let SubCommands::Tools(Tools::Run(cmd)) = &args.cmd else {
            panic!("expected tools run");
        };
        let cmd: &RunTool = cmd;
        assert_eq!(cmd.tool_instance, "scan@1.0.0");
        assert_eq!(cmd.common.group.as_deref(), Some("acme"));
        assert_eq!(cmd.common.inputs.samples, ["abc"]);
        assert_eq!(cmd.common.inputs.repos, ["https://example.com/r:main"]);
        assert_eq!(cmd.args, ["-v"]);
        assert!(cmd.common.wait);
        assert_eq!(
            cmd.common.output.as_deref(),
            Some(std::path::Path::new("out"))
        );
    }

    /// `workflows run` parses into the shared run args with every flag it had before
    #[test]
    fn workflows_run_parses_into_run_workflow() {
        // parse a workflow run that uses the tag and sla flags
        let args = Args::try_parse_from([
            "thorctl",
            "workflows",
            "run",
            "triage#r2",
            "--sample",
            "abc",
            "--tag",
            "t1",
            "--sla",
            "60",
        ])
        .expect("workflows run parses");
        // the command must come back as the struct defined beside the other run args
        let SubCommands::Workflows(Workflows::Run(cmd)) = &args.cmd else {
            panic!("expected workflows run");
        };
        let cmd: &RunWorkflow = cmd;
        assert_eq!(cmd.instance, "triage#r2");
        assert_eq!(cmd.common.inputs.samples, ["abc"]);
        assert_eq!(cmd.common.tag, ["t1"]);
        assert_eq!(cmd.common.sla, Some(60));
    }
}
