//! Arguments for the run Thorctl commands

use clap::Parser;
use std::path::PathBuf;
use uuid::Uuid;

use super::helpers::{ShortFormatArgs, WaitForArgs, WatchArgs};

/// The commands to send to the runs task handler
#[derive(Parser, Debug)]
pub enum Runs {
    /// List workflow runs in your groups
    #[clap(version, author)]
    Get(GetRuns),
    /// Describe a tool or workflow run: its source, plan, and result key
    #[clap(version, author)]
    Describe(DescribeRun),
    /// Retry a failed workflow run as a new run on the same inputs
    #[clap(version, author)]
    Retry(RetryRun),
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
    /// A repo to run on, optionally with a commitish after a colon (<REPO-URL>[:<COMMITISH>])
    #[clap(long = "repo")]
    pub repos: Vec<String>,
}

/// A command to list workflow runs
#[derive(Parser, Debug)]
pub struct GetRuns {
    /// Only list the runs of this workflow, by ID or name
    #[clap(short, long)]
    pub workflow: Option<String>,
    /// The groups to list runs from (all of your groups by default)
    #[clap(short, long)]
    pub group: Vec<String>,
    /// The most runs to list per group
    #[clap(short, long, default_value_t = 50)]
    pub limit: usize,
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
