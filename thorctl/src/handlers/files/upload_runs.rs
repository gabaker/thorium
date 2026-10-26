//! Starts pipelines, workflows, and tools on the files an upload creates
//!
//! `files upload` hands each file it uploaded to a [`Starter`] through a bounded channel. The
//! starter gathers the files into chunks and starts every selected pipeline, workflow, and tool
//! on each file of a chunk: the pipelines' reactions in bulk requests of [`BULK_CHUNK`] reactions
//! and the workflow and tool runs one request each (those routes have no bulk form), at most
//! `--workers` at a time and with the retries and batch follow `run --one-per-input` uses.

use std::path::{Path, PathBuf};
use std::time::Duration;
use thorium::models::{
    EntityKinds, EntityMetadata, ReactionRequest, RunInputs, ToolRunRequest, WorkflowRunRequest,
};
use thorium::{Error, Thorium};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

use super::{Printer, error_text};
use crate::args::files::UploadFiles;
use crate::args::runs::RunCommonArgs;
use crate::handlers::instances::find_instance;
use crate::handlers::run_batch::{
    BatchItem, BatchOpts, MAX_ATTEMPTS, Outcome, Verdict, follow_batch, is_transient,
    prints_progress, start_one, status_line, summary_lines, verdict,
};
use crate::handlers::tools::find_runnable;
use crate::handlers::workflows::run_instance;
use crate::utils::watch;

/// The most reactions one bulk request creates
pub const BULK_CHUNK: usize = 100;

/// How long the starter waits for another uploaded file before it starts the runs of the files
/// it already has
const IDLE_FLUSH: Duration = Duration::from_secs(1);

/// A file that was uploaded and needs its runs started
#[derive(Debug, Clone)]
pub struct RunFile {
    /// The path the file was uploaded from
    pub path: PathBuf,
    /// The SHA256 of the file
    pub sha256: String,
}

/// Something to start on each uploaded file
#[derive(Debug, Clone)]
pub enum Target {
    /// A pipeline, started as a reaction
    Pipeline {
        /// The pipeline as `<group>/<name>`
        label: String,
        /// The reaction request without any file
        request: Box<ReactionRequest>,
    },
    /// A workflow instance, started as a workflow run
    Workflow {
        /// The workflow as the user named it
        label: String,
        /// The workflow instance to run
        instance: Uuid,
        /// The group to run in
        group: String,
        /// The flags the run is started and followed with
        common: Box<RunCommonArgs>,
    },
    /// A tool instance, started as a tool run
    Tool {
        /// The tool as the user named it
        label: String,
        /// The tool instance to run
        instance: Uuid,
        /// The group to run in
        group: String,
        /// The flags the run is started and followed with
        common: Box<RunCommonArgs>,
    },
}

impl Target {
    /// Get the kind of run this target starts as a word
    #[must_use]
    pub fn kind(&self) -> &'static str {
        // name each kind the way the run commands do
        match self {
            Target::Pipeline { .. } => "pipeline",
            Target::Workflow { .. } => "workflow",
            Target::Tool { .. } => "tool",
        }
    }

    /// Get the target as the user named it
    #[must_use]
    pub fn label(&self) -> &str {
        // every target keeps the name it was given
        match self {
            Target::Pipeline { label, .. }
            | Target::Workflow { label, .. }
            | Target::Tool { label, .. } => label,
        }
    }

    /// Get the group the target's runs are in
    #[must_use]
    pub fn group(&self) -> &str {
        // a pipeline's group is on its request
        match self {
            Target::Pipeline { request, .. } => &request.group,
            Target::Workflow { group, .. } | Target::Tool { group, .. } => group,
        }
    }
}

/// Split an optional leading group off a workflow or tool name
///
/// # Arguments
///
/// * `raw` - The name as typed: `<name>` or `<group>/<name>`
#[must_use]
pub fn split_group(raw: &str) -> (Option<&str>, &str) {
    // a group needs a name after its slash and a name before it
    match raw.split_once('/') {
        Some((group, name)) if !group.is_empty() && !name.is_empty() => (Some(group), name),
        _ => (None, raw),
    }
}

/// Pick the group a workflow or tool runs in on files uploaded to some groups
///
/// # Arguments
///
/// * `label` - The workflow or tool as the user named it
/// * `named` - The group the user put before the name, if any
/// * `allowed` - The groups the workflow or tool instance is in
/// * `uploaded` - The groups the files are uploaded to
pub fn pick_group(
    label: &str,
    named: Option<&str>,
    allowed: &[String],
    uploaded: &[String],
) -> Result<String, Error> {
    // a named group must hold both the instance and the files
    if let Some(group) = named {
        if !allowed.iter().any(|allowed| allowed == group) {
            return Err(Error::new(format!(
                "'{label}' is not in group '{group}' (it is in: {})",
                allowed.join(", ")
            )));
        }
        if !uploaded.iter().any(|uploaded| uploaded == group) {
            return Err(Error::new(format!(
                "'{label}' names group '{group}', which the files are not uploaded to \
                 (--groups {})",
                uploaded.join(",")
            )));
        }
        return Ok(group.to_owned());
    }
    // otherwise the run must be in a group that holds the instance and the files
    let shared: Vec<&String> = allowed
        .iter()
        .filter(|group| uploaded.contains(group))
        .collect();
    match shared.as_slice() {
        [group] => Ok((*group).clone()),
        [] => Err(Error::new(format!(
            "'{label}' is in none of the groups the files are uploaded to ({})",
            uploaded.join(", ")
        ))),
        several => Err(Error::new(format!(
            "'{label}' can run in several of the upload groups ({}); name one as <GROUP>/{label}",
            several
                .iter()
                .map(|group| group.as_str())
                .collect::<Vec<&str>>()
                .join(", ")
        ))),
    }
}

/// Find the workflow instances, tool instances, and pipelines an upload starts, or refuse any
/// that cannot run
///
/// Nothing is uploaded until every target is known to exist, so a typo is caught before any file
/// moves.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The files upload command
/// * `common` - The flags every run is followed with
pub async fn resolve(
    thorium: &Thorium,
    cmd: &UploadFiles,
    common: &RunCommonArgs,
) -> Result<Vec<Target>, Error> {
    let mut targets = Vec::new();
    // pipelines come first, each as a reaction request without a file
    for request in super::build_reaction_reqs(thorium, cmd).await? {
        targets.push(Target::Pipeline {
            label: format!("{}/{}", request.group, request.pipeline),
            request: Box::new(request),
        });
    }
    // workflows run at the instance their name or id means
    for raw in cmd.runs.workflows.iter().flatten() {
        let (named, name) = split_group(raw);
        let id = run_instance(thorium, name).await?;
        let instance = find_instance(thorium, &id).await?;
        if instance.kind != EntityKinds::WorkflowInstance {
            return Err(Error::new(format!(
                "'{raw}' is a {}, not a workflow instance",
                instance.kind
            )));
        }
        let group = pick_group(raw, named, &instance.groups, &cmd.file_groups)?;
        let mut common = common.clone();
        common.group = Some(group.clone());
        targets.push(Target::Workflow {
            label: raw.clone(),
            instance: instance.id,
            group,
            common: Box::new(common),
        });
    }
    // tools run at the instance their name or id means
    for raw in cmd.runs.tools.iter().flatten() {
        let (named, name) = split_group(raw);
        // a group-qualified tool name is looked up in its group, and a version names an instance
        let lookup = if name.contains('@') { name } else { raw };
        let instance = find_runnable(thorium, lookup).await?;
        // a tool that cannot run on a file with no input is refused now
        tool_runs_plan(&instance, raw)?;
        let group = pick_group(raw, named, &instance.groups, &cmd.file_groups)?;
        let mut common = common.clone();
        common.group = Some(group.clone());
        targets.push(Target::Tool {
            label: raw.clone(),
            instance: instance.id,
            group,
            common: Box::new(common),
        });
    }
    Ok(targets)
}

/// Check that a tool instance can run with no input beyond the files it is given
///
/// # Arguments
///
/// * `instance` - The tool instance
/// * `raw` - The tool as the user named it
fn tool_runs_plan(instance: &thorium::models::Entity, raw: &str) -> Result<(), Error> {
    // only tool instances run, and the plan names why one cannot
    if !matches!(instance.metadata, EntityMetadata::ToolInstance(_)) {
        return Err(Error::new(format!(
            "'{raw}' is a {}, not a tool instance",
            instance.kind
        )));
    }
    thorium::client::tool_runs::plan(instance, &serde_json::Map::new())
        .map(|_| ())
        .map_err(|error| Error::new(format!("Unable to run '{raw}': {error}")))
}

/// What became of one target on one file
#[derive(Debug, Clone)]
pub struct Started {
    /// The path the file was uploaded from
    pub path: PathBuf,
    /// The SHA256 of the file
    pub sha256: String,
    /// The kind of run
    pub kind: &'static str,
    /// The pipeline, workflow, or tool as the user named it
    pub target: String,
}

/// Starts the runs of the files an upload creates and reports how they went
pub struct Starter<'a> {
    /// The Thorium client
    thorium: &'a Thorium,
    /// What to start on each file, pipelines first
    targets: Vec<Target>,
    /// The batch name every run is tagged with, when there is one
    batch: Option<String>,
    /// How runs are started and printed
    opts: BatchOpts,
    /// The flags the runs are followed and summarized with
    common: RunCommonArgs,
    /// How the lines are printed
    printer: Printer,
    /// What each started run was, parallel to `outcomes`
    started: Vec<Started>,
    /// What became of each run, parallel to `started`
    outcomes: Vec<Outcome>,
}

impl<'a> Starter<'a> {
    /// Build a starter for some targets
    ///
    /// # Arguments
    ///
    /// * `thorium` - The Thorium client
    /// * `targets` - What to start on each file
    /// * `batch` - The batch name to tag runs with
    /// * `opts` - How runs are started and printed
    /// * `common` - The flags the runs are followed and summarized with
    /// * `printer` - How lines are printed
    #[must_use]
    pub fn new(
        thorium: &'a Thorium,
        targets: Vec<Target>,
        batch: Option<String>,
        opts: BatchOpts,
        common: RunCommonArgs,
        printer: Printer,
    ) -> Self {
        Starter {
            thorium,
            targets,
            batch,
            opts,
            common,
            printer,
            started: Vec::new(),
            outcomes: Vec::new(),
        }
    }

    /// Get how many files make up a chunk
    ///
    /// A chunk never holds more than [`BULK_CHUNK`] reactions, so each file takes as many as there
    /// are pipelines.
    #[must_use]
    pub fn files_per_chunk(&self) -> usize {
        // count the reactions one file adds to a bulk request
        let pipelines = self
            .targets
            .iter()
            .filter(|target| matches!(target, Target::Pipeline { .. }))
            .count()
            .max(1);
        (BULK_CHUNK / pipelines).max(1)
    }

    /// Start the runs of the files that arrive until the sender is dropped
    ///
    /// Files are started a chunk at a time: when a chunk is full, when no file arrived for a
    /// moment, and when no more will.
    ///
    /// # Arguments
    ///
    /// * `files` - The files that were uploaded
    pub async fn run(&mut self, mut files: mpsc::Receiver<RunFile>) {
        let per_chunk = self.files_per_chunk();
        let mut chunk: Vec<RunFile> = Vec::with_capacity(per_chunk);
        loop {
            // wait for the next file, but not so long that finished uploads sit unstarted
            match timeout(IDLE_FLUSH, files.recv()).await {
                Ok(Some(file)) => {
                    chunk.push(file);
                    // start a full chunk
                    if chunk.len() >= per_chunk {
                        self.start_chunk(std::mem::take(&mut chunk)).await;
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    // nothing new arrived, so start what we have
                    if !chunk.is_empty() {
                        self.start_chunk(std::mem::take(&mut chunk)).await;
                    }
                }
            }
        }
        // start the files that were left when the uploads ended
        if !chunk.is_empty() {
            self.start_chunk(chunk).await;
        }
    }

    /// Start every target on every file of a chunk and report each run
    ///
    /// # Arguments
    ///
    /// * `files` - The chunk of uploaded files
    async fn start_chunk(&mut self, files: Vec<RunFile>) {
        // start the pipelines in bulk and the workflows and tools one by one, together
        let (pipelines, others) =
            tokio::join!(self.start_pipelines(&files), self.start_others(&files));
        let mut outcomes: Vec<Option<Outcome>> = vec![None; files.len() * self.targets.len()];
        for (slot, outcome) in pipelines.into_iter().chain(others) {
            outcomes[slot] = Some(outcome);
        }
        // report and keep the outcomes in the order of the files, then the targets
        for (slot, outcome) in outcomes.into_iter().enumerate() {
            let Some(outcome) = outcome else {
                continue;
            };
            let file = &files[slot / self.targets.len()];
            let target = &self.targets[slot % self.targets.len()];
            self.printer
                .run_line(&file.path, &file.sha256, target, &outcome);
            self.started.push(Started {
                path: file.path.clone(),
                sha256: file.sha256.clone(),
                kind: target.kind(),
                target: target.label().to_owned(),
            });
            self.outcomes.push(outcome);
        }
    }

    /// Get the input text of a run, which names what ran on what
    ///
    /// # Arguments
    ///
    /// * `target` - What was started
    /// * `path` - The file it was started on
    fn input(target: &Target, path: &Path) -> String {
        // the kind, the target, and the file the run is on
        format!("{} {} {}", target.kind(), target.label(), path.display())
    }

    /// Create the reactions of every pipeline for a chunk of files in bulk requests
    ///
    /// Returns each outcome with the slot of its file and target.
    ///
    /// # Arguments
    ///
    /// * `files` - The chunk of uploaded files
    async fn start_pipelines(&self, files: &[RunFile]) -> Vec<(usize, Outcome)> {
        // build one reaction request per file and pipeline
        let mut slots = Vec::new();
        let mut reqs = Vec::new();
        for (file_index, file) in files.iter().enumerate() {
            for (target_index, target) in self.targets.iter().enumerate() {
                if let Target::Pipeline { request, .. } = target {
                    let mut req = request.as_ref().clone().sample(file.sha256.clone());
                    if let Some(batch) = &self.batch {
                        req = req.tag(batch);
                    }
                    slots.push(file_index * self.targets.len() + target_index);
                    reqs.push(req);
                }
            }
        }
        // create the reactions a bulk chunk at a time
        let mut results = Vec::with_capacity(reqs.len());
        for chunk in reqs.chunks(BULK_CHUNK) {
            results.extend(self.create_chunk(chunk).await);
        }
        // pair each result with its file and pipeline
        slots
            .into_iter()
            .zip(results)
            .map(|(slot, result)| {
                let target = &self.targets[slot % self.targets.len()];
                let path = &files[slot / self.targets.len()].path;
                let input = Self::input(target, path);
                let outcome = match result {
                    Ok(id) => Outcome::created(input, id, target.group().to_owned()),
                    Err(error) => Outcome::not_started(input, error),
                };
                (slot, outcome)
            })
            .collect()
    }

    /// Create one bulk chunk of reactions, trying again when the failure is likely to pass
    ///
    /// Returns the id of each reaction or why it was not created, in the order of the requests.
    ///
    /// # Arguments
    ///
    /// * `reqs` - The reaction requests of this chunk
    async fn create_chunk(&self, reqs: &[ReactionRequest]) -> Vec<Result<Uuid, String>> {
        let mut attempt = 0;
        // send the chunk, waiting longer before each retry
        let sent = loop {
            match self.thorium.reactions.create_bulk(reqs).await {
                Ok(resp) => break Ok(resp),
                Err(error) if attempt + 1 < MAX_ATTEMPTS && is_transient(&error) => {
                    sleep(self.opts.retry_base * 2_u32.pow(attempt)).await;
                    attempt += 1;
                }
                Err(error) => break Err(error),
            }
        };
        match sent {
            // a refused chunk fails every reaction in it
            Err(error) => vec![Err(error_text(&error)); reqs.len()],
            Ok(resp) => {
                // created ids come back in order for the requests that had no error
                let mut created = resp.created.into_iter();
                (0..reqs.len())
                    .map(|index| match resp.errors.get(&index) {
                        Some(error) => Err(error.clone()),
                        None => created.next().ok_or_else(|| {
                            "the API did not return an id for this reaction".to_owned()
                        }),
                    })
                    .collect()
            }
        }
    }

    /// Start the workflow and tool runs of a chunk of files, a few at a time
    ///
    /// Returns each outcome with the slot of its file and target.
    ///
    /// # Arguments
    ///
    /// * `files` - The chunk of uploaded files
    async fn start_others(&self, files: &[RunFile]) -> Vec<(usize, Outcome)> {
        use futures::StreamExt;
        // one job per file and workflow or tool
        let jobs: Vec<(usize, usize)> = (0..files.len())
            .flat_map(|file_index| {
                (0..self.targets.len())
                    .filter(|target_index| {
                        !matches!(self.targets[*target_index], Target::Pipeline { .. })
                    })
                    .map(move |target_index| (file_index, target_index))
            })
            .collect();
        // start the jobs within the workers bound
        futures::stream::iter(jobs)
            .map(|(file_index, target_index)| async move {
                let file = &files[file_index];
                let target = &self.targets[target_index];
                let outcome = self.start_other(file, target).await;
                (file_index * self.targets.len() + target_index, outcome)
            })
            .buffer_unordered(self.opts.workers.max(1))
            .collect()
            .await
    }

    /// Start one workflow or tool run on one file
    ///
    /// # Arguments
    ///
    /// * `file` - The uploaded file
    /// * `target` - The workflow or tool to run
    async fn start_other(&self, file: &RunFile, target: &Target) -> Outcome {
        let thorium = self.thorium;
        // every workflow and tool run is tagged with the batch name
        let batch = self.batch.clone().unwrap_or_default();
        let item = BatchItem {
            label: Self::input(target, &file.path),
            inputs: RunInputs::default().sample(file.sha256.clone()),
        };
        match target {
            Target::Workflow {
                instance,
                group,
                common,
                ..
            } => {
                let start = |inputs: RunInputs, tags: Vec<String>| {
                    let mut req = WorkflowRunRequest::new(inputs);
                    req.group = Some(group.clone());
                    req.tags = tags;
                    let instance = *instance;
                    async move { Ok(thorium.reactions.run_workflow(instance, &req).await?.id) }
                };
                start_one(thorium, common, &self.opts, &batch, &start, item).await
            }
            Target::Tool {
                instance,
                group,
                common,
                ..
            } => {
                let start = |inputs: RunInputs, tags: Vec<String>| {
                    let mut req = ToolRunRequest::new(inputs).group(group);
                    req.tags = tags;
                    let instance = *instance;
                    async move { Ok(thorium.reactions.run_tool(instance, &req).await?.id) }
                };
                start_one(thorium, common, &self.opts, &batch, &start, item).await
            }
            // pipelines are started in bulk and never get here
            Target::Pipeline { .. } => Outcome::not_started(
                item.label,
                "a pipeline is not started one run at a time".to_owned(),
            ),
        }
    }

    /// Follow the runs as the flags ask, print how they went, and name the exit code
    ///
    /// # Arguments
    ///
    /// * `upload_failures` - How many files failed to upload
    /// * `files` - How many files were looked at
    pub async fn finish(mut self, upload_failures: usize, files: usize) -> Result<(), Error> {
        let followed = self.common.follows_batch();
        // follow the runs that started
        if followed && !self.outcomes.is_empty() {
            let batch = self.batch.clone().unwrap_or_default();
            if prints_progress(&self.common, &self.opts) {
                println!("{}", status_line("STATUS", "RUN ID", "INPUT"));
            }
            follow_batch(
                self.thorium,
                &self.common,
                &self.opts,
                &batch,
                &mut self.outcomes,
            )
            .await?;
        }
        // describe each run as a JSON line
        if self.printer.json {
            for (started, outcome) in self.started.iter().zip(&self.outcomes) {
                Printer::run_json(started, outcome);
            }
        }
        // summarize the runs of a batch by status
        if let (Some(batch), false) = (&self.batch, self.outcomes.is_empty()) {
            for line in summary_lines(batch, &self.outcomes, followed) {
                if self.printer.json {
                    eprintln!("{line}");
                } else {
                    println!("{line}");
                }
            }
        }
        // name how the upload and its runs ended in the exit code
        let runs = self.outcomes.len();
        match (upload_failures, verdict(&self.outcomes, followed)) {
            (0, Verdict::Success) => Ok(()),
            (0, Verdict::Failed(count)) => Err(Error::new(format!(
                "{count} of {runs} runs failed or did not start"
            ))),
            (0, Verdict::TimedOut(count)) => {
                // exit with the timeout code
                watch::mark_timed_out();
                Err(Error::new(format!(
                    "Timed out after {:?} with {count} of {runs} runs unfinished",
                    self.common.wait_for.timeout
                )))
            }
            (uploads, Verdict::Failed(count)) => Err(Error::new(format!(
                "{uploads} of {files} files failed to upload and {count} of {runs} runs failed \
                 or did not start"
            ))),
            (uploads, _) => Err(Error::new(format!(
                "{uploads} of {files} files failed to upload"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use thorium::models::ReactionRequest;
    use tokio::sync::mpsc;
    use uuid::Uuid;

    use super::{BULK_CHUNK, RunFile, Starter, Target, pick_group, split_group};
    use crate::args::files::UploadRunArgs;
    use crate::handlers::files::Printer;
    use crate::handlers::run_batch::{BatchOpts, Outcome};
    use crate::utils::run_wait::testing::{client, mock};

    /// The printer the tests use: JSON so nothing is printed to the table
    const PRINTER: Printer = Printer {
        json: true,
        quiet: true,
    };

    /// Build batch options with quick retries and a worker bound
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

    /// Build a pipeline target
    ///
    /// # Arguments
    ///
    /// * `name` - The pipeline's name in the corn group
    fn pipeline(name: &str) -> Target {
        Target::Pipeline {
            label: format!("corn/{name}"),
            request: Box::new(ReactionRequest::new("corn", name)),
        }
    }

    /// Build a workflow target in the corn group
    fn workflow() -> Target {
        Target::Workflow {
            label: "triage".to_owned(),
            instance: Uuid::new_v4(),
            group: "corn".to_owned(),
            common: Box::new(UploadRunArgs::default().common()),
        }
    }

    /// Build the files of an upload
    ///
    /// # Arguments
    ///
    /// * `count` - How many files were uploaded
    fn files(count: usize) -> Vec<RunFile> {
        (0..count)
            .map(|index| RunFile {
                path: PathBuf::from(format!("f{index}.bin")),
                sha256: format!("sha{index}"),
            })
            .collect()
    }

    /// Run a starter over files sent through its channel and return it
    ///
    /// # Arguments
    ///
    /// * `starter` - The starter to run
    /// * `files` - The files to send
    async fn drive(starter: &mut Starter<'_>, files: Vec<RunFile>) {
        // send every file and close the channel
        let (tx, rx) = mpsc::channel(starter.files_per_chunk());
        let send = async move {
            for file in files {
                tx.send(file).await.unwrap();
            }
        };
        tokio::join!(send, starter.run(rx));
    }

    /// A bulk answer that creates every request, failing those whose body names a refused file
    ///
    /// # Arguments
    ///
    /// * `body` - The bulk request's body
    fn bulk_answer(body: &str) -> (u16, String) {
        // answer each request in order, with an error for the refused ones
        let reqs: Vec<Value> = serde_json::from_str(body).unwrap();
        let mut errors = serde_json::Map::new();
        let mut created = Vec::new();
        for (index, req) in reqs.iter().enumerate() {
            if req.to_string().contains("refused") {
                errors.insert(index.to_string(), json!("pipeline is banned"));
            } else {
                created.push(Uuid::new_v4());
            }
        }
        (
            200,
            json!({"errors": errors, "created": created}).to_string(),
        )
    }

    /// Group names are only split off when both sides are present
    #[test]
    fn a_group_prefix_needs_a_name_on_both_sides() {
        assert_eq!(split_group("corn/triage"), (Some("corn"), "triage"));
        assert_eq!(split_group("triage"), (None, "triage"));
        assert_eq!(split_group("/triage"), (None, "/triage"));
        assert_eq!(split_group("corn/"), (None, "corn/"));
        // only the first slash splits
        assert_eq!(split_group("a/b/c"), (Some("a"), "b/c"));
    }

    /// The run group is the one group the instance and the files share, or a named one
    #[test]
    fn pick_group_needs_exactly_one_shared_group() {
        let groups = |names: &[&str]| names.iter().map(ToString::to_string).collect::<Vec<_>>();
        // one shared group is used
        assert_eq!(
            pick_group("w", None, &groups(&["a", "b"]), &groups(&["b", "c"])).unwrap(),
            "b"
        );
        // no shared group is refused
        let none = pick_group("w", None, &groups(&["a"]), &groups(&["b"])).unwrap_err();
        assert!(none.to_string().contains("none of the groups"), "{none}");
        // several shared groups ask for one to be named
        let many = pick_group("w", None, &groups(&["a", "b"]), &groups(&["a", "b"])).unwrap_err();
        assert!(many.to_string().contains("<GROUP>/w"), "{many}");
        // a named group must hold the instance and the files
        assert_eq!(
            pick_group("w", Some("a"), &groups(&["a", "b"]), &groups(&["a", "b"])).unwrap(),
            "a"
        );
        assert!(pick_group("w", Some("c"), &groups(&["a"]), &groups(&["a", "c"])).is_err());
        assert!(pick_group("w", Some("a"), &groups(&["a"]), &groups(&["b"])).is_err());
    }

    /// Pipelines are created one bulk request per chunk, never one request per file
    #[tokio::test]
    async fn pipelines_are_created_in_bulk_chunks() {
        let (host, requests) = mock(|_, body| bulk_answer(body)).await;
        let thorium = client(&host).await;
        let mut starter = Starter::new(
            &thorium,
            vec![pipeline("harvest")],
            None,
            opts(4),
            UploadRunArgs::default().common(),
            PRINTER,
        );
        // 250 files with one pipeline make chunks of 100, 100, and 50
        drive(&mut starter, files(250)).await;
        let log = requests.lock().unwrap().clone();
        assert_eq!(starter.files_per_chunk(), BULK_CHUNK);
        assert_eq!(log.len(), 3, "{log:?}");
        assert!(
            log.iter()
                .all(|(line, _)| line.contains("/api/reactions/bulk/"))
        );
        let sizes: Vec<usize> = log
            .iter()
            .map(|(_, body)| serde_json::from_str::<Vec<Value>>(body).unwrap().len())
            .collect();
        assert_eq!(sizes, [100, 100, 50]);
        // every file got a created reaction and the order of the files is kept
        assert_eq!(starter.outcomes.len(), 250);
        assert!(starter.outcomes.iter().all(|outcome| outcome.id.is_some()));
        assert_eq!(starter.started[0].path, PathBuf::from("f0.bin"));
        assert_eq!(starter.started[249].path, PathBuf::from("f249.bin"));
        // a pipeline only upload with no batch leaves the reactions untagged
        assert!(log[0].1.contains(r#""tags":[]"#), "{}", log[0].1);
    }

    /// Several pipelines on each file share chunks of at most the bulk size
    #[tokio::test]
    async fn several_pipelines_share_chunks() {
        let (host, requests) = mock(|_, body| bulk_answer(body)).await;
        let thorium = client(&host).await;
        let mut starter = Starter::new(
            &thorium,
            vec![pipeline("a"), pipeline("b"), pipeline("c")],
            Some("nightly".to_owned()),
            opts(4),
            UploadRunArgs::default().common(),
            PRINTER,
        );
        // three pipelines make 33 files per chunk, so 70 files are 3 requests of 99, 99, 12
        drive(&mut starter, files(70)).await;
        assert_eq!(starter.files_per_chunk(), 33);
        let log = requests.lock().unwrap().clone();
        let sizes: Vec<usize> = log
            .iter()
            .map(|(_, body)| serde_json::from_str::<Vec<Value>>(body).unwrap().len())
            .collect();
        assert_eq!(sizes, [99, 99, 12]);
        // every reaction names its file and carries the batch tag
        let first: Vec<Value> = serde_json::from_str(&log[0].1).unwrap();
        assert_eq!(first[0]["samples"], json!(["sha0"]));
        assert_eq!(first[0]["tags"], json!(["nightly"]));
        assert_eq!(first[1]["pipeline"], "b");
        assert_eq!(starter.outcomes.len(), 210);
    }

    /// A reaction the API refuses is that file's failure and the others still start
    #[tokio::test]
    async fn a_refused_reaction_fails_only_its_file() {
        let (host, _) = mock(|_, body| bulk_answer(body)).await;
        let thorium = client(&host).await;
        let mut starter = Starter::new(
            &thorium,
            vec![pipeline("harvest")],
            None,
            opts(4),
            UploadRunArgs::default().common(),
            PRINTER,
        );
        let mut sent = files(4);
        sent[2].sha256 = "refused".to_owned();
        drive(&mut starter, sent).await;
        // the refused file has no run and the others each got the next created id
        let ids: Vec<bool> = starter.outcomes.iter().map(|o| o.id.is_some()).collect();
        assert_eq!(ids, [true, true, false, true]);
        assert_eq!(
            starter.outcomes[2].error.as_deref(),
            Some("pipeline is banned")
        );
        // the failure is the exit code
        let err = starter.finish(0, 4).await.unwrap_err();
        assert!(err.to_string().contains("1 of 4 runs failed"), "{err}");
    }

    /// A refused bulk request fails every reaction in its chunk without retrying a refusal
    #[tokio::test]
    async fn a_refused_chunk_fails_all_its_reactions() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let (host, _) = mock(move |_, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            (
                400,
                r#"{"error":"Unable to create reaction(s)! banned"}"#.to_owned(),
            )
        })
        .await;
        let thorium = client(&host).await;
        let mut starter = Starter::new(
            &thorium,
            vec![pipeline("harvest")],
            None,
            opts(4),
            UploadRunArgs::default().common(),
            PRINTER,
        );
        drive(&mut starter, files(3)).await;
        // one request, three failures that say why
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(starter.outcomes.len(), 3);
        assert!(starter.outcomes.iter().all(|o| o.id.is_none()));
        assert!(
            starter.outcomes[0]
                .error
                .as_deref()
                .unwrap()
                .contains("banned")
        );
    }

    /// A busy API is retried and the chunk then succeeds
    #[tokio::test]
    async fn a_busy_bulk_request_is_retried() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let (host, _) = mock(move |_, body| {
            if counter.fetch_add(1, Ordering::SeqCst) < 2 {
                (503, r#"{"error":"busy"}"#.to_owned())
            } else {
                bulk_answer(body)
            }
        })
        .await;
        let thorium = client(&host).await;
        let mut starter = Starter::new(
            &thorium,
            vec![pipeline("harvest")],
            None,
            opts(4),
            UploadRunArgs::default().common(),
            PRINTER,
        );
        drive(&mut starter, files(2)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(starter.outcomes.iter().all(|o| o.id.is_some()));
    }

    /// Workflow runs start once per file within the workers bound and carry the batch tag
    #[tokio::test]
    async fn workflow_runs_start_per_file_within_the_workers_bound() {
        let in_flight = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let (flight, peak) = (in_flight.clone(), most.clone());
        let (host, requests) = mock(move |line, _| {
            if line.contains("/runs") {
                // count the overlap with a blocking pause the mock's thread pool can see
                let now = flight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(30));
                flight.fetch_sub(1, Ordering::SeqCst);
                (200, json!({"id": Uuid::new_v4()}).to_string())
            } else {
                (404, "{}".to_owned())
            }
        })
        .await;
        let thorium = client(&host).await;
        let mut starter = Starter::new(
            &thorium,
            vec![workflow()],
            Some("nightly".to_owned()),
            opts(2),
            UploadRunArgs::default().common(),
            PRINTER,
        );
        drive(&mut starter, files(6)).await;
        // one request per file, each on its own file and tagged with the batch
        let log = requests.lock().unwrap().clone();
        assert_eq!(log.len(), 6);
        let mut shas: Vec<String> = log
            .iter()
            .map(|(_, body)| {
                let body: Value = serde_json::from_str(body).unwrap();
                assert_eq!(body["tags"], json!(["nightly"]));
                assert_eq!(body["group"], "corn");
                body["inputs"]["samples"][0].as_str().unwrap().to_owned()
            })
            .collect();
        shas.sort();
        assert_eq!(shas, ["sha0", "sha1", "sha2", "sha3", "sha4", "sha5"]);
        assert!(starter.outcomes.iter().all(|o| o.id.is_some()));
        // no run is in the corn group's unknown state and the input names the target and file
        assert!(
            starter.outcomes[0]
                .input
                .starts_with("workflow triage f0.bin")
        );
        // the mock answers serially, so only the bound itself is checked
        assert!(most.load(Ordering::SeqCst) <= 2);
    }

    /// A workflow run that fails to start is that file's failure and the exit code names it
    #[tokio::test]
    async fn a_failed_workflow_start_is_an_exit_error() {
        let (host, _) = mock(|_, body| {
            if body.contains("sha1") {
                (400, r#"{"error":"nope"}"#.to_owned())
            } else {
                (200, json!({"id": Uuid::new_v4()}).to_string())
            }
        })
        .await;
        let thorium = client(&host).await;
        let mut starter = Starter::new(
            &thorium,
            vec![workflow()],
            Some("b".to_owned()),
            opts(2),
            UploadRunArgs::default().common(),
            PRINTER,
        );
        drive(&mut starter, files(3)).await;
        let failed: Vec<bool> = starter.outcomes.iter().map(Outcome::failed).collect();
        assert_eq!(failed, [false, true, false]);
        let err = starter.finish(0, 3).await.unwrap_err();
        assert!(err.to_string().contains("1 of 3 runs failed"), "{err}");
    }

    /// Upload failures are the exit code even when every run started
    #[tokio::test]
    async fn upload_failures_fail_the_command() {
        let (host, _) = mock(|_, body| bulk_answer(body)).await;
        let thorium = client(&host).await;
        let mut starter = Starter::new(
            &thorium,
            vec![pipeline("harvest")],
            None,
            opts(2),
            UploadRunArgs::default().common(),
            PRINTER,
        );
        drive(&mut starter, files(2)).await;
        let err = starter.finish(1, 3).await.unwrap_err();
        assert!(
            err.to_string().contains("1 of 3 files failed to upload"),
            "{err}"
        );
        // no failures and every run started is success
        let (host, _) = mock(|_, body| bulk_answer(body)).await;
        let thorium = client(&host).await;
        let mut starter = Starter::new(
            &thorium,
            vec![pipeline("harvest")],
            None,
            opts(2),
            UploadRunArgs::default().common(),
            PRINTER,
        );
        drive(&mut starter, files(2)).await;
        starter.finish(0, 2).await.unwrap();
    }

    /// Files that arrive slowly are started after a pause instead of waiting for a full chunk
    #[tokio::test]
    async fn a_pause_in_uploads_starts_the_chunk_so_far() {
        let (host, requests) = mock(|_, body| bulk_answer(body)).await;
        let thorium = client(&host).await;
        let mut starter = Starter::new(
            &thorium,
            vec![pipeline("harvest")],
            None,
            opts(2),
            UploadRunArgs::default().common(),
            PRINTER,
        );
        let (tx, rx) = mpsc::channel(10);
        let log = requests.clone();
        let send = async move {
            // two files, a pause longer than the idle wait, then one more
            for file in files(2) {
                tx.send(file).await.unwrap();
            }
            tokio::time::sleep(Duration::from_millis(1500)).await;
            assert_eq!(log.lock().unwrap().len(), 1);
            tx.send(files(3).pop().unwrap()).await.unwrap();
        };
        tokio::join!(send, starter.run(rx));
        // the first two went in one request and the last in another
        assert_eq!(requests.lock().unwrap().len(), 2);
        assert_eq!(starter.outcomes.len(), 3);
    }

    /// Nothing is started for a pipeline only upload that had no new files
    #[tokio::test]
    async fn no_files_means_no_requests() {
        let (host, requests) = mock(|_, body| bulk_answer(body)).await;
        let thorium = client(&host).await;
        let mut starter = Starter::new(
            &thorium,
            vec![pipeline("harvest"), workflow()],
            None,
            opts(2),
            UploadRunArgs::default().common(),
            PRINTER,
        );
        drive(&mut starter, Vec::new()).await;
        assert!(requests.lock().unwrap().is_empty());
        starter.finish(0, 0).await.unwrap();
    }
}
