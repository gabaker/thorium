//! Batch runs: one run per input, started a few at a time, followed together by their batch tag
//!
//! `run --one-per-input` (on `run <kind>`, `tools run`, `workflows run`, and `pipelines run`)
//! hands the inputs and a function that starts one run to [`run_batch`]. A batch never stops
//! because one run failed to start, and it exits like a single followed run: 0 when every run
//! completed, 1 when any run failed or did not start, and 3 when some had not finished before the
//! timeout.

use futures::StreamExt;
use futures::stream;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;
use thorium::models::{Reaction, ReactionStatus, RunInputs};
use thorium::{Error, Thorium};
use tokio::time::{Instant, sleep};
use uuid::Uuid;

use crate::args::Args;
use crate::args::runs::RunCommonArgs;
use crate::handlers::runs::{PlannedInputs, find_run};
use crate::utils;
use crate::utils::output::OutputFormat;
use crate::utils::run_wait::download_results;
use crate::utils::watch;

/// The most times one run is tried before its start is given up on
pub(crate) const MAX_ATTEMPTS: u32 = 3;

/// The most times in a row reading a batch's runs may fail before following it is given up on
const MAX_READ_FAILURES: u32 = 3;

/// The most the poll interval grows to, as a multiple of the interval that was asked for
const MAX_BACKOFF_FACTOR: u32 = 4;

/// The status shown for an input whose run never started
const NOT_STARTED: &str = "NotStarted";

/// How a batch of runs is started and printed
#[derive(Debug, Clone)]
pub struct BatchOpts {
    /// The most runs to start at once
    pub workers: usize,
    /// Whether progress lines are left out
    pub quiet: bool,
    /// The wait before the first retry of a run that failed to start; each retry waits twice as
    /// long
    pub retry_base: Duration,
}

impl Default for BatchOpts {
    /// Build the options a command line without flags gets
    fn default() -> Self {
        BatchOpts {
            workers: 10,
            quiet: false,
            retry_base: Duration::from_millis(500),
        }
    }
}

impl BatchOpts {
    /// Build batch options from Thorctl's global flags
    ///
    /// # Arguments
    ///
    /// * `args` - The arguments passed to Thorctl
    #[must_use]
    pub fn from_args(args: &Args) -> Self {
        // take the number of parallel actions and the quiet flag from the global flags
        BatchOpts {
            workers: args.workers.max(1),
            quiet: args.quiet,
            ..BatchOpts::default()
        }
    }
}

/// One input of a batch and the inputs of the run that covers it
#[derive(Debug, Clone)]
pub struct BatchItem {
    /// The input as shown to the user: a SHA256, or a repo url with its commitish
    pub label: String,
    /// The inputs of the run for this item
    pub inputs: RunInputs,
}

/// What became of one input of a batch
#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    /// The input as shown to the user
    pub input: String,
    /// The id of the input's run, unless it never started
    pub id: Option<Uuid>,
    /// The group the run is in, once known
    pub group: String,
    /// The run's last known status, or `NotStarted`
    pub status: String,
    /// Why the run did not start or could not be followed
    pub error: Option<String>,
    /// The run as last read, kept to download its results
    #[serde(skip)]
    pub reaction: Option<Reaction>,
}

impl Outcome {
    /// Build the outcome of a run that was created and has not run yet
    ///
    /// # Arguments
    ///
    /// * `input` - The input as shown to the user
    /// * `id` - The id of the run
    /// * `group` - The group the run is in
    #[must_use]
    pub fn created(input: String, id: Uuid, group: String) -> Self {
        // a new run is Created and has no error
        Outcome {
            input,
            id: Some(id),
            group,
            status: ReactionStatus::Created.to_string(),
            error: None,
            reaction: None,
        }
    }

    /// Build the outcome of an input whose run never started
    ///
    /// # Arguments
    ///
    /// * `input` - The input as shown to the user
    /// * `error` - Why the run did not start
    #[must_use]
    pub fn not_started(input: String, error: String) -> Self {
        // an input that never started has no run, group, or reaction
        Outcome {
            input,
            id: None,
            group: String::new(),
            status: NOT_STARTED.to_owned(),
            error: Some(error),
            reaction: None,
        }
    }

    /// Whether the run's status is final
    #[must_use]
    pub fn finished(&self) -> bool {
        // a run ends by completing or failing
        self.id.is_some() && matches!(self.status.as_str(), "Completed" | "Failed")
    }

    /// Get the run's id as text, or nothing when the run never started
    #[must_use]
    pub fn id_text(&self) -> String {
        // an input that never started has no id
        self.id.map_or_else(String::new, |id| id.to_string())
    }

    /// Whether the input's run failed or never started
    #[must_use]
    pub fn failed(&self) -> bool {
        // an input without a run failed to start
        self.id.is_none() || self.status == "Failed"
    }
}

/// How a batch ended
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Every run started and, when followed, completed
    Success,
    /// This many inputs have a run that failed or never started
    Failed(usize),
    /// No run failed but this many had not finished when the timeout passed
    TimedOut(usize),
}

/// What a `--dry-run` found a command would start
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    /// The kind of run
    pub kind: &'static str,
    /// The tool, workflow, or pipeline that would run
    pub target: String,
    /// The group the runs would be in, when one was asked for
    pub group: Option<String>,
    /// The groups the target allows, when no group was asked for
    pub groups: Vec<String>,
    /// The batch name the runs would be tagged with
    pub batch: Option<String>,
    /// How many runs would start
    pub runs: usize,
    /// The inputs the runs would be given
    pub inputs: Vec<String>,
}

impl Plan {
    /// Build the plan of a command from its flags and inputs
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of run
    /// * `target` - The tool, workflow, or pipeline as the user named it
    /// * `common` - The run command's flags
    /// * `groups` - The groups the target allows
    /// * `planned` - The inputs the runs would be given
    #[must_use]
    pub fn new(
        kind: &'static str,
        target: &str,
        common: &RunCommonArgs,
        groups: &[String],
        planned: &PlannedInputs,
    ) -> Self {
        // list the inputs, dropping repeats when each input gets its own run
        let mut inputs = input_labels(&planned.inputs, &planned.files);
        if common.one_per_input {
            let mut seen = HashSet::new();
            inputs.retain(|input| seen.insert(input.clone()));
        }
        // a fan-out starts one run per input and anything else starts one run
        let runs = if common.one_per_input {
            inputs.len()
        } else {
            1
        };
        Plan {
            kind,
            target: target.to_owned(),
            group: common.group.clone(),
            groups: groups.to_vec(),
            batch: common.batch_name(),
            runs,
            inputs,
        }
    }
}

/// Split a run's inputs into one item per file or repo
///
/// Repeated inputs are dropped so each file or repo gets one run.
///
/// # Arguments
///
/// * `inputs` - The inputs of the whole run
#[must_use]
pub fn split_inputs(inputs: &RunInputs) -> Vec<BatchItem> {
    let mut seen = HashSet::new();
    let mut items = Vec::new();
    // one item for each file
    for sample in &inputs.samples {
        if seen.insert(sample.clone()) {
            items.push(BatchItem {
                label: sample.clone(),
                inputs: RunInputs::default().sample(sample),
            });
        }
    }
    // one item for each repo
    for repo in &inputs.repos {
        let label = match &repo.commitish {
            Some(commitish) => format!("{}:{commitish}", repo.url),
            None => repo.url.clone(),
        };
        if seen.insert(label.clone()) {
            items.push(BatchItem {
                label,
                inputs: RunInputs::default().repo(repo.clone()),
            });
        }
    }
    items
}

/// List the inputs of a run as they are shown to the user
///
/// # Arguments
///
/// * `inputs` - The files and repos the run is given
/// * `files` - The local files that would be uploaded and run on
#[must_use]
pub fn input_labels(inputs: &RunInputs, files: &[PathBuf]) -> Vec<String> {
    // list the files, repos, and local files in that order
    let mut labels = inputs.samples.clone();
    labels.extend(inputs.repos.iter().map(|repo| match &repo.commitish {
        Some(commitish) => format!("{}:{commitish}", repo.url),
        None => repo.url.clone(),
    }));
    labels.extend(files.iter().map(|file| file.display().to_string()));
    labels
}

/// Render what a command would start as text
///
/// # Arguments
///
/// * `plan` - What the command would start
#[must_use]
pub fn plan_text(plan: &Plan) -> String {
    // say where the runs would be
    let place = match (&plan.group, plan.groups.as_slice()) {
        (Some(group), _) => format!("in group {group}"),
        (None, []) => "in a group the API picks".to_owned(),
        (None, groups) => format!("in a group the API picks from {}", groups.join(", ")),
    };
    // describe how many runs of what would start
    let noun = if plan.runs == 1 { "run" } else { "runs" };
    let mut lines = vec![format!(
        "Dry run: would start {} {noun} of {} {} {place}",
        plan.runs, plan.kind, plan.target
    )];
    if let Some(batch) = &plan.batch {
        lines.push(format!("Batch: {batch}"));
    }
    // list the inputs
    lines.push(format!("Inputs ({}):", plan.inputs.len()));
    lines.extend(plan.inputs.iter().map(|input| format!("  {input}")));
    lines.push("Nothing was started.".to_owned());
    lines.join("\n")
}

/// Print what a command would start, as JSON with `-o json` and as text otherwise
///
/// # Arguments
///
/// * `plan` - What the command would start
/// * `format` - The format the command was asked to print in
pub fn print_plan(plan: &Plan, format: Option<OutputFormat>) -> Result<(), Error> {
    // JSON gets one document
    if format == Some(OutputFormat::Json) {
        println!("{}", serde_json::to_string(plan)?);
    } else {
        println!("{}", plan_text(plan));
    }
    Ok(())
}

/// Whether a failure to start a run is worth trying again
///
/// Failures to reach the API, too many requests, and the API's own failures are; a request the
/// API refused is not.
///
/// # Arguments
///
/// * `error` - The error from starting the run
#[must_use]
pub fn is_transient(error: &Error) -> bool {
    // a failure without a status never reached the API's answer
    match error.status() {
        None => true,
        Some(status) => matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504),
    }
}

/// Get how long to wait before the next read of a batch
///
/// The wait doubles, up to a multiple of the asked-for interval, while nothing changes and starts
/// over when a run changes status.
///
/// # Arguments
///
/// * `current` - The wait that was just used
/// * `base` - The interval that was asked for
/// * `changed` - Whether a run changed status in the last read
#[must_use]
pub fn next_interval(current: Duration, base: Duration, changed: bool) -> Duration {
    // a change starts the backoff over
    if changed {
        return base;
    }
    // otherwise double the wait, up to the cap
    (current * 2).min(base * MAX_BACKOFF_FACTOR)
}

/// Get how a batch ended
///
/// # Arguments
///
/// * `outcomes` - What became of each input
/// * `followed` - Whether the batch was followed until its runs finished
#[must_use]
pub fn verdict(outcomes: &[Outcome], followed: bool) -> Verdict {
    // a run that failed or never started is the first thing to report
    let failed = outcomes.iter().filter(|outcome| outcome.failed()).count();
    if failed > 0 {
        return Verdict::Failed(failed);
    }
    // a batch that was followed may have runs the timeout cut off
    let unfinished = outcomes
        .iter()
        .filter(|outcome| !outcome.finished())
        .count();
    if followed && unfinished > 0 {
        return Verdict::TimedOut(unfinished);
    }
    Verdict::Success
}

/// Render the end of a batch: the runs by status, and the runs that need attention
///
/// # Arguments
///
/// * `batch` - The batch name
/// * `outcomes` - What became of each input
/// * `followed` - Whether the batch was followed until its runs finished
#[must_use]
pub fn summary_lines(batch: &str, outcomes: &[Outcome], followed: bool) -> Vec<String> {
    // count the runs by status
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for outcome in outcomes {
        match counts
            .iter_mut()
            .find(|(status, _)| *status == outcome.status)
        {
            Some((_, count)) => *count += 1,
            None => counts.push((outcome.status.as_str(), 1)),
        }
    }
    counts.sort_by_key(|(status, _)| *status);
    let by_status = counts
        .iter()
        .map(|(status, count)| format!("{count} {status}"))
        .collect::<Vec<String>>()
        .join(", ");
    let noun = if outcomes.len() == 1 {
        "input"
    } else {
        "inputs"
    };
    let mut lines = vec![format!(
        "Batch {batch}: {} {noun} ({by_status})",
        outcomes.len()
    )];
    // list the runs that failed
    for outcome in outcomes
        .iter()
        .filter(|outcome| outcome.id.is_some() && outcome.failed())
    {
        lines.push(format!(
            "  Failed: {} ({})",
            outcome.id_text(),
            outcome.input
        ));
    }
    // list the inputs that never started and why
    for outcome in outcomes.iter().filter(|outcome| outcome.id.is_none()) {
        lines.push(format!(
            "  Not started: {}: {}",
            outcome.input,
            outcome.error.as_deref().unwrap_or("unknown error")
        ));
    }
    // list the runs a timeout cut off
    if followed {
        for outcome in outcomes
            .iter()
            .filter(|outcome| outcome.id.is_some() && !outcome.finished())
        {
            lines.push(format!(
                "  Unfinished: {} ({}) is {}",
                outcome.id_text(),
                outcome.input,
                outcome.status
            ));
        }
    }
    lines
}

/// Render one input's outcome as a JSON line
///
/// # Arguments
///
/// * `outcome` - What became of the input
pub fn json_line(outcome: &Outcome) -> Result<String, Error> {
    // the input, its run, its group, its status, and why it failed if it did
    Ok(serde_json::to_string(outcome)?)
}

/// Render a run's status as one line of progress
///
/// # Arguments
///
/// * `status` - The run's status
/// * `id` - The run's id, or `-` when it never started
/// * `input` - The input the run is on
pub(crate) fn status_line(status: &str, id: &str, input: &str) -> String {
    format!("{status:<12} | {id:<36} | {input}")
}

/// Whether a batch prints progress lines to stdout
///
/// # Arguments
///
/// * `common` - The run command's flags
/// * `opts` - How the batch is printed
pub(crate) fn prints_progress(common: &RunCommonArgs, opts: &BatchOpts) -> bool {
    // JSON keeps stdout to its lines, and quiet drops progress
    !opts.quiet && common.format.format != Some(OutputFormat::Json)
}

/// Start one input's run, trying again when the failure is likely to pass
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `common` - The run command's flags
/// * `opts` - How the batch is started
/// * `batch` - The batch name to tag the run with
/// * `start` - Starts one run on some inputs with some extra tags and returns its id
/// * `item` - The input to run on
pub(crate) async fn start_one<S, Fut>(
    thorium: &Thorium,
    common: &RunCommonArgs,
    opts: &BatchOpts,
    batch: &str,
    start: &S,
    item: BatchItem,
) -> Outcome
where
    S: Fn(RunInputs, Vec<String>) -> Fut,
    Fut: Future<Output = Result<Uuid, Error>>,
{
    let mut attempt = 0;
    // try to start the run, waiting longer before each retry
    let created = loop {
        match start(item.inputs.clone(), vec![batch.to_owned()]).await {
            Ok(id) => break Ok(id),
            Err(error) if attempt + 1 < MAX_ATTEMPTS && is_transient(&error) => {
                sleep(opts.retry_base * 2_u32.pow(attempt)).await;
                attempt += 1;
            }
            Err(error) => break Err(error),
        }
    };
    // an input that never started has no run to follow
    let id = match created {
        Ok(id) => id,
        Err(error) => {
            return Outcome {
                input: item.label,
                id: None,
                group: String::new(),
                status: NOT_STARTED.to_owned(),
                error: Some(error.to_string()),
                reaction: None,
            };
        }
    };
    // find the group of a run that will be followed
    let mut error = None;
    let group = match &common.group {
        Some(group) => group.clone(),
        None if common.follows_batch() => match find_run(thorium, id, None).await {
            Ok(run) => run.group,
            Err(find) => {
                error = Some(format!("unable to find the run's group: {find}"));
                String::new()
            }
        },
        None => String::new(),
    };
    Outcome {
        input: item.label,
        id: Some(id),
        group,
        status: ReactionStatus::Created.to_string(),
        error,
        reaction: None,
    }
}

/// Read the runs of a batch once and note the status of each of ours
///
/// Returns whether any run changed status. A run that the tag listing did not show is read on its
/// own so one slow listing can't hide it.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `batch` - The batch name
/// * `outcomes` - What became of each input; updated as runs change
/// * `print` - Whether to print a line for each change
async fn poll_batch(
    thorium: &Thorium,
    batch: &str,
    outcomes: &mut [Outcome],
    print: bool,
) -> Result<bool, Error> {
    // index our started runs by id
    let by_id: HashMap<Uuid, usize> = outcomes
        .iter()
        .enumerate()
        .filter_map(|(index, outcome)| outcome.id.map(|id| (id, index)))
        .collect();
    let groups: HashSet<String> = outcomes
        .iter()
        .filter(|outcome| outcome.id.is_some() && !outcome.group.is_empty())
        .map(|outcome| outcome.group.clone())
        .collect();
    let mut seen = HashSet::new();
    let mut changed = false;
    // read the batch's runs in each group the runs are in
    for group in &groups {
        let mut cursor = thorium
            .reactions
            .list_tag(group, batch)
            .page_size(100)
            .details();
        while !cursor.exhausted {
            cursor.next().await?;
            for reaction in cursor.details.drain(..) {
                // only the runs this command started count
                let Some(&index) = by_id.get(&reaction.id) else {
                    continue;
                };
                seen.insert(reaction.id);
                changed |= note_status(&mut outcomes[index], reaction, print);
            }
        }
    }
    // read a run the listing did not show on its own
    for (id, index) in &by_id {
        if seen.contains(id) || outcomes[*index].finished() || outcomes[*index].group.is_empty() {
            continue;
        }
        let group = outcomes[*index].group.clone();
        match thorium.reactions.get(&group, *id).await {
            // note the status of a run that still exists
            Ok(reaction) => changed |= note_status(&mut outcomes[*index], reaction, print),
            // a run that is gone is final, so its input fails and the rest are still followed
            Err(error) if error.status().is_some_and(|status| status.as_u16() == 404) => {
                changed |= note_missing(&mut outcomes[*index], &error, print);
            }
            // any other failure fails the whole read, which is tried again
            Err(error) => return Err(error),
        }
    }
    Ok(changed)
}

/// Record that a run no longer exists and print a line for the change
///
/// Returns whether the status changed. A run the API no longer knows is final, so its input
/// counts as failed in the summary and the exit code.
///
/// # Arguments
///
/// * `outcome` - The input's outcome
/// * `error` - The error reading the run returned
/// * `print` - Whether to print a line for a change
fn note_missing(outcome: &mut Outcome, error: &Error, print: bool) -> bool {
    // a run already known to have failed is not a change
    let status = ReactionStatus::Failed.to_string();
    let changed = outcome.status != status;
    // remember the status and why the input failed
    outcome.status.clone_from(&status);
    outcome.error = Some(format!("the run no longer exists: {error}"));
    // say what changed
    if changed && print {
        let id = outcome.id_text();
        println!("{}", status_line(&status, &id, &outcome.input));
    }
    changed
}

/// Record a run's status and print a line when it changed
///
/// Returns whether the status changed.
///
/// # Arguments
///
/// * `outcome` - The input's outcome
/// * `reaction` - The run as just read
/// * `print` - Whether to print a line for a change
fn note_status(outcome: &mut Outcome, reaction: Reaction, print: bool) -> bool {
    // a status that is already known is not a change
    let status = reaction.status.to_string();
    let changed = outcome.status != status;
    // remember the run and its status
    outcome.status.clone_from(&status);
    outcome.reaction = Some(reaction);
    // say what changed
    if changed && print {
        let id = outcome.id_text();
        println!("{}", status_line(&status, &id, &outcome.input));
    }
    changed
}

/// Follow a batch until every run finishes or the timeout passes
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `common` - The run command's flags
/// * `opts` - How the batch is printed
/// * `batch` - The batch name
/// * `outcomes` - What became of each input; updated as runs change
pub(crate) async fn follow_batch(
    thorium: &Thorium,
    common: &RunCommonArgs,
    opts: &BatchOpts,
    batch: &str,
    outcomes: &mut [Outcome],
) -> Result<(), Error> {
    // `--wait` gives up at the timeout and `--watch` never does
    let deadline = common
        .waits()
        .then(|| Instant::now() + common.wait_for.timeout);
    let print = prints_progress(common, opts);
    let base = common.watch.interval;
    let mut interval = base;
    let mut failures = 0;
    // read the batch until its runs are final or the time is up
    loop {
        // a failed read is tried again a few times before giving up
        match poll_batch(thorium, batch, outcomes, print).await {
            Ok(changed) => {
                failures = 0;
                interval = next_interval(interval, base, changed);
            }
            Err(error) => {
                failures += 1;
                if failures >= MAX_READ_FAILURES {
                    return Err(error);
                }
                eprintln!("Unable to read batch {batch}: {error}");
            }
        }
        // stop when every started run is final
        if outcomes
            .iter()
            .all(|outcome| outcome.id.is_none() || outcome.finished())
        {
            return Ok(());
        }
        // stop at the deadline, and never sleep past it
        let wait = match deadline {
            Some(deadline) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Ok(());
                }
                interval.min(left)
            }
            None => interval,
        };
        sleep(wait).await;
    }
}

/// Start and follow a batch, printing progress as runs start and change
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `common` - The run command's flags
/// * `opts` - How the batch is started and printed
/// * `batch` - The batch name every run is tagged with
/// * `items` - The inputs to start a run for
/// * `start` - Starts one run on some inputs with some extra tags and returns its id
pub async fn execute<S, Fut>(
    thorium: &Thorium,
    common: &RunCommonArgs,
    opts: &BatchOpts,
    batch: &str,
    items: Vec<BatchItem>,
    start: S,
) -> Result<Vec<Outcome>, Error>
where
    S: Fn(RunInputs, Vec<String>) -> Fut,
    Fut: Future<Output = Result<Uuid, Error>>,
{
    let print = prints_progress(common, opts);
    // name the batch so the user can find its runs again
    if print {
        println!("Batch: {batch}");
        println!("{}", status_line("STATUS", "RUN ID", "INPUT"));
    } else if !opts.quiet {
        eprintln!("Batch: {batch}");
    }
    // start the runs a few at a time, keeping the order of the inputs
    let start = &start;
    let mut started: Vec<(usize, Outcome)> = stream::iter(items.into_iter().enumerate())
        .map(|(index, item)| async move {
            let outcome = start_one(thorium, common, opts, batch, start, item).await;
            (index, outcome)
        })
        .buffer_unordered(opts.workers.max(1))
        .inspect(|(_, outcome)| report_start(outcome, print))
        .collect()
        .await;
    started.sort_by_key(|(index, _)| *index);
    let mut outcomes: Vec<Outcome> = started.into_iter().map(|(_, outcome)| outcome).collect();
    // follow the runs when asked to
    if common.follows_batch() {
        follow_batch(thorium, common, opts, batch, &mut outcomes).await?;
    }
    // download the results of the runs that completed
    if let Some(dir) = &common.output {
        for outcome in &outcomes {
            let Some(reaction) = &outcome.reaction else {
                continue;
            };
            if reaction.status != ReactionStatus::Completed {
                continue;
            }
            let target = dir.join(utils::fs::path_component(&outcome.input));
            let written = download_results(thorium, reaction, &target).await?;
            if !opts.quiet {
                eprintln!("Saved {} files to {}", written.len(), target.display());
            }
        }
    }
    Ok(outcomes)
}

/// Print a line for a run that just started or failed to start (the summary says why)
///
/// # Arguments
///
/// * `outcome` - What became of the input
/// * `print` - Whether progress is printed to stdout
fn report_start(outcome: &Outcome, print: bool) {
    // a run that started is a line of progress
    if print {
        let id = outcome
            .id
            .map_or_else(|| "-".to_owned(), |id| id.to_string());
        println!("{}", status_line(&outcome.status, &id, &outcome.input));
    }
}

/// Print how a batch ended and return the error that sets the exit code, if any
///
/// # Arguments
///
/// * `common` - The run command's flags
/// * `batch` - The batch name
/// * `outcomes` - What became of each input
pub fn finish(common: &RunCommonArgs, batch: &str, outcomes: &[Outcome]) -> Result<(), Error> {
    let followed = common.follows_batch();
    let json = common.format.format == Some(OutputFormat::Json);
    // JSON prints one line per input and sends the summary to stderr
    if json {
        for outcome in outcomes {
            println!("{}", json_line(outcome)?);
        }
    }
    for line in summary_lines(batch, outcomes, followed) {
        if json {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    }
    // name how the batch ended in the exit code
    match verdict(outcomes, followed) {
        Verdict::Success => Ok(()),
        Verdict::Failed(count) => Err(Error::new(format!(
            "{count} of {} runs in batch {batch} failed or did not start",
            outcomes.len()
        ))),
        Verdict::TimedOut(count) => {
            // exit with the timeout code
            watch::mark_timed_out();
            Err(Error::new(format!(
                "Timed out after {:?} with {count} of {} runs in batch {batch} unfinished",
                common.wait_for.timeout,
                outcomes.len()
            )))
        }
    }
}

/// Start one run per input and follow the batch as the command's flags ask
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `common` - The run command's flags
/// * `opts` - How the batch is started and printed
/// * `items` - The inputs to start a run for
/// * `start` - Starts one run on some inputs with some extra tags and returns its id
pub async fn run_batch<S, Fut>(
    thorium: &Thorium,
    common: &RunCommonArgs,
    opts: &BatchOpts,
    items: Vec<BatchItem>,
    start: S,
) -> Result<(), Error>
where
    S: Fn(RunInputs, Vec<String>) -> Fut,
    Fut: Future<Output = Result<Uuid, Error>>,
{
    // a batch needs at least one input
    if items.is_empty() {
        return Err(Error::new(
            "--one-per-input needs at least one input (--sample, --file, --repo, --sample-list, or --repo-list)",
        ));
    }
    // use the given batch name or make one
    let batch = common
        .batch_name()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    // start and follow the runs, then report how it went
    let outcomes = execute(thorium, common, opts, &batch, items, start).await?;
    finish(common, &batch, &outcomes)
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use thorium::models::{ReactionRequest, ReactionStatus, RepoDependencyRequest, RunInputs};
    use uuid::Uuid;

    use super::{
        BatchItem, BatchOpts, Outcome, Plan, Verdict, execute, input_labels, is_transient,
        json_line, next_interval, plan_text, split_inputs, summary_lines, verdict,
    };
    use crate::args::runs::{RunCommonArgs, RunWorkflow};
    use crate::handlers::runs::PlannedInputs;
    use crate::utils::run_wait::testing::{client, mock, reaction};

    /// Parse the shared flags of a `workflows run` command in the corn group
    ///
    /// # Arguments
    ///
    /// * `flags` - The flags after the workflow name
    fn common(flags: &[&str]) -> RunCommonArgs {
        let mut argv = vec![
            "run",
            "wf",
            "-g",
            "corn",
            "--one-per-input",
            "--batch",
            "nightly",
        ];
        argv.extend_from_slice(flags);
        RunWorkflow::try_parse_from(argv).unwrap().common
    }

    /// Batch options with quick retries and a worker bound
    ///
    /// # Arguments
    ///
    /// * `workers` - The most runs to start at once
    fn opts(workers: usize) -> BatchOpts {
        BatchOpts {
            workers,
            quiet: true,
            retry_base: Duration::from_millis(1),
        }
    }

    /// Build the items of a batch from file names
    ///
    /// # Arguments
    ///
    /// * `names` - The SHA256s to run on
    fn items(names: &[&str]) -> Vec<BatchItem> {
        let mut inputs = RunInputs::default();
        for name in names {
            inputs = inputs.sample(*name);
        }
        split_inputs(&inputs)
    }

    /// Build an outcome
    ///
    /// # Arguments
    ///
    /// * `input` - The input
    /// * `id` - The run's id, if it started
    /// * `status` - The run's status
    fn outcome(input: &str, id: Option<Uuid>, status: &str) -> Outcome {
        Outcome {
            input: input.to_owned(),
            id,
            group: "corn".to_owned(),
            status: status.to_owned(),
            error: id.is_none().then(|| "boom".to_owned()),
            reaction: None,
        }
    }

    /// Turn a run's statuses into the body of a tag listing
    ///
    /// # Arguments
    ///
    /// * `runs` - Each run's id and status
    fn listing(runs: &[(Uuid, ReactionStatus)]) -> String {
        // the details of each reaction, and no next page
        let details: Vec<Value> = runs
            .iter()
            .map(|(id, status)| serde_json::to_value(reaction(*id, status.clone())).unwrap())
            .collect();
        json!({"cursor": null, "details": details}).to_string()
    }

    /// Splitting makes one item per file or repo and drops repeats
    #[test]
    fn split_inputs_makes_one_item_per_input() {
        let inputs = RunInputs::default()
            .sample("aa")
            .sample("bb")
            .sample("aa")
            .repo(RepoDependencyRequest::new("https://example.com/r"))
            .repo(RepoDependencyRequest::new("https://example.com/r"));
        let items = split_inputs(&inputs);
        // files first, then repos, each once and alone
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels, ["aa", "bb", "https://example.com/r"]);
        assert!(
            items
                .iter()
                .all(|item| item.inputs.samples.len() + item.inputs.repos.len() == 1)
        );
        // nothing in, nothing out
        assert!(split_inputs(&RunInputs::default()).is_empty());
    }

    /// Labels list files, repos with their commitish, and local files
    #[test]
    fn input_labels_list_every_kind() {
        let mut repo = RepoDependencyRequest::new("https://example.com/r");
        repo.commitish = Some("main".to_owned());
        let inputs = RunInputs::default().sample("aa").repo(repo);
        let labels = input_labels(&inputs, &[std::path::PathBuf::from("a.bin")]);
        assert_eq!(labels, ["aa", "https://example.com/r:main", "a.bin"]);
    }

    /// Only failures to reach the API, too many requests, and server errors are retried
    #[tokio::test]
    async fn only_transient_errors_are_retried() {
        for (code, transient) in [
            (400, false),
            (403, false),
            (404, false),
            (429, true),
            (500, true),
            (503, true),
        ] {
            let (host, _) = mock(move |_, _| (code, r#"{"error":"no"}"#.to_owned())).await;
            let thorium = client(&host).await;
            let error = thorium
                .reactions
                .create(&ReactionRequest::new("corn", "harvest"))
                .await
                .unwrap_err();
            assert_eq!(is_transient(&error), transient, "{code}: {error}");
        }
        // an error with no status never got an answer
        assert!(is_transient(&thorium::Error::new("connection refused")));
    }

    /// The poll interval doubles up to a cap while nothing changes and resets on a change
    #[test]
    fn the_interval_backs_off_and_resets() {
        let base = Duration::from_secs(2);
        let mut current = base;
        let mut seen = Vec::new();
        for _ in 0..4 {
            current = next_interval(current, base, false);
            seen.push(current.as_secs());
        }
        // 2s doubles to 4s, 8s, then stays at four times the base
        assert_eq!(seen, [4, 8, 8, 8]);
        // a change starts over
        assert_eq!(next_interval(current, base, true), base);
    }

    /// A batch succeeds only when every run completed, fails on any failure, and times out last
    #[test]
    fn the_verdict_follows_the_exit_codes() {
        let id = || Some(Uuid::new_v4());
        let done = vec![
            outcome("a", id(), "Completed"),
            outcome("b", id(), "Completed"),
        ];
        assert_eq!(verdict(&done, true), Verdict::Success);
        // runs that are only started are fine when the batch was not followed
        let started = vec![outcome("a", id(), "Created"), outcome("b", id(), "Created")];
        assert_eq!(verdict(&started, false), Verdict::Success);
        // ...and unfinished when it was
        assert_eq!(verdict(&started, true), Verdict::TimedOut(2));
        // a failed run, a cancelled one (also Failed), and a start that failed all count
        let mixed = vec![
            outcome("a", id(), "Completed"),
            outcome("b", id(), "Failed"),
            outcome("c", None, "NotStarted"),
            outcome("d", id(), "Started"),
        ];
        assert_eq!(verdict(&mixed, true), Verdict::Failed(2));
        assert_eq!(verdict(&mixed, false), Verdict::Failed(2));
    }

    /// The summary counts runs by status and names the runs that need attention
    #[test]
    fn the_summary_names_failures() {
        let failed = Uuid::new_v4();
        let slow = Uuid::new_v4();
        let outcomes = vec![
            outcome("a", Some(Uuid::new_v4()), "Completed"),
            outcome("b", Some(failed), "Failed"),
            outcome("c", None, "NotStarted"),
            outcome("d", Some(slow), "Started"),
        ];
        let text = summary_lines("nightly", &outcomes, true).join("\n");
        assert!(
            text.contains(
                "Batch nightly: 4 inputs (1 Completed, 1 Failed, 1 NotStarted, 1 Started)"
            ),
            "{text}"
        );
        assert!(text.contains(&format!("Failed: {failed} (b)")), "{text}");
        assert!(text.contains("Not started: c: boom"), "{text}");
        assert!(
            text.contains(&format!("Unfinished: {slow} (d) is Started")),
            "{text}"
        );
        // a batch that was not followed has no unfinished runs to report
        let text = summary_lines("nightly", &outcomes, false).join("\n");
        assert!(!text.contains("Unfinished"), "{text}");
    }

    /// Each outcome is one JSON line with the input, run id, group, status, and error
    #[test]
    fn json_lines_carry_the_input_run_and_status() {
        let id = Uuid::new_v4();
        let line = json_line(&outcome("aa", Some(id), "Completed")).unwrap();
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["input"], "aa");
        assert_eq!(value["id"], id.to_string());
        assert_eq!(value["status"], "Completed");
        assert_eq!(value["group"], "corn");
        assert!(value["error"].is_null());
        // the run as read is not part of the line, and one outcome is one line
        assert!(value.get("reaction").is_none() && !line.contains('\n'));
        // a start that failed has no id and says why
        let value: Value =
            serde_json::from_str(&json_line(&outcome("bb", None, "NotStarted")).unwrap()).unwrap();
        assert!(value["id"].is_null());
        assert_eq!(value["status"], "NotStarted");
        assert_eq!(value["error"], "boom");
    }

    /// A dry run's plan says what would start, where, and in which batch
    #[test]
    fn the_plan_says_what_would_start() {
        let planned = PlannedInputs {
            inputs: RunInputs::default().sample("aa").sample("bb"),
            files: vec![std::path::PathBuf::from("c.bin")],
        };
        // a fan-out is one run per input
        let flags =
            RunWorkflow::try_parse_from(["run", "wf", "--one-per-input", "--batch", "nightly"])
                .unwrap()
                .common;
        let plan = Plan::new(
            "workflow",
            "triage",
            &flags,
            &["corn".to_owned(), "wheat".to_owned()],
            &planned,
        );
        assert_eq!(plan.runs, 3);
        let text = plan_text(&plan);
        assert!(
            text.contains(
                "would start 3 runs of workflow triage in a group the API picks from corn, wheat"
            ),
            "{text}"
        );
        assert!(
            text.contains("Batch: nightly")
                && text.contains("  c.bin")
                && text.contains("Nothing was started."),
            "{text}"
        );
        // without a fan-out all inputs go in one run, in the group that was asked for
        let flags = RunWorkflow::try_parse_from(["run", "wf", "-g", "corn"])
            .unwrap()
            .common;
        let plan = Plan::new("workflow", "triage", &flags, &[], &planned);
        assert_eq!(plan.runs, 1);
        assert!(plan.batch.is_none());
        assert!(plan_text(&plan).contains("would start 1 run of workflow triage in group corn"));
        // the plan serializes for -o json
        let value = serde_json::to_value(&plan).unwrap();
        assert_eq!(value["runs"], 1);
        assert_eq!(value["inputs"], json!(["aa", "bb", "c.bin"]));
    }

    /// A batch starts one run per input, every run gets the batch tag, and no more than the
    /// workers bound start at once
    #[tokio::test]
    async fn execute_starts_one_run_per_input_within_the_workers_bound() {
        let (host, requests) = mock(|_, _| (404, "{}".to_owned())).await;
        let thorium = client(&host).await;
        let in_flight = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let tags = Arc::new(Mutex::new(Vec::new()));
        let names = ["a", "b", "c", "d", "e", "f"];
        let start = |inputs: RunInputs, extra: Vec<String>| {
            let (in_flight, most, tags) = (in_flight.clone(), most.clone(), tags.clone());
            async move {
                // track how many starts overlap
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                most.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(40)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
                tags.lock().unwrap().push((inputs.samples.clone(), extra));
                Ok(Uuid::new_v4())
            }
        };
        let outcomes = execute(
            &thorium,
            &common(&[]),
            &opts(2),
            "nightly",
            items(&names),
            start,
        )
        .await
        .unwrap();
        // one outcome per input, in the order of the inputs, each with its own run
        let inputs: Vec<&str> = outcomes
            .iter()
            .map(|outcome| outcome.input.as_str())
            .collect();
        assert_eq!(inputs, names);
        assert!(
            outcomes
                .iter()
                .all(|outcome| outcome.id.is_some() && outcome.status == "Created")
        );
        let ids: std::collections::HashSet<_> =
            outcomes.iter().filter_map(|outcome| outcome.id).collect();
        assert_eq!(ids.len(), 6);
        // every run got one file and the batch tag
        let tags = tags.lock().unwrap();
        assert_eq!(tags.len(), 6);
        assert!(
            tags.iter()
                .all(|(samples, extra)| samples.len() == 1 && extra == &["nightly"])
        );
        // the bound was reached and never exceeded
        assert_eq!(most.load(Ordering::SeqCst), 2);
        // nothing was followed so nothing was read
        assert!(requests.lock().unwrap().is_empty());
        assert_eq!(verdict(&outcomes, false), Verdict::Success);
    }

    /// A failed start never stops the batch: a refusal is not retried, a flaky API is retried, and
    /// an API that stays down fails only its input
    #[tokio::test]
    async fn a_failed_start_does_not_stop_the_batch() {
        let flaky_calls = Arc::new(AtomicUsize::new(0));
        let counter = flaky_calls.clone();
        let (host, requests) = mock(move |_, body| {
            if body.contains("refused") {
                (400, r#"{"error":"no"}"#.to_owned())
            } else if body.contains("down") {
                (503, r#"{"error":"down"}"#.to_owned())
            } else if body.contains("flaky") && counter.fetch_add(1, Ordering::SeqCst) < 2 {
                (503, r#"{"error":"busy"}"#.to_owned())
            } else {
                (200, json!({"id": Uuid::new_v4()}).to_string())
            }
        })
        .await;
        let thorium = client(&host).await;
        let post = |inputs: RunInputs, extra: Vec<String>| {
            let thorium = &thorium;
            async move {
                let req = ReactionRequest::new("corn", "harvest")
                    .samples(inputs.samples)
                    .tags(extra);
                Ok(thorium.reactions.create(&req).await?.id)
            }
        };
        let outcomes = execute(
            &thorium,
            &common(&[]),
            &opts(3),
            "nightly",
            items(&["fine", "refused", "down", "flaky"]),
            post,
        )
        .await
        .unwrap();
        // the inputs that could start did, and the others say why they didn't
        let started: Vec<bool> = outcomes
            .iter()
            .map(|outcome| outcome.id.is_some())
            .collect();
        assert_eq!(started, [true, false, false, true]);
        assert!(
            outcomes[1].error.as_deref().unwrap().contains("400"),
            "{:?}",
            outcomes[1].error
        );
        assert_eq!(outcomes[1].status, "NotStarted");
        // a refusal is tried once, a failure of the API three times, and the flaky one until it works
        let posted = |needle: &str| {
            requests
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, body)| body.contains(needle))
                .count()
        };
        assert_eq!(posted("refused"), 1);
        assert_eq!(posted("down"), 3);
        assert_eq!(posted("flaky"), 3);
        // the batch exits 1 because two inputs have no run
        assert_eq!(verdict(&outcomes, false), Verdict::Failed(2));
    }

    /// Following a batch prints each change once and ends when every run completed
    #[tokio::test]
    async fn following_ends_when_every_run_completed() {
        let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
        let polls = Arc::new(AtomicUsize::new(0));
        let counter = polls.clone();
        let (host, requests) = mock(move |line, _| {
            if line.starts_with("GET /api/reactions/tag/corn/nightly/") {
                // the first run starts and finishes while the second waits, then it starts
                let poll = counter.fetch_add(1, Ordering::SeqCst);
                let runs = match poll {
                    0 => [
                        (first, ReactionStatus::Started),
                        (second, ReactionStatus::Created),
                    ],
                    1 => [
                        (first, ReactionStatus::Completed),
                        (second, ReactionStatus::Started),
                    ],
                    _ => [
                        (first, ReactionStatus::Completed),
                        (second, ReactionStatus::Completed),
                    ],
                };
                (200, listing(&runs))
            } else {
                (404, "{}".to_owned())
            }
        })
        .await;
        let thorium = client(&host).await;
        let ids = HashMap::from([("a", first), ("b", second)]);
        let start = |inputs: RunInputs, _extra: Vec<String>| {
            let id = ids[inputs.samples[0].as_str()];
            async move { Ok(id) }
        };
        let flags = common(&["--wait", "--interval", "10ms"]);
        let outcomes = execute(
            &thorium,
            &flags,
            &opts(2),
            "nightly",
            items(&["a", "b"]),
            start,
        )
        .await
        .unwrap();
        // both runs completed after three reads of the tag
        assert!(
            outcomes.iter().all(|outcome| outcome.status == "Completed"),
            "{outcomes:?}"
        );
        assert_eq!(polls.load(Ordering::SeqCst), 3);
        assert_eq!(verdict(&outcomes, true), Verdict::Success);
        // the batch was read by its tag and each run was never read on its own
        assert!(
            requests
                .lock()
                .unwrap()
                .iter()
                .all(|(line, _)| line.contains("/tag/corn/nightly/"))
        );
    }

    /// A run that fails ends the wait and fails the batch; a cancel is the same as a failure
    #[tokio::test]
    async fn following_reports_a_failed_run() {
        let (good, bad) = (Uuid::new_v4(), Uuid::new_v4());
        let (host, _) = mock(move |_, _| {
            (
                200,
                listing(&[
                    (good, ReactionStatus::Completed),
                    (bad, ReactionStatus::Failed),
                ]),
            )
        })
        .await;
        let thorium = client(&host).await;
        let ids = HashMap::from([("a", good), ("b", bad)]);
        let start = |inputs: RunInputs, _extra: Vec<String>| {
            let id = ids[inputs.samples[0].as_str()];
            async move { Ok(id) }
        };
        let flags = common(&["--wait", "--interval", "10ms"]);
        let outcomes = execute(
            &thorium,
            &flags,
            &opts(2),
            "nightly",
            items(&["a", "b"]),
            start,
        )
        .await
        .unwrap();
        assert_eq!(verdict(&outcomes, true), Verdict::Failed(1));
        let text = summary_lines("nightly", &outcomes, true).join("\n");
        assert!(text.contains(&format!("Failed: {bad} (b)")), "{text}");
    }

    /// A batch that never finishes stops at the timeout with its runs unfinished
    #[tokio::test]
    async fn following_stops_at_the_timeout() {
        let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
        let (host, _) = mock(move |_, _| {
            (
                200,
                listing(&[
                    (first, ReactionStatus::Completed),
                    (second, ReactionStatus::Started),
                ]),
            )
        })
        .await;
        let thorium = client(&host).await;
        let ids = HashMap::from([("a", first), ("b", second)]);
        let start = |inputs: RunInputs, _extra: Vec<String>| {
            let id = ids[inputs.samples[0].as_str()];
            async move { Ok(id) }
        };
        let flags = common(&["--wait", "--interval", "20ms", "--timeout", "200ms"]);
        let began = std::time::Instant::now();
        let outcomes = execute(
            &thorium,
            &flags,
            &opts(2),
            "nightly",
            items(&["a", "b"]),
            start,
        )
        .await
        .unwrap();
        // it waited about the timeout, not forever, and one run is left
        assert!(began.elapsed() < Duration::from_secs(5));
        assert_eq!(verdict(&outcomes, true), Verdict::TimedOut(1));
        assert_eq!(outcomes[1].status, "Started");
        assert!(
            summary_lines("nightly", &outcomes, true)
                .join("\n")
                .contains(&format!("Unfinished: {second}"))
        );
    }

    /// Only the runs a command started are waited on, and a run the tag listing missed is read on
    /// its own
    #[tokio::test]
    async fn following_counts_only_its_own_runs() {
        let (ours, theirs) = (Uuid::new_v4(), Uuid::new_v4());
        let (host, requests) = mock(move |line, _| {
            if line.starts_with("GET /api/reactions/tag/corn/nightly/") {
                // a run of someone else's with the same tag never finishes, and ours isn't listed
                (200, listing(&[(theirs, ReactionStatus::Started)]))
            } else if line.starts_with(&format!("GET /api/reactions/corn/{ours}")) {
                (
                    200,
                    serde_json::to_string(&reaction(ours, ReactionStatus::Completed)).unwrap(),
                )
            } else {
                (404, "{}".to_owned())
            }
        })
        .await;
        let thorium = client(&host).await;
        let start = |_inputs: RunInputs, _extra: Vec<String>| async move { Ok(ours) };
        let flags = common(&["--wait", "--interval", "10ms", "--timeout", "5s"]);
        let outcomes = execute(&thorium, &flags, &opts(1), "nightly", items(&["a"]), start)
            .await
            .unwrap();
        assert_eq!(verdict(&outcomes, true), Verdict::Success);
        assert!(
            requests
                .lock()
                .unwrap()
                .iter()
                .any(|(line, _)| line.contains(&ours.to_string()))
        );
    }

    /// A run that no longer exists fails its input without stopping the follow of the others
    #[tokio::test]
    async fn a_missing_run_does_not_abort_the_follow() {
        let (good, gone) = (Uuid::new_v4(), Uuid::new_v4());
        let (host, _) = mock(move |line, _| {
            if line.starts_with("GET /api/reactions/tag/corn/nightly/") {
                // the deleted run is never listed again
                (200, listing(&[(good, ReactionStatus::Completed)]))
            } else {
                // and reading it on its own answers 404
                (404, r#"{"error":"not found"}"#.to_owned())
            }
        })
        .await;
        let thorium = client(&host).await;
        let ids = HashMap::from([("a", good), ("b", gone)]);
        let start = |inputs: RunInputs, _extra: Vec<String>| {
            let id = ids[inputs.samples[0].as_str()];
            async move { Ok(id) }
        };
        let flags = common(&["--wait", "--interval", "10ms", "--timeout", "5s"]);
        let outcomes = execute(
            &thorium,
            &flags,
            &opts(2),
            "nightly",
            items(&["a", "b"]),
            start,
        )
        .await
        .unwrap();
        // the missing run failed its input, and the summary names it
        assert_eq!(outcomes[0].status, "Completed");
        assert_eq!(outcomes[1].status, "Failed");
        assert!(outcomes[1].error.is_some());
        assert_eq!(verdict(&outcomes, true), Verdict::Failed(1));
        let text = summary_lines("nightly", &outcomes, true).join("\n");
        assert!(text.contains(&format!("Failed: {gone} (b)")), "{text}");
    }

    /// A batch whose listing keeps being refused still stops after a few tries
    #[tokio::test]
    async fn persistent_listing_failures_still_abort() {
        let run = Uuid::new_v4();
        let (host, _) = mock(|_, _| (403, r#"{"error":"denied"}"#.to_owned())).await;
        let thorium = client(&host).await;
        let start = |_inputs: RunInputs, _extra: Vec<String>| async move { Ok(run) };
        let flags = common(&["--wait", "--interval", "10ms", "--timeout", "5s"]);
        let result = execute(&thorium, &flags, &opts(1), "nightly", items(&["a"]), start).await;
        assert!(result.is_err());
    }
}
