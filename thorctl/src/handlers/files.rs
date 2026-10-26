//! Handles files commands

use crate::utils::paint::Paint;
use download::FilesDownloadWorker;
use futures::stream::{self, StreamExt};
use http::status::StatusCode;
use regex::RegexSet;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use thorium::models::{
    FileDeleteOpts, ReactionRequest, Sample, SampleListLine, SampleSubmissionResponse,
    SubmissionChunk,
};
use thorium::{CtlConf, Error, Thorium};
use tokio::sync::mpsc;
use uuid::Uuid;

mod download;
pub mod upload_runs;

use super::{Controller, update};
use crate::args::files::{
    CountFiles, DeleteFiles, DescribeFiles, DownloadFiles, Files, GetFiles, UploadFiles,
};
use crate::args::pipelines::PipelineTarget;
use crate::args::{Args, DescribeCommand, SearchParameterized};
use crate::handlers::run_batch::{BatchOpts, Outcome};
use crate::utils;
use crate::utils::output::OutputFormat;
use upload_runs::{RunFile, Started, Starter, Target};

/// A single line for an file upload log
struct UploadLine;

/// Format one upload log line into a string, padded to the columns of the upload header
///
/// # Arguments
///
/// * `$status` - The status code shown in the first column
/// * `$path` - The path the file was uploaded from
/// * `$sha256` - The file's SHA256
/// * `$id` - The submission id
/// * `$msg` - The message shown in the last column
macro_rules! upload_fmt {
    ($status:expr, $path:expr, $sha256:expr, $id:expr, $msg:expr) => {
        format!(
            "{:<4} | {:<32} | {:<64} | {:<36} | {:<24}",
            $status,
            $path.to_string_lossy(),
            $sha256,
            $id,
            $msg
        )
    };
}

/// Print one upload log line, formatted by [`upload_fmt`]
///
/// # Arguments
///
/// * `$status` - The status code shown in the first column
/// * `$path` - The path the file was uploaded from
/// * `$sha256` - The file's SHA256
/// * `$id` - The submission id
/// * `$msg` - The message shown in the last column
macro_rules! upload_print {
    ($status:expr, $path:expr, $sha256:expr, $id:expr, $msg:expr) => {
        println!("{}", upload_fmt!($status, $path, $sha256, $id, $msg))
    };
}

macro_rules! upload_print_dry_run {
    ($status:expr, $path:expr, $sha256:expr, $msg:expr) => {
        println!(
            "{:<4} | {:<32} | {:<64} | {}",
            $status,
            $path.to_string_lossy(),
            $sha256,
            $msg
        )
    };
}

impl UploadLine {
    /// Print this log lines header
    #[allow(clippy::print_literal)]
    pub fn header() {
        println!(
            "{} | {:<32} | {:<64} | {:<36} | {:<24}",
            "CODE", "PATH", "SHA256", "SUBMISSION", "MESSAGE"
        );
        println!("{:-<5}+{:-<34}+{:-<66}+{:-<38}+{:-<26}", "", "", "", "", "");
    }

    /// Print this log lines header
    #[allow(clippy::print_literal)]
    pub fn header_dry_run() {
        println!(
            "{} | {:<32} | {:<64} | {}",
            "CODE", "PATH", "SHA256", "MESSAGE"
        );
        println!("{:-<5}+{:-<34}+{:-<66}+{:-<60}", "", "", "", "");
    }

    /// Build and print a successful file upload log line
    ///
    /// # Arguments
    ///
    /// * `path` - The path this file was uploaded from
    /// * `resp` - The submission response from the API
    pub fn uploaded(path: &Path, resp: &SampleSubmissionResponse) {
        // print an uploaded line
        upload_print!("200".bright_green(), path, resp.sha256, resp.id, "-");
    }

    /// Build and print that this file was already uploaded
    ///
    /// # Arguments
    ///
    /// * `path` - The path this file was uploaded from
    /// * `sha256` - The sha256 for this file
    pub fn conflict(path: &Path, sha256: &str) {
        upload_print!("409".bright_blue(), path, sha256, "-", "Already Exists");
    }

    /// Build and print a successful file upload log line but for
    /// a dry run
    ///
    /// # Arguments
    ///
    /// * `path` - The path this file was uploaded from
    /// * `sha256` - The SHA256 of the file
    /// * `tags` - The tags the file would be given
    /// * `runs` - The runs that would start on the file, as text
    pub fn uploaded_dry_run(
        path: &Path,
        sha256: &str,
        tags: &HashMap<String, HashSet<String>>,
        runs: &str,
    ) {
        // construct a message to display the tags assigned to the file
        let tags_msg = if tags.is_empty() {
            String::new()
        } else {
            tags.iter()
                .fold("Tags: ".to_string(), |mut msg, (key, values)| {
                    for value in values {
                        write!(&mut msg, "{key}={value}, ").unwrap();
                    }
                    msg
                })
                .trim_end_matches(", ")
                .to_string()
        };
        // add the runs that would start
        let msg = match (tags_msg.is_empty(), runs.is_empty()) {
            (_, true) => tags_msg,
            (true, false) => format!("Runs: {runs}"),
            (false, false) => format!("{tags_msg}; Runs: {runs}"),
        };
        // print an uploaded dry run line
        upload_print_dry_run!("-", path, sha256, msg);
    }

    /// Build the line that says we failed to upload a file
    ///
    /// # Arguments
    ///
    /// * `path` - The path this file was uploaded from
    /// * `err` - The error that stopped the upload
    pub fn error_line(path: &Path, err: &thorium::Error) -> String {
        // get the error message if one was set
        let msg = err.msg().unwrap_or_else(|| "-".to_owned());
        // show either the reqwest body error or the hyper error
        match err.status() {
            // we have a status so well return the code and body as a message
            Some(code) => upload_fmt!(code.as_str().bright_red(), path, "-", "-", msg),
            // no status code is present so just use '-' painted bright red
            None => upload_fmt!("-".bright_red(), path, "-", "-", msg),
        }
    }

    /// Build and print that we failed to upload a file
    ///
    /// # Arguments
    ///
    /// * `path` - The path this file was uploaded from
    /// * `err` - The error that stopped the upload
    pub fn error(path: &Path, err: &thorium::Error) {
        // print the failure on stdout with the other lines
        println!("{}", Self::error_line(path, err));
    }

    /// Build the line for a run that was started on an uploaded file
    ///
    /// # Arguments
    ///
    /// * `path` - The path the file was uploaded from
    /// * `sha256` - The sha256 of the file
    /// * `id` - The id of the run's reaction
    /// * `what` - The kind of run and what ran
    pub fn run_started(path: &Path, sha256: &str, id: &str, what: &str) -> String {
        // a started run has the 200 code and its reaction in the submission column
        upload_fmt!("200".bright_green(), path, sha256, id, what)
    }

    /// Build the line for a run that failed to start on an uploaded file
    ///
    /// # Arguments
    ///
    /// * `path` - The path the file was uploaded from
    /// * `sha256` - The sha256 of the file
    /// * `what` - The kind of run and what ran
    /// * `error` - Why the run did not start
    pub fn run_failed(path: &Path, sha256: &str, what: &str, error: &str) -> String {
        // a run that failed to start has no reaction to show
        upload_fmt!(
            "ERR".bright_red(),
            path,
            sha256,
            "-",
            format!("{what}: {error}")
        )
    }
}

/// How the lines of an upload are printed
#[derive(Debug, Clone, Copy)]
pub struct Printer {
    /// Whether results are JSON lines instead of a table
    pub json: bool,
    /// Whether only failures are printed
    pub quiet: bool,
}

impl Printer {
    /// Build the printer for an upload command
    ///
    /// # Arguments
    ///
    /// * `cmd` - The files upload command
    /// * `args` - The arguments passed to Thorctl
    #[must_use]
    pub fn new(cmd: &UploadFiles, args: &Args) -> Self {
        Printer {
            json: cmd.runs.format.format == Some(OutputFormat::Json),
            quiet: args.quiet,
        }
    }

    /// Print the header of the upload table
    ///
    /// # Arguments
    ///
    /// * `dry_run` - Whether this upload is a dry run
    pub fn header(self, dry_run: bool) {
        // JSON and quiet output have no header
        if self.json || self.quiet {
            return;
        }
        if dry_run {
            UploadLine::header_dry_run();
        } else {
            UploadLine::header();
        }
    }

    /// Print a JSON line for a file
    ///
    /// # Arguments
    ///
    /// * `path` - The path the file was uploaded from
    /// * `status` - What became of the file
    /// * `sha256` - The sha256 of the file, if it is known
    /// * `submission` - The id of the file's submission, if it was made
    /// * `error` - Why the file failed to upload, if it did
    fn json_file(
        path: &Path,
        status: &str,
        sha256: Option<&str>,
        submission: Option<&str>,
        error: Option<&str>,
    ) {
        // one object per file
        println!(
            "{}",
            serde_json::json!({
                "type": "upload",
                "path": path,
                "status": status,
                "sha256": sha256,
                "submission": submission,
                "error": error,
            })
        );
    }

    /// Print that a file was uploaded
    ///
    /// # Arguments
    ///
    /// * `path` - The path the file was uploaded from
    /// * `resp` - The submission response from the API
    pub fn uploaded(self, path: &Path, resp: &SampleSubmissionResponse) {
        if self.json {
            Self::json_file(
                path,
                "uploaded",
                Some(&resp.sha256),
                Some(&resp.id.to_string()),
                None,
            );
        } else if !self.quiet {
            UploadLine::uploaded(path, resp);
        }
    }

    /// Print that a file already exists
    ///
    /// # Arguments
    ///
    /// * `path` - The path the file was uploaded from
    /// * `sha256` - The sha256 of the file
    pub fn conflict(self, path: &Path, sha256: &str) {
        if self.json {
            Self::json_file(path, "exists", Some(sha256), None, None);
        } else if !self.quiet {
            UploadLine::conflict(path, sha256);
        }
    }

    /// Print what a dry run would upload
    ///
    /// # Arguments
    ///
    /// * `path` - The path the file would be uploaded from
    /// * `sha256` - The sha256 of the file
    /// * `tags` - The tags the file would be given
    /// * `runs` - The runs that would start on the file, as text
    pub fn dry_run(
        self,
        path: &Path,
        sha256: &str,
        tags: &HashMap<String, HashSet<String>>,
        runs: &str,
    ) {
        if self.json {
            Self::json_file(path, "dry-run", Some(sha256), None, None);
        } else if !self.quiet {
            UploadLine::uploaded_dry_run(path, sha256, tags, runs);
        }
    }

    /// Print that a file failed to upload
    ///
    /// # Arguments
    ///
    /// * `path` - The path the file was uploaded from
    /// * `err` - The error that stopped the upload
    pub fn error(self, path: &Path, err: &Error) {
        if self.json {
            Self::json_file(path, "failed", None, None, Some(&error_text(err)));
        } else if self.quiet {
            eprintln!("{}", UploadLine::error_line(path, err));
        } else {
            UploadLine::error(path, err);
        }
    }

    /// Print the result of one run started on an uploaded file
    ///
    /// JSON output prints every run once the runs are followed, so only the table prints here.
    ///
    /// # Arguments
    ///
    /// * `path` - The path the file was uploaded from
    /// * `sha256` - The sha256 of the file
    /// * `target` - What was started
    /// * `outcome` - What became of the run
    pub fn run_line(self, path: &Path, sha256: &str, target: &Target, outcome: &Outcome) {
        if self.json {
            return;
        }
        let what = format!("{} {}", target.kind(), target.label());
        match (outcome.id, &outcome.error) {
            // a run that started
            (Some(id), _) => {
                if !self.quiet {
                    println!(
                        "{}",
                        UploadLine::run_started(path, sha256, &id.to_string(), &what)
                    );
                }
            }
            // a run that did not
            (None, error) => eprintln_or_print(
                self.quiet,
                &UploadLine::run_failed(path, sha256, &what, error.as_deref().unwrap_or("unknown")),
            ),
        }
    }

    /// Print a JSON line for a run that was started on an uploaded file
    ///
    /// # Arguments
    ///
    /// * `started` - What the run was
    /// * `outcome` - What became of the run
    pub fn run_json(started: &Started, outcome: &Outcome) {
        // one object per run with the file it is on
        println!(
            "{}",
            serde_json::json!({
                "type": "run",
                "path": started.path,
                "sha256": started.sha256,
                "kind": started.kind,
                "target": started.target,
                "id": outcome.id,
                "group": outcome.group,
                "status": outcome.status,
                "error": outcome.error,
            })
        );
    }
}

/// Print a line to stderr when quiet and to stdout otherwise
///
/// # Arguments
///
/// * `quiet` - Whether output is quiet
/// * `line` - The line to print
fn eprintln_or_print(quiet: bool, line: &str) {
    // quiet output keeps stdout for results and puts failures on stderr
    if quiet {
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
}

/// Get the text of an error for a line of output
///
/// # Arguments
///
/// * `error` - The error to describe
fn error_text(error: &Error) -> String {
    // prefer the API's message and fall back to the whole error
    let msg = error.msg().unwrap_or_else(|| error.to_string());
    // the API answers with a JSON object holding the message
    match serde_json::from_str::<serde_json::Value>(&msg) {
        Ok(serde_json::Value::Object(body)) => match body.get("error") {
            Some(serde_json::Value::String(text)) => text.clone(),
            _ => msg,
        },
        _ => msg,
    }
}

/// Hashes a file and uploads it if it doesn't exist
///
/// A file that is uploaded is handed to the starter so its runs start; a file that already exists
/// is only reported.
///
/// # Arguments
///
///  * `thorium` - A Thorium client
///  * `cmd` - The upload files command to execute
///  * `printer` - How lines are printed
///  * `path` - The file we are uploading
///  * `runs_text` - The runs a dry run says would start on the file
///  * `starter` - Where uploaded files are sent to start their runs
async fn uploader(
    thorium: &Thorium,
    cmd: &UploadFiles,
    printer: Printer,
    path: &Path,
    runs_text: &str,
    starter: Option<&mpsc::Sender<RunFile>>,
) -> Result<(), Error> {
    // get the sha256 for this file
    let sha256 = utils::sha256(path).await?;
    // check if this file has already been uploaded to these groups
    let exists = thorium
        .files
        .exists(&cmd.build_check(path, &sha256))
        .await?;
    // a file that already exists is only reported
    if exists.id.is_some() {
        printer.conflict(path, &sha256);
        return Ok(());
    }
    // Build the sample request for this file
    let sample_req = cmd.build_req(path);
    // a dry run says what would be uploaded and stops
    if cmd.dry_run {
        printer.dry_run(path, &sha256, &sample_req.tags, runs_text);
        return Ok(());
    }
    // upload this file
    match thorium.files.create(sample_req).await {
        Ok(resp) => {
            printer.uploaded(path, &resp);
            // hand the new file to the starter so its runs start
            if let Some(starter) = starter {
                let file = RunFile {
                    path: path.to_path_buf(),
                    sha256,
                };
                // a starter that is gone has nothing left to start
                let _ = starter.send(file).await;
            }
            Ok(())
        }
        // if this file was already uploaded then don't print an error
        Err(err) if err.status() == Some(StatusCode::CONFLICT) => {
            printer.conflict(path, &sha256);
            Ok(())
        }
        Err(err) => Err(err),
    }
}

/// Build base reaction requests from the given pipelines
/// or error if they're invalid, don't exist, or are banned
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The files upload command
pub async fn build_reaction_reqs(
    thorium: &Thorium,
    cmd: &UploadFiles,
) -> Result<Vec<ReactionRequest>, Error> {
    // create list of base reaction requests
    let reaction_reqs = match &cmd.pipelines {
        Some(pipelines) => {
            stream::iter(pipelines.iter())
                .map(|pipeline| async move {
                    // parse the pipeline's group and name with the shared target parser
                    let target = PipelineTarget::parse(pipeline)?;
                    // uploads don't search for a pipeline's group, so one must be given
                    let Some(group) = target.group else {
                        return Err(Error::new(format!(
                            "Pipeline '{pipeline}' has no group; specify it as \
                                 <GROUP>/<PIPELINE> (or <PIPELINE>:<GROUP>)",
                        )));
                    };
                    // make sure the pipeline exists before uploading anything
                    let found = thorium
                        .pipelines
                        .get(&group, &target.pipeline)
                        .await
                        .map_err(|err| {
                            Error::new(format!("Invalid pipeline '{pipeline}': {err}"))
                        })?;
                    // a banned pipeline refuses every reaction and any bulk request holding one
                    if !found.bans.is_empty() {
                        return Err(Error::new(format!(
                            "Pipeline '{pipeline}' is banned; see its notifications for details"
                        )));
                    }
                    // create a base reaction request
                    Ok(ReactionRequest::new(group, target.pipeline))
                })
                .buffer_unordered(25)
                .collect::<Vec<Result<ReactionRequest, Error>>>()
                .await
        }
        None => Vec::default(),
    };
    // recollect the possible Vec<Result> to a Result<Vec> or just convert the default Vec to a Result
    reaction_reqs.into_iter().collect()
}

/// Crawl a directory and upload files
///
/// New files are uploaded up to `--workers` at a time, and the pipelines, workflows, and tools
/// asked for are started on each of them as they arrive.
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The files or directories to crawl and upload
/// * `args` - The arguments passed to Thorctl
async fn upload(thorium: &Thorium, cmd: &UploadFiles, args: &Args) -> Result<(), Error> {
    // the flags every run is followed with
    let common = cmd.runs.common();
    // find every pipeline, workflow, and tool before uploading anything
    let targets = upload_runs::resolve(thorium, cmd, &common).await?;
    // build the set of regexs to determine which files to include or skip
    let filter = RegexSet::new(&cmd.filter)?;
    let skip = RegexSet::new(&cmd.skip)?;
    let printer = Printer::new(cmd, args);
    // name the batch the runs are tagged with when there is one
    let batch = cmd.batch_name();
    // say what a dry run would start on each new file
    let runs_text = targets
        .iter()
        .map(|target| format!("{} {}", target.kind(), target.label()))
        .collect::<Vec<String>>()
        .join(", ");
    // tell the user the batch name before anything starts
    if let (Some(batch), false, false) = (&batch, cmd.dry_run, args.quiet) {
        if printer.json {
            eprintln!("Batch: {batch}");
        } else {
            println!("Batch: {batch}");
        }
    }
    // print the upload logs headers
    printer.header(cmd.dry_run);
    // collect targets to upload from the command
    let paths = cmd.targets.clone().into_targets().await?;
    // count the files and the failures for the exit code
    let seen = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    // a dry run starts nothing, so only a real upload with targets needs a starter
    let mut starter = (!cmd.dry_run && !targets.is_empty()).then(|| {
        Starter::new(
            thorium,
            targets,
            batch,
            BatchOpts::from_args(args),
            common,
            printer,
        )
    });
    let (sender, receiver) = mpsc::channel(starter.as_ref().map_or(1, Starter::files_per_chunk));
    // upload the files, sending each new one on to start its runs
    let has_starter = starter.is_some();
    let walk = async {
        let sender_ref = has_starter.then_some(&sender);
        let (seen, failed, runs_text) = (&seen, &failed, runs_text.as_str());
        utils::fs::process_async_walk(
            paths.into_iter(),
            |target| async move {
                seen.fetch_add(1, Ordering::Relaxed);
                // upload the entry if it's new
                if let Err(err) =
                    uploader(thorium, cmd, printer, &target, runs_text, sender_ref).await
                {
                    failed.fetch_add(1, Ordering::Relaxed);
                    printer.error(&target, &err);
                }
            },
            |path, err| {
                // a path that could not be read is a file that failed
                seen.fetch_add(1, Ordering::Relaxed);
                failed.fetch_add(1, Ordering::Relaxed);
                printer.error(path, err);
            },
            &filter,
            &skip,
            cmd.include_hidden,
            args.workers.max(1),
        )
        .await;
        // no more files will arrive
        drop(sender);
    };
    // start runs while uploads continue
    match starter.as_mut() {
        Some(starter) => {
            tokio::join!(walk, starter.run(receiver));
        }
        None => walk.await,
    }
    let (seen, failed) = (seen.load(Ordering::Relaxed), failed.load(Ordering::Relaxed));
    // follow the runs and report how the upload and its runs ended
    match starter {
        Some(starter) => starter.finish(failed, seen).await,
        None if failed > 0 => Err(Error::new(format!(
            "{failed} of {seen} files failed to upload"
        ))),
        None => Ok(()),
    }
}

/// Download all requested files from Thorium
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The full download command/args
async fn download_targets(
    cmd: &DownloadFiles,
    added: &mut HashSet<String>,
    controller: &mut Controller<FilesDownloadWorker>,
) {
    // add any files from our cmd
    for sha256 in &cmd.sha256s {
        // check if this repo has already been addded to be downloaded
        if added.insert(sha256.clone()) {
            // try to add this download job
            if let Err(error) = controller.add_job(sha256.clone()).await {
                // log this error
                controller.error(&error.to_string());
            }
        }
    }
}

/// Download files from Thorium based on search parameters
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The full download command/args
async fn download_search(
    thorium: &Thorium,
    cmd: &DownloadFiles,
    added: &mut HashSet<String>,
    controller: &mut Controller<FilesDownloadWorker>,
) -> Result<(), Error> {
    // make sure the command is valid
    cmd.validate_search()?;
    // build a search object from our args
    let search = cmd.build_file_opts()?;
    // build a cursor object
    let mut cursor = thorium.files.list(&search).await?;
    // crawl over this cursor until its exhausted
    loop {
        // add each of these sha256s to be downloaded
        for line in cursor.data.drain(..) {
            // check if this repo has already been addded to be downloaded
            if added.insert(line.sha256.clone()) {
                // try to add this download job
                if let Err(error) = controller.add_job(line.sha256).await {
                    // log this error
                    controller.error(&error.to_string());
                }
            }
        }
        // check if this cursor has been exhausted
        if cursor.exhausted() {
            break;
        }
        // get the next page of data
        cursor.refill().await?;
    }
    Ok(())
}

/// Download all requested files from Thorium
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The full download command/args
async fn download(
    thorium: &Thorium,
    cmd: &DownloadFiles,
    args: &Args,
    conf: &CtlConf,
) -> Result<(), Error> {
    // create a new worker controller
    let mut controller = Controller::<FilesDownloadWorker>::spawn(
        "Downloading Files",
        thorium,
        args.workers,
        conf,
        args,
        cmd,
    )
    .await;
    // track and remove any duplicate repos as those can cause hangs
    let mut added = HashSet::with_capacity(100);
    // add the user specified targets to be downloaded
    download_targets(cmd, &mut added, &mut controller).await;
    // if no sha256s were provided then search for samples to download
    if cmd.sha256s.is_empty() {
        // add targets based on searching in Thorium
        if let Err(error) = download_search(thorium, cmd, &mut added, &mut controller).await {
            // log this error
            controller.error(&error.to_string());
        }
    }
    // wait for all our workers to complete
    controller.finish().await?;
    Ok(())
}

/// A single line for an file upload log
struct GetLine;

impl GetLine {
    /// Print this log lines header
    pub fn header() {
        println!(
            "{:<64} | {:<36} | {:<28}",
            "SHA256", "SUBMISSION", "UPLOADED "
        );
        println!("{:-<65}+{:-<38}+{:-<28}", "", "", "");
    }

    /// Print the files get header when tags are used to query
    pub fn header_tags() {
        println!("{:<64} | {:<28}", "SHA256", "UPLOADED ");
        println!("{:-<65}+{:-<28}", "", "");
    }

    /// Build and print a successful file list line
    ///
    /// # Arguments
    ///
    ///* `line` - The sample list line to print
    pub fn list(line: &SampleListLine) {
        // if a submission was set the get it as a string or use "-"
        let submission = line.submission.map_or("-".to_string(), String::from);
        // print an list file line
        println!(
            "{:<64} | {:<36} | {:<28}",
            line.sha256, submission, line.uploaded
        );
    }

    /// Build and print a successful file list line from a query with tags
    ///
    /// # Arguments
    ///
    ///* `line` - The sample list line to print
    pub fn list_tags(line: &SampleListLine) {
        // print an list file line
        println!("{:<64} | {:<28}", line.sha256, line.uploaded);
    }
}

/// Gets information on files
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The full get command/args
async fn get(thorium: &Thorium, cmd: &GetFiles) -> Result<(), Error> {
    // print the header for getting file info, omitting submission if tags are included
    if cmd.tags.is_empty() {
        GetLine::header();
    } else {
        GetLine::header_tags();
    }
    // build a search object from our args
    let opts = cmd.build_file_opts()?;
    // build a cursor object
    let mut cursor = thorium.files.list(&opts).await?;
    // track the submission ids that we find
    let mut ids = HashSet::with_capacity(10000);
    // track the number of pages we have crawled
    let mut pages = 0;
    // crawl over this cursor until its exhausted
    loop {
        // crawl over each submission in this chunk and check if its a dup
        for sub in cursor.data.iter().flat_map(|sample| &sample.submission) {
            if !ids.insert(sub.clone()) {
                panic!("Duplicate on page {pages} - {sub}");
            }
        }
        // crawl the files listed and print info about them
        if cmd.tags.is_empty() {
            cursor.data.iter().for_each(GetLine::list);
        } else {
            cursor.data.iter().for_each(GetLine::list_tags);
        }
        // check if this cursor has been exhausted
        if cursor.exhausted() {
            break;
        }
        // get the next page of data
        cursor.refill().await?;
        pages += 1;
    }
    Ok(())
}

/// Counts files and their tags
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The full count command/args
async fn count(thorium: &Thorium, cmd: &CountFiles) -> Result<(), Error> {
    // build a search object from our args
    let opts = cmd.build_file_opts()?;
    // build a cursor object for counting files and their tags
    let mut cursor = thorium.files.count(&opts).await?;
    // crawl over this cursor until its exhausted
    loop {
        // check if this cursor has been exhausted
        if cursor.exhausted() {
            break;
        }
        // get the next page of data
        cursor.refill().await?;
    }
    // print our count info
    println!("{:#?}", cursor.data);
    Ok(())
}

/// Describe files by displaying/saving their JSON-formatted details
///
/// * `thorium` - The Thorium client
/// * `cmd` - The [`DescribeFiles`] command to execute
async fn describe(thorium: &Thorium, cmd: &DescribeFiles) -> Result<(), Error> {
    cmd.describe(thorium).await
}

/// A single line for an file upload log
struct DeleteLine;

impl DeleteLine {
    /// Print this log lines header
    pub fn header() {
        println!(
            "{:<64} | {:<36} | {:<18} | {:<24}",
            "SHA256", "SUBMISSION", "GROUP", "MESSAGE"
        );
        println!("{:-<65}+{:-<38}+{:-<20}+{:-<26}", "", "", "", "");
    }

    /// Print that we successfully deleted a submission
    ///
    /// # Arguments
    ///
    /// * `sha256` - The sample that was deleted
    /// * `submission` - The submission that was deleted
    pub fn deleted(sha256: &str, submission: &Uuid, groups: &Vec<String>) {
        // print a delete file line
        if groups.is_empty() {
            println!("{:<64} | {:<28} | -{:<18} | -", sha256, submission, "");
        } else {
            for group in groups {
                println!("{sha256:<64} | {submission:<28} | {group:<18} | -");
            }
        }
    }

    /// Log an error from writting a samples details
    pub fn error(sha256: &str, submission: Option<&Uuid>, err: &Error) {
        // print a delete file line
        match submission {
            Some(sub) => println!("{:<64} | {:<28} | {:<18} | {}", sha256, sub, "-", err),
            None => println!("{:<64} | {:<28} | {:<18} | {}", sha256, "-", "-", err),
        }
    }
}

/// The sample SHA256 and list of submissions UUID's designating files for deletion
struct DeleteTarget {
    sha256: String,
    submissions: Vec<Uuid>,
}

/// Parse the sha256 and list of submissions from the raw target string
///
/// # Arguments
///
/// * `target` - The raw target string
fn parse_target(target: &str) -> DeleteTarget {
    // get the submission if its specified or get all of our submissions
    if target.contains(':') {
        // split this into a sha256 and a submission
        let mut split = target.split(':');
        // get our sha256
        let sha256 = split.by_ref().take(1).collect::<String>();
        // get our submissions
        let submissions = split
            .map(Uuid::parse_str)
            .filter_map(|res| {
                match res {
                    Ok(sub) => Some(sub),
                    Err(error) => {
                        // log this error
                        DeleteLine::error(&sha256, None, &Error::from(error));
                        None
                    }
                }
            })
            .collect::<Vec<Uuid>>();
        return DeleteTarget {
            sha256,
            submissions,
        };
    }
    DeleteTarget {
        sha256: target.to_owned(),
        submissions: Vec::new(),
    }
}

/// Delete a sample or its submissions based on a specific target given by the user
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `target` - The sha256 + optional submissions to delete
/// * `users` - The users to delete submissions for (may be empty)
/// * `groups` - The groups to delete from (may be empty)
async fn delete_specific(
    thorium: &Thorium,
    target: DeleteTarget,
    users: &HashSet<String>,
    groups: &Vec<String>,
) {
    // retrieve the file's details
    let details = match thorium.files.get(&target.sha256).await {
        Ok(details) => details,
        Err(error) => {
            // log error and cancel if the file could not be retrieved
            DeleteLine::error(&target.sha256, None, &error);
            return;
        }
    };
    // create a map of submission id's to submissions
    let submission_map: HashMap<Uuid, &SubmissionChunk> = details
        .submissions
        .iter()
        .map(|sub| (sub.id, sub))
        .collect();
    // select either all of the the file's submissions or only the given submissions
    let submissions = if target.submissions.is_empty() {
        submission_map.keys().copied().collect()
    } else {
        target.submissions
    };
    let mut opts = FileDeleteOpts::default();
    // if groups were specified, add them to the our delete opts, otherwise
    // delete submissions from all groups
    if !groups.is_empty() {
        opts = opts.groups(groups.clone());
    }
    // crawl over the submissions we are deleting
    for sub_id in &submissions {
        // attempt to retrieve the submission by its given id
        match submission_map.get(sub_id) {
            Some(sub) => {
                if !users.is_empty() && !users.contains(&sub.submitter) {
                    // error and continue if the submitter is not among the list of users to delete from (if any were given)
                    DeleteLine::error(
                        &target.sha256,
                        Some(sub_id),
                        &Error::new(
                            "The submission was not submitted by any of the given users!"
                                .to_string(),
                        ),
                    );
                    continue;
                }
            }
            None => {
                // error and skip if the submission is not found in the sample
                DeleteLine::error(
                    &target.sha256,
                    Some(sub_id),
                    &Error::new(
                        "The sample does not have a submission with the given id!".to_string(),
                    ),
                );
                continue;
            }
        }
        // delete this submission and log the result
        match thorium.files.delete(&target.sha256, sub_id, &opts).await {
            Ok(_) => DeleteLine::deleted(&target.sha256, sub_id, groups),
            Err(error) => DeleteLine::error(&target.sha256, Some(sub_id), &error),
        }
    }
}

/// Delete samples that we already have details for
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `details` - The sample details to delete
/// * `users` - The users to delete submissions for (may be empty)
/// * `groups` - The groups to delete submissions from (may be empty)
async fn delete_from_details(
    thorium: &Thorium,
    details: &Sample,
    users: &HashSet<String>,
    groups: &Vec<String>,
) {
    let submissions: Vec<&Uuid> = if users.is_empty() {
        // don't filter submissions (delete all submissions) if no users were given
        details.submissions.iter().map(|sub| &sub.id).collect()
    } else {
        // select only submissions submitted by the given list of users
        details
            .submissions
            .iter()
            .filter_map(|sub| users.contains(&sub.submitter).then_some(&sub.id))
            .collect()
    };
    let mut opts = FileDeleteOpts::default();
    // if groups were specified, add them to the our delete opts, otherwise
    // delete all submissions
    if !groups.is_empty() {
        opts = opts.groups(groups.clone());
    }
    for sub in &submissions {
        match thorium.files.delete(&details.sha256, sub, &opts).await {
            Ok(_) => DeleteLine::deleted(&details.sha256, sub, groups),
            Err(error) => DeleteLine::error(&details.sha256, Some(sub), &error),
        }
    }
}

/// Deletes files from Thorium
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The full get command/args
async fn delete(thorium: &Thorium, cmd: &DeleteFiles) -> Result<(), Error> {
    // make sure the command has targets or at least one search parameter
    cmd.validate_search()?;
    // make sure the force flag is set if any search parameters were given
    if cmd.has_parameters() && !cmd.force {
        return Err(Error::new(
            "--force/-f is required when deleting files without \
            explicitly supplying their SHA256's",
        ));
    }
    // print the header for getting file info
    DeleteLine::header();
    // create the set of users to delete from
    let users = if cmd.users.is_empty() {
        if cmd.all_users {
            // provide an empty set to signal to delete from all users
            HashSet::default()
        } else {
            // if no users were given, default to just the current user
            HashSet::from([thorium.users.info().await?.username])
        }
    } else {
        cmd.users.iter().cloned().collect()
    };
    if cmd.has_targets() {
        // delete explicitly specified targets if any were given
        stream::iter(&cmd.targets)
            .for_each_concurrent(10, |raw_target| {
                delete_specific(thorium, parse_target(raw_target), &users, &cmd.groups)
            })
            .await;
    }
    if cmd.has_parameters() {
        // search for samples to delete if any search parameters were given
        let opts = cmd.build_file_opts()?;
        // build a cursor object
        let mut cursor = thorium.files.list_details(&opts).await?;
        // crawl over this cursor until its exhausted
        loop {
            stream::iter(&cursor.data)
                .for_each_concurrent(10, |sample| {
                    delete_from_details(thorium, sample, &users, &cmd.groups)
                })
                .await;
            // check if this cursor has been exhausted
            if cursor.exhausted() {
                break;
            }
            // get the next page of data
            cursor.refill().await?;
        }
    }
    Ok(())
}

/// Handle all files commands or print files docs
///
/// # Arguments
///
/// * `args` - The arguments passed to Thorctl
/// * `cmd` - The files command to execute
pub async fn handle(args: &Args, cmd: &Files) -> Result<(), Error> {
    // load our config and instance our client
    let (conf, thorium) = utils::get_client(args).await?;
    // warn about insecure connections if not set to skip
    if !conf.skip_insecure_warning.unwrap_or_default() {
        utils::warn_insecure_conf(&conf)?;
    }
    // check if we need to update
    if !args.skip_update && !conf.skip_update.unwrap_or_default() {
        update::ask_update(&thorium).await?;
    }
    // call the right files handler
    match cmd {
        Files::Upload(cmd) => upload(&thorium, cmd, args).await,
        Files::Download(cmd) => download(&thorium, cmd, args, &conf).await,
        Files::Get(cmd) => get(&thorium, cmd).await,
        Files::Count(cmd) => count(&thorium, cmd).await,
        Files::Describe(cmd) => describe(&thorium, cmd).await,
        Files::Delete(cmd) => delete(&thorium, cmd).await,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use thorium::Error;

    use super::{UploadLine, error_text};

    /// The API's JSON error body is shown as its message and anything else as is
    #[test]
    fn error_text_unwraps_the_api_message() {
        // a JSON error object shows its message
        let body = Error::new(r#"{"error":"Unable to create reaction(s)! banned"}"#);
        assert_eq!(error_text(&body), "Unable to create reaction(s)! banned");
        // other JSON and plain text are left alone
        let other = Error::new(r#"{"detail":"x"}"#);
        assert_eq!(error_text(&other), r#"{"detail":"x"}"#);
        assert_eq!(error_text(&Error::new("boom")), "boom");
    }

    /// A started run fills the submission column with its reaction and a failed one says why
    #[test]
    fn run_lines_use_the_upload_table_columns() {
        let sha = "a".repeat(64);
        let id = "6a3e2f9c-1b7d-4a58-9c0e-3d5f7a1b2c4d";
        // a started run
        let ok = UploadLine::run_started(Path::new("f.bin"), &sha, id, "workflow triage");
        let cells: Vec<&str> = ok.split('|').map(str::trim).collect();
        assert!(cells[0].ends_with("200"), "{ok}");
        assert_eq!(&cells[1..], ["f.bin", sha.as_str(), id, "workflow triage"]);
        // a run that did not start has no reaction and the error in the message
        let bad = UploadLine::run_failed(Path::new("f.bin"), &sha, "tool scan", "no such tool");
        let cells: Vec<&str> = bad.split('|').map(str::trim).collect();
        assert!(cells[0].ends_with("ERR"), "{bad}");
        assert_eq!(cells[3], "-");
        assert_eq!(cells[4], "tool scan: no such tool");
    }
}
