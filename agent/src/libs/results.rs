//! Handles collecting results for the agent and sending them back to the API

use async_walkdir::WalkDir;
use crossbeam::channel::Sender;
use futures::StreamExt;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use thorium::client::ResultsClient;
use thorium::models::{
    Buffer, EntityKinds, EntityRequest, GenericJob, HarnessOutputs, Image, OnDiskFile,
    OutputDisplayType, OutputRequest, ReactionSource, Repo, Sample, ToolVersion,
};
use thorium::{Error, Thorium};
use tracing::instrument;
use uuid::Uuid;

use super::Workload;
use crate::log;

/// The directory a harness writes its conversation and traces to
const HARNESS_RESULT_FILES: &str = "/tmp/thorium/result-files";

/// The tool name and version a job's results are stored under
#[derive(Debug, Clone, PartialEq)]
pub struct ResultTool {
    /// The tool name the results are stored under
    pub name: String,
    /// The tool version the results are stored with (unset for a workflow run)
    pub version: Option<ToolVersion>,
}

impl ResultTool {
    /// Pick the tool name and version a job's results are stored under
    ///
    /// A workflow run's results are stored under its result key, and every other job's under
    /// the name and version of the tool instance it ran.
    ///
    /// # Arguments
    ///
    /// * `workload` - The tool instance the job ran
    /// * `source` - What the job's reaction runs, if it was read
    #[must_use]
    pub fn new(workload: &Workload, source: Option<&ReactionSource>) -> Self {
        // workflow runs store results under their result key
        match source {
            Some(ReactionSource::WorkflowRun(run)) => ResultTool {
                name: run.result_key.clone(),
                version: None,
            },
            _ => ResultTool {
                name: workload.tool.clone(),
                version: Some(workload.version.clone()),
            },
        }
    }

    /// Get the tool name and version a job's results are stored under
    ///
    /// Only a harness runs workflow runs, so the job's reaction is read only for a harness.
    ///
    /// # Arguments
    ///
    /// * `thorium` - The client to read the job's reaction with
    /// * `workload` - The tool instance the job ran
    /// * `job` - The job whose results are being stored
    #[instrument(name = "ResultTool::for_job", skip_all, err(Debug))]
    pub async fn for_job(
        thorium: &Thorium,
        workload: &Workload,
        job: &GenericJob,
    ) -> Result<Self, Error> {
        // jobs of tools that aren't harnesses are never workflow runs
        if workload.harness.is_none() {
            return Ok(Self::new(workload, None));
        }
        // read the reaction to learn whether this job is a workflow run
        let reaction = thorium.reactions.get(&job.group, job.reaction).await?;
        Ok(Self::new(workload, Some(&reaction.source)))
    }
}

/// Get the files a harness writes that are always uploaded as result files
///
/// # Arguments
///
/// * `outputs` - Where the harness writes its outputs
#[must_use]
pub fn harness_files(outputs: &HarnessOutputs) -> Vec<PathBuf> {
    // the conversation and traces go to fixed paths and the status to the harness's own path
    let dir = Path::new(HARNESS_RESULT_FILES);
    vec![
        dir.join("conversation.jsonl"),
        dir.join("traces.jsonl"),
        PathBuf::from(&outputs.status),
    ]
}

/// Whether this result should be uploaded to the db or to s3
#[derive(Debug, Clone)]
pub enum ResultTarget {
    /// This result should go to the db
    Db(String),
    /// The result should go to  s3
    S3 { results: String, len: u64 },
}

impl ResultTarget {
    /// Build the correct result for an output request
    pub fn to_output_request(&self, logs: &mut Sender<String>) -> Result<String, Error> {
        // if  this is going to s3 then add it to our files and set a warning
        match self {
            ResultTarget::Db(result) => Ok(result.clone()),
            ResultTarget::S3 { len, .. } => {
                // log that our results file is over 1 MB
                log!(logs, "Warning: Results file exists but is {}B", len);
                // create an output with the warning that the result was too large to display
                let mut output = HashMap::with_capacity(1);
                output.insert(
                    "Warnings",
                    vec!["result stored as result file since it was bigger then 1 MB"],
                );
                // serialize our results
                let new_results = serde_json::to_string(&output)?;
                Ok(new_results)
            }
        }
    }

    pub fn get_results(&self) -> &String {
        match self {
            ResultTarget::Db(results) => results,
            ResultTarget::S3 { results, .. } => results,
        }
    }
}

/// A raw output request that later gets duplicated for each possible
/// input (samples, repos ...)
#[allow(clippy::module_name_repetitions)]
pub struct RawResults {
    /// Whether this results should be scanned for tags or not
    pub scan: bool,
    /// The serialized output for this result
    pub results: ResultTarget,
    /// Any files tied to this result
    pub files: Vec<OnDiskFile>,
    /// What entities were found in this result but not neccesarily created in Thorium
    pub entities: HashMap<EntityKinds, (usize, Buffer)>,
    /// The display type of this result
    pub display_type: OutputDisplayType,
}

impl RawResults {
    /// Checks the filesystem for results to send to Thorium
    ///
    /// # Arguments
    ///
    /// * `image` - The image we are collecting results in
    /// * `path` - The path to collect results at
    /// * `logs` - The logs to send to the API
    #[instrument(name = "RawResults::new", skip_all, fields(path = path.to_string_lossy().into_owned()), err(Debug))]
    async fn new(
        image: &Image,
        path: &Path,
        logs: &mut Sender<String>,
    ) -> Result<RawResults, Error> {
        // check to see if this path exists
        if path.exists() {
            // check the size of this file to determine if it should be a result file
            let metadata = path.metadata()?;
            // only try to ingest results if this is a file
            if metadata.is_file() {
                // check if our results file length is too large or empty
                let raw_result = match metadata.len() {
                    // results is empty so don't bother uploading it
                    0 => {
                        if image.display_type.requires_results() {
                            // log that our results file is empty
                            log!(logs, "Warning: Results file exists but is empty");
                            // create an output with the warning that the result was empty
                            let mut output = HashMap::with_capacity(1);
                            output.insert("Warnings", vec!["Results file exists but is empty"]);
                            // serialize our results
                            let results = serde_json::to_string(&output)?;
                            // build our raw results
                            RawResults {
                                scan: false,
                                results: ResultTarget::Db(results),
                                files: Vec::default(),
                                entities: HashMap::default(),
                                display_type: OutputDisplayType::Json,
                            }
                        } else {
                            // build our raw results with our empty but not required results
                            RawResults {
                                scan: image.display_type == OutputDisplayType::Json,
                                results: ResultTarget::Db("".to_string()),
                                files: Vec::default(),
                                entities: HashMap::default(),
                                display_type: image.display_type,
                            }
                        }
                    }
                    // results is too large to be stored in the DB
                    len if len > 1_000_000 => {
                        // this result will be streamed to s3 as a result file, so the only
                        // thing that reads the in memory copy is auto tag extraction. Only
                        // buffer the whole (potentially very large) file when auto tagging
                        // is actually configured, otherwise we would read hundreds of MiB
                        // into memory just to discard it
                        let results = if image.output_collection.auto_tag.is_empty() {
                            String::new()
                        } else {
                            // read in our results, decoding lossily so a non utf8 result
                            // file doesn't fail the entire submission
                            let bytes = tokio::fs::read(path).await?;
                            String::from_utf8_lossy(&bytes).into_owned()
                        };
                        // build our result file to store
                        let file = OnDiskFile::new(path)
                            .trim_prefix(path.parent().unwrap_or_else(|| Path::new("/")));
                        // build our raw results
                        RawResults {
                            scan: true,
                            results: ResultTarget::S3 { results, len },
                            files: vec![file],
                            entities: HashMap::default(),
                            display_type: OutputDisplayType::Json,
                        }
                    }
                    // the result is the correct size to be stored in the DB
                    _ => {
                        // read in our results
                        let results = tokio::fs::read_to_string(path).await?;
                        // build our raw results
                        RawResults {
                            scan: image.display_type == OutputDisplayType::Json,
                            results: ResultTarget::Db(results),
                            files: Vec::default(),
                            entities: HashMap::default(),
                            display_type: image.display_type,
                        }
                    }
                };
                Ok(raw_result)
            } else {
                // log that this result file is not a file
                log!(logs, "Warning: Results file is not a file");
                // create an output with the warning that the result was not a file
                let mut output = HashMap::with_capacity(1);
                output.insert("Warnings", vec!["Results file is not a file"]);
                // serialize our results
                let results = serde_json::to_string(&output)?;
                // build our raw results
                let raw_result = RawResults {
                    scan: false,
                    results: ResultTarget::Db(results),
                    files: Vec::default(),
                    entities: HashMap::default(),
                    display_type: OutputDisplayType::Json,
                };
                Ok(raw_result)
            }
        } else {
            // there is no file at the result path
            if image.display_type.requires_results() {
                // log that no results file was found
                log!(logs, "Warning: No results file found");
                // create an output with the warning that the result was not found
                let mut output = HashMap::with_capacity(1);
                output.insert("Warnings", vec!["No non file results found"]);
                // serialize our results
                let results = serde_json::to_string(&output)?;
                // build our raw results
                let raw_result = RawResults {
                    scan: false,
                    results: ResultTarget::Db(results),
                    files: Vec::default(),
                    entities: HashMap::default(),
                    display_type: OutputDisplayType::Json,
                };
                Ok(raw_result)
            } else {
                // build our raw results with our empty but not required results
                let raw_results = RawResults {
                    scan: image.display_type == OutputDisplayType::Json,
                    results: ResultTarget::Db("".to_string()),
                    files: Vec::default(),
                    entities: HashMap::default(),
                    display_type: image.display_type,
                };
                Ok(raw_results)
            }
        }
    }

    /// Collect all discovered entities
    ///
    /// # Arguments
    ///
    /// * `image` - The image to collect entities for
    async fn collect_entities(&mut self, image: &Image) -> Result<(), Error> {
        // build a map of our different entity request kinds
        let mut kind_map = HashMap::<EntityKinds, Vec<EntityRequest>>::default();
        // check if this file exists
        if tokio::fs::try_exists(&image.output_collection.files.entities).await? {
            // read the raw bytes for our entities from disk
            let data = tokio::fs::read(&image.output_collection.files.entities).await?;
            // deserialize our entities from disk
            let parsed: Vec<EntityRequest> = serde_json::from_slice(&data)?;
            // break our list up based on what kind of entity this is
            for req in parsed {
                // get what kind of entity this is
                let kind = req.kind();
                // get an entry to our kinds entity list
                let entry = kind_map.entry(kind).or_default();
                // add this request to its kinds
                entry.push(req);
            }
            // add each of our entity kinds to our raw results
            for (kind, reqs) in kind_map {
                // serialize this kinds requests
                let serialized = serde_json::to_string(&reqs)?;
                // wrap our serialized data in a buffer
                let buff = Buffer::new(serialized);
                // add our serialized entity kinds to our results
                self.entities.insert(kind, (reqs.len(), buff));
            }
        }
        Ok(())
    }

    /// Checks the filesystem for result files to send to Thorium
    ///
    /// This looks for result files not results to store in s3.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to collect result files from
    /// * `logs` - The logs to send to the API
    #[instrument(name = "RawResults::collect_result_files", skip_all, fields(path = path.to_string_lossy().into_owned()), err(Debug))]
    async fn collect_result_files(
        &mut self,
        path: &Path,
        logs: &mut Sender<String>,
    ) -> Result<(), Error> {
        // check to see if this path exists
        if path.exists() {
            // check the size of this file to determine if it should be a result file
            let metadata = path.metadata()?;
            // only try to ingest results files if this path is a directory
            if metadata.is_dir() {
                // walk over entries in this path
                let mut walker = WalkDir::new(path);
                // start walking over entries in this dir
                while let Some(entry_result) = walker.next().await {
                    // check if we failed to get info on this entry
                    let entry = entry_result?;
                    // get this entry's metadata
                    let meta = entry.metadata().await?;
                    // check if this is a file or not
                    if meta.is_file() {
                        // build an on disk file for this file
                        let on_disk = OnDiskFile::new(entry.path()).trim_prefix(path);
                        // log that we found this file
                        log!(logs, "Found result file {}", on_disk.path.display());
                        // add this file to our list fo result files
                        self.files.push(on_disk);
                    }
                }
            } else {
                // log that this result file is not a file
                log!(logs, "Warning: Results files path is not a directory");
            }
        }
        Ok(())
    }

    /// Add files that exist to the result files if they aren't already in them
    ///
    /// Each file is named by its file name.
    ///
    /// # Arguments
    ///
    /// * `paths` - The paths of the files to add
    /// * `logs` - The logs to send to the API
    pub async fn add_files(
        &mut self,
        paths: &[PathBuf],
        logs: &mut Sender<String>,
    ) -> Result<(), Error> {
        // add each file that exists and isn't already a result file
        for path in paths {
            // skip files the harness didn't write
            if !tokio::fs::try_exists(path).await? || !path.is_file() {
                continue;
            }
            // skip files the result files directory already holds
            if self.files.iter().any(|file| &file.path == path) {
                continue;
            }
            // name this file by its file name
            let parent = path.parent().unwrap_or_else(|| Path::new("/"));
            let on_disk = OnDiskFile::new(path).trim_prefix(parent);
            // log that we found this file
            log!(logs, "Found result file {}", on_disk.path.display());
            self.files.push(on_disk);
        }
        Ok(())
    }

    /// Create a sample output request for these raw results
    ///
    /// # Arguments
    ///
    /// * `sha256` - The sha256 of the sample we are uploading results for
    /// * `tool` - The tool name and version to store these results under
    /// * `logs` - The logs to send to the API
    pub fn to_sample_req(
        &self,
        sha256: &str,
        tool: &ResultTool,
        logs: &mut Sender<String>,
    ) -> Result<OutputRequest<Sample>, Error> {
        // convert our results
        let results = self.results.to_output_request(logs)?;
        // build our output request
        let mut req = OutputRequest::<Sample>::new(
            sha256.to_owned(),
            tool.name.clone(),
            results,
            self.display_type,
        )
        .files(self.files.clone());
        // store the tool version these results came from
        req.tool_version.clone_from(&tool.version);
        // add our any entities from this job
        req.entities = self.entities.clone();
        Ok(req)
    }

    /// Create a repo output request for these raw results
    ///
    /// # Arguments
    ///
    /// * `repo` - The url of the repo we are uploading results for
    /// * `tool` - The tool name and version to store these results under
    /// * `logs` - The logs to send to the API
    pub fn to_repo_req(
        &self,
        repo: &str,
        tool: &ResultTool,
        logs: &mut Sender<String>,
    ) -> Result<OutputRequest<Repo>, Error> {
        // convert our results
        let results = self.results.to_output_request(logs)?;
        // build our output request
        let mut req = OutputRequest::<Repo>::new(
            repo.to_owned(),
            tool.name.clone(),
            results,
            self.display_type,
        )
        .files(self.files.clone());
        // store the tool version these results came from
        req.tool_version.clone_from(&tool.version);
        // add our any entities from this job
        req.entities = self.entities.clone();
        Ok(req)
    }
}

/// Collects any results from executing a job
///
/// # Arguments
///
/// * `job` - The job we are collecting results from
/// * `image` - The image to collect result and result files in
/// * `results` - The path to look for results at
/// * `results_files` - The path to look for result files at
/// * `logs` - The logs to send to the API
#[instrument(
    name = "results::collect",
    skip_all,
    fields(
        results = results.as_ref().to_string_lossy().into_owned(),
        result_files = result_files.as_ref().to_string_lossy().into_owned()
    ),
    err(Debug))]
pub async fn collect<P: AsRef<Path>>(
    image: &Image,
    results: P,
    result_files: P,
    logs: &mut Sender<String>,
) -> Result<RawResults, Error> {
    // call the correct output collector
    let mut outputs = RawResults::new(image, results.as_ref(), logs).await?;
    // collect any entities from this job
    outputs.collect_entities(image).await?;
    // we have results so collect any result files
    outputs
        .collect_result_files(result_files.as_ref(), logs)
        .await?;
    Ok(outputs)
}

/// Send any collected results to Thorium
///
/// Every input sample and repo of the job gets the same results.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `raw` - The results to submit to Thorium
/// * `job` - The job the results came from
/// * `tool` - The tool name and version to store the results under
/// * `logs` - The logs to send to the API
#[instrument(name = "results::submit", skip_all, err(Debug))]
pub async fn submit(
    thorium: &Thorium,
    raw: &RawResults,
    job: &GenericJob,
    tool: &ResultTool,
    logs: &mut Sender<String>,
) -> Result<Vec<Uuid>, Error> {
    // track the results we create
    let mut ids = Vec::with_capacity(job.samples.len() + job.repos.len());
    // send our results for samples
    for sha256 in &job.samples {
        // build an output request for this samples
        let req = raw.to_sample_req(sha256, tool, logs)?;
        // send this request to the API
        let id = thorium.files.create_result(req).await?;
        // add this new result id to our list
        ids.push(id.id);
    }
    // send our results for repos
    for repo in &job.repos {
        // build an output request for this repos
        let req = raw.to_repo_req(&repo.url, tool, logs)?;
        // send this request to the API
        let id = thorium.repos.create_result(req).await?;
        // add this new result id to our list
        ids.push(id.id);
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use thorium::Thorium;
    use thorium::models::{
        CommitishKinds, GenericJob, GenericJobArgs, HarnessOutputs, JobStatus, OutputDisplayType,
        Reaction, ReactionPlan, ReactionSource, ReactionStatus, ReactionWorkflowRun,
        RepoDependency, ToolVersion,
    };
    use uuid::Uuid;

    use super::{RawResults, ResultTarget, ResultTool, harness_files};
    use crate::libs::test_server::{self, MockThorium};

    /// Build a job with two input samples and one input repo
    fn job() -> GenericJob {
        // a job of the harvest pipeline's harvester stage
        GenericJob {
            reaction: Uuid::new_v4(),
            id: Uuid::new_v4(),
            group: "corn".into(),
            pipeline: "harvest".into(),
            stage: "harvester".into(),
            creator: "mcarson".into(),
            args: GenericJobArgs::default(),
            status: JobStatus::Running,
            deadline: chrono::Utc::now(),
            parent: None,
            generator: false,
            samples: vec!["sample1".into(), "sample2".into()],
            ephemeral: Vec::new(),
            parent_ephemeral: HashMap::default(),
            repos: vec![RepoDependency {
                url: "github.com/curl/curl".into(),
                commitish: Some("main".into()),
                kind: Some(CommitishKinds::Branch),
            }],
            trigger_depth: None,
            tool_instance: Uuid::new_v4(),
            run: None,
        }
    }

    /// Build a workflow run source with a result key
    ///
    /// # Arguments
    ///
    /// * `result_key` - The result key of the run
    fn workflow_run(result_key: &str) -> ReactionSource {
        // a workflow run with no resolved models
        ReactionSource::WorkflowRun(ReactionWorkflowRun {
            workflow_instance: Uuid::new_v4(),
            digest: "sha256:abc".to_owned(),
            agent: Uuid::new_v4(),
            harness: Uuid::new_v4(),
            result_key: result_key.to_owned(),
            models: Vec::new(),
            provenance: thorium::models::RunProvenance::default(),
        })
    }

    /// Build a reaction for a job
    ///
    /// # Arguments
    ///
    /// * `job` - The job the reaction owns
    /// * `source` - What the reaction runs
    fn reaction(job: &GenericJob, source: ReactionSource) -> Reaction {
        // a started reaction with one job
        Reaction {
            id: job.reaction,
            group: job.group.clone(),
            creator: job.creator.clone(),
            pipeline: job.pipeline.clone(),
            status: ReactionStatus::Started,
            current_stage: 0,
            current_stage_progress: 0,
            current_stage_length: 1,
            args: HashMap::default(),
            sla: chrono::Utc::now(),
            jobs: vec![job.id],
            tags: Vec::new(),
            parent: None,
            sub_reactions: 0,
            completed_sub_reactions: 0,
            generators: Vec::new(),
            samples: job.samples.clone(),
            ephemeral: Vec::new(),
            parent_ephemeral: HashMap::default(),
            repos: job.repos.clone(),
            trigger_depth: None,
            has_cache: false,
            source,
            plan: ReactionPlan::default(),
            run_token: None,
            run: None,
        }
    }

    /// Build raw results holding a small json result
    fn raw() -> RawResults {
        // a result stored in the database with no files
        RawResults {
            scan: false,
            results: ResultTarget::Db(r#"{"ok":true}"#.to_owned()),
            files: Vec::new(),
            entities: HashMap::default(),
            display_type: OutputDisplayType::Json,
        }
    }

    /// Get the value of a multipart form field in a request
    ///
    /// # Arguments
    ///
    /// * `request` - The whole request
    /// * `name` - The name of the field
    fn field(request: &str, name: &str) -> Option<String> {
        // find the field's header and take the line after the blank line that ends it
        let header = format!("name=\"{name}\"");
        let start = request.find(&header)?;
        let rest = &request[start..];
        let body = rest.split("\r\n\r\n").nth(1)?;
        Some(body.split("\r\n").next()?.to_owned())
    }

    /// Tool jobs store results under the tool's name and version and workflow runs under their
    /// result key
    #[test]
    fn result_tool_names() {
        // a tool job, with or without its pipeline source, uses the tool's name and version
        let workload = test_server::workload(None);
        let expected = ResultTool {
            name: "harvester".to_owned(),
            version: Some(ToolVersion::from("1.2.0")),
        };
        assert_eq!(ResultTool::new(&workload, None), expected);
        let pipeline = ReactionSource::Pipeline {
            name: "harvest".to_owned(),
            provenance: thorium::models::RunProvenance::default(),
        };
        assert_eq!(ResultTool::new(&workload, Some(&pipeline)), expected);
        // a workflow run uses its result key and no version
        let run = workflow_run("triage#r1");
        let tool = ResultTool::new(&workload, Some(&run));
        assert_eq!(tool.name, "triage#r1");
        assert_eq!(tool.version, None);
    }

    /// Only a harness's jobs read their reaction, and a workflow run's results use its key
    #[tokio::test]
    async fn result_tool_reads_the_reaction_only_for_harnesses() {
        // a mock API serving a workflow run reaction for the job
        let job = job();
        let body = serde_json::to_string(&reaction(&job, workflow_run("triage#r1"))).unwrap();
        let path = format!("GET /api/reactions/corn/{} ", job.reaction);
        let mock = MockThorium::start(move |line| {
            if line.starts_with(&path) {
                (200, body.clone())
            } else {
                (500, "{}".to_owned())
            }
        })
        .await;
        let thorium = Thorium::build(&mock.host)
            .token("token")
            .build()
            .await
            .unwrap();
        // an Image tool's job reads nothing and uses the tool's name
        let image = test_server::workload(None);
        let tool = ResultTool::for_job(&thorium, &image, &job).await.unwrap();
        assert_eq!(tool.name, "harvester");
        assert!(mock.lines().is_empty(), "{:?}", mock.lines());
        // a harness's job reads its reaction and uses the run's result key
        let harness = test_server::workload(Some(HarnessOutputs::default()));
        let tool = ResultTool::for_job(&thorium, &harness, &job).await.unwrap();
        assert_eq!(tool.name, "triage#r1");
        assert_eq!(tool.version, None);
        assert_eq!(mock.lines().len(), 1, "{:?}", mock.lines());
    }

    /// Results are stored under the chosen tool on every input sample and repo
    #[tokio::test]
    async fn results_stored_on_every_input() {
        // a mock API that creates results
        let mock = MockThorium::start(|line| {
            if line.starts_with("POST ") && line.contains("/results/") {
                (200, format!(r#"{{"id":"{}"}}"#, Uuid::new_v4()))
            } else {
                (500, "{}".to_owned())
            }
        })
        .await;
        let thorium = Thorium::build(&mock.host)
            .token("token")
            .build()
            .await
            .unwrap();
        let job = job();
        let (mut logs, _receiver) = crossbeam::channel::unbounded();
        // a tool job stores its results under the tool's name and version
        let tool = ResultTool::new(&test_server::workload(None), None);
        let ids = super::submit(&thorium, &raw(), &job, &tool, &mut logs)
            .await
            .unwrap();
        assert_eq!(ids.len(), 3);
        let requests = mock.matching("/results/");
        assert_eq!(requests.len(), 3, "{requests:?}");
        for request in &requests {
            assert_eq!(
                field(request, "tool").as_deref(),
                Some("harvester"),
                "{request}"
            );
            assert_eq!(
                field(request, "tool_version").as_deref(),
                Some("1.2.0"),
                "{request}"
            );
        }
        // two of the results are on the samples and one is on the repo
        assert_eq!(
            requests
                .iter()
                .filter(|req| req.starts_with("POST /api/files/"))
                .count(),
            2
        );
        assert_eq!(
            requests
                .iter()
                .filter(|req| req.starts_with("POST /api/repos/"))
                .count(),
            1
        );
        // a workflow run stores its results under its result key with no version
        let run = workflow_run("triage#r1");
        let tool = ResultTool::new(
            &test_server::workload(Some(HarnessOutputs::default())),
            Some(&run),
        );
        super::submit(&thorium, &raw(), &job, &tool, &mut logs)
            .await
            .unwrap();
        let requests = mock.matching("/results/");
        assert_eq!(requests.len(), 6, "{requests:?}");
        for request in &requests[3..] {
            assert_eq!(
                field(request, "tool").as_deref(),
                Some("triage#r1"),
                "{request}"
            );
            assert_eq!(field(request, "tool_version"), None, "{request}");
        }
    }

    /// A harness's conversation, traces, and status are result files even when the result files
    /// directory doesn't hold them, and no file is added twice
    #[tokio::test]
    async fn harness_files_are_always_result_files() {
        // the conversation and traces have fixed paths and the status has the harness's own path
        let outputs = HarnessOutputs {
            status: "/tmp/harness/status.json".to_owned(),
            ..HarnessOutputs::default()
        };
        assert_eq!(
            harness_files(&outputs),
            vec![
                PathBuf::from("/tmp/thorium/result-files/conversation.jsonl"),
                PathBuf::from("/tmp/thorium/result-files/traces.jsonl"),
                PathBuf::from("/tmp/harness/status.json"),
            ]
        );
        // a harness that wrote its files outside the collected result files directory
        let dir = std::env::temp_dir().join(format!("thorium-harness-{}", Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let written =
            ["conversation.jsonl", "traces.jsonl", "status.json"].map(|name| dir.join(name));
        for path in &written {
            tokio::fs::write(path, "{}\n").await.unwrap();
        }
        let missing = dir.join("missing.json");
        // the status file was already collected from the result files directory
        let mut raw = raw();
        raw.files
            .push(thorium::models::OnDiskFile::new(&written[2]).trim_prefix(&dir));
        let (mut logs, _receiver) = crossbeam::channel::unbounded();
        let mut paths = written.to_vec();
        paths.push(missing);
        raw.add_files(&paths, &mut logs).await.unwrap();
        tokio::fs::remove_dir_all(&dir).await.unwrap();
        // each written file is a result file once, named by its file name
        let found: Vec<&PathBuf> = raw.files.iter().map(|file| &file.path).collect();
        assert_eq!(found, vec![&written[2], &written[0], &written[1]]);
        assert!(
            raw.files
                .iter()
                .all(|file| file.trim_prefix.as_deref() == Some(dir.as_path()))
        );
    }
}
