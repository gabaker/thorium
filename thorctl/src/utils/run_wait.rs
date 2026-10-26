//! Following a started run: waiting for it to finish, downloading its results, and printing its
//! logs
//!
//! Every command that starts a run (`tools run`, `workflows run`, `pipelines run`) hands the new
//! run's reaction to [`follow`], so each of them waits, downloads, and exits the same way.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use thorium::client::ResultsClient;
use thorium::models::{Output, Reaction, ReactionStatus, ResultGetParams};
use thorium::{Error, Thorium};
use uuid::Uuid;

use super::output::{self, OutputFormat};
use super::watch;
use crate::args::runs::RunCommonArgs;
use crate::handlers::reactions;
use crate::handlers::runs::RunView;
use crate::utils;

/// How waiting for a run ended
#[derive(Debug)]
pub enum RunEnd {
    /// The run completed
    Completed(Reaction),
    /// The run failed
    Failed(Reaction),
    /// The run had not finished when the timeout passed
    TimedOut(Reaction),
}

/// Whether a run's status is final
///
/// # Arguments
///
/// * `status` - The status to check
#[must_use]
pub fn is_finished(status: &ReactionStatus) -> bool {
    // a run ends by completing or failing
    matches!(status, ReactionStatus::Completed | ReactionStatus::Failed)
}

/// Split a list file into its entries
///
/// Blank lines and lines starting with `#` are skipped and each entry is trimmed.
///
/// # Arguments
///
/// * `text` - The text of the list file
/// * `split_commas` - Whether a line may hold several entries separated by commas
#[must_use]
pub fn list_entries(text: &str, split_commas: bool) -> Vec<String> {
    // keep the lines that hold an entry
    let lines = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'));
    // split a bundle into its entries when asked to
    if split_commas {
        lines
            .flat_map(|line| line.split(','))
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    } else {
        lines.map(ToOwned::to_owned).collect()
    }
}

/// Join a result file name onto a directory, refusing names that would leave it
///
/// # Arguments
///
/// * `dir` - The directory to write into
/// * `name` - The result file's name
pub fn safe_join(dir: &Path, name: &str) -> Result<PathBuf, Error> {
    // every part of the name must be a plain file or directory name
    let path = Path::new(name);
    let plain = path.components().count() > 0
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        && !name.contains('\\');
    if !plain {
        return Err(Error::new(format!(
            "result file '{name}' would be written outside of {}",
            dir.display()
        )));
    }
    Ok(dir.join(path))
}

/// Poll a run until it finishes or the timeout passes
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `group` - The group the run is in
/// * `id` - The run's reaction id
/// * `timeout` - How long to wait for the run to finish
/// * `interval` - How long to wait between reads
pub async fn wait_finished(
    thorium: &Thorium,
    group: &str,
    id: Uuid,
    timeout: Duration,
    interval: Duration,
) -> Result<RunEnd, Error> {
    // read the run until it is final or the timeout passes
    let found = watch::poll_until(
        || thorium.reactions.get(group, id),
        |reaction| Ok(is_finished(&reaction.status)),
        Some(timeout),
        interval,
    )
    .await?;
    // name how the run ended, reading it once more to report its status when it timed out
    let reaction = match found {
        Some(reaction) => reaction,
        None => thorium.reactions.get(group, id).await?,
    };
    Ok(match reaction.status {
        ReactionStatus::Completed => RunEnd::Completed(reaction),
        ReactionStatus::Failed => RunEnd::Failed(reaction),
        ReactionStatus::Created | ReactionStatus::Started => RunEnd::TimedOut(reaction),
    })
}

/// A file or repo a run ran on
enum Input<'a> {
    /// A file, by SHA256
    Sample(&'a str),
    /// A repo, by url
    Repo(&'a str),
}

impl Input<'_> {
    /// Get the key the input's results are stored under
    fn key(&self) -> &str {
        // a file's key is its SHA256 and a repo's its url
        match self {
            Input::Sample(key) | Input::Repo(key) => key,
        }
    }
}

/// Write one result and its files under a directory
///
/// The result is written as JSON named after its tool; a result with files gets a directory named
/// after its tool that holds them and the JSON.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `input` - The file or repo the result is of
/// * `tool` - The tool that made the result
/// * `output` - The result
/// * `dir` - The directory to write under
async fn write_result(
    thorium: &Thorium,
    input: &Input<'_>,
    tool: &str,
    output: &Output,
    dir: &Path,
) -> Result<Vec<PathBuf>, Error> {
    // name the tool's output in a form valid on every platform
    let tool_path = utils::fs::path_component(tool);
    let mut target = dir.join(&tool_path);
    let mut written = Vec::new();
    // write the result's files into the tool's directory, or just make the base directory
    if output.files.is_empty() {
        tokio::fs::create_dir_all(dir).await?;
    } else {
        tokio::fs::create_dir_all(&target).await?;
        for name in &output.files {
            // check where the file lands before downloading it
            let file_target = safe_join(&target, name)?;
            let attachment = match input {
                Input::Sample(sha256) => {
                    thorium
                        .files
                        .download_result_file(sha256, tool, &output.id, name)
                        .await?
                }
                Input::Repo(url) => {
                    thorium
                        .repos
                        .download_result_file(url, tool, &output.id, name)
                        .await?
                }
            };
            if let Some(parent) = file_target.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&file_target, &attachment.data[..]).await?;
            written.push(file_target);
        }
        // the result itself goes beside its files
        target.push(&tool_path);
    }
    // write the result's contents
    let text = serde_json::to_string_pretty(&output.result)?;
    tokio::fs::write(&target, text).await?;
    written.push(target);
    Ok(written)
}

/// Name the tools a pipeline run may have stored results under
///
/// A stage whose job runs a tool stores its result under the tool's name, which can differ from
/// the stage's name, so the tool names of the run's plan are searched along with the stages.
///
/// # Arguments
///
/// * `stages` - The names of the run's stages
/// * `reaction` - The finished run's reaction
fn result_tools(mut stages: Vec<String>, reaction: &Reaction) -> Vec<String> {
    // find the name of the tool each planned stage runs
    let provenance = reaction.source.provenance();
    let tools = reaction.plan.stages.iter().flatten().filter_map(|stage| {
        provenance
            .tools
            .iter()
            .find(|tool| tool.tool_instance == stage.tool_instance)
            .map(|tool| {
                // an instance is named `<tool>@<version>`, and the result is stored under the tool
                let suffix = format!("@{}", tool.version);
                tool.name
                    .strip_suffix(&suffix)
                    .unwrap_or(&tool.name)
                    .to_owned()
            })
    });
    // search the stage names and the tool names once each
    stages.extend(tools);
    stages.sort();
    stages.dedup();
    stages
}

/// Download a finished run's results into a directory
///
/// A tool or workflow run's results are stored under its result key and a pipeline's under the
/// name of each of its stages or of the tool a stage runs; the newest result of each is written
/// for every input. With several inputs each input's results go in a directory named after it.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `reaction` - The finished run's reaction
/// * `dir` - The directory to write the results into
pub async fn download_results(
    thorium: &Thorium,
    reaction: &Reaction,
    dir: &Path,
) -> Result<Vec<PathBuf>, Error> {
    // find the tools whose results the run made, hidden results included, since a hidden
    // display type only keeps a result out of the UI's listings
    let params = match reaction.result_key() {
        Some(key) => ResultGetParams::default().tool(key).hidden(),
        None => ResultGetParams::default()
            .tools(result_tools(
                reactions::reaction_stages(thorium, reaction).await?,
                reaction,
            ))
            .hidden(),
    };
    // read the results of each input the run ran on
    let inputs: Vec<Input> = reaction
        .samples
        .iter()
        .map(|sample| Input::Sample(sample))
        .chain(reaction.repos.iter().map(|repo| Input::Repo(&repo.url)))
        .collect();
    let several = inputs.len() > 1;
    let mut written = Vec::new();
    for input in &inputs {
        let found = match input {
            Input::Sample(sha256) => thorium.files.get_results(sha256, &params).await?,
            Input::Repo(url) => thorium.repos.get_results(url, &params).await?,
        };
        // write the newest result of each tool
        let base = if several {
            dir.join(utils::fs::path_component(input.key()))
        } else {
            dir.to_path_buf()
        };
        let mut tools: Vec<(String, Output)> = found
            .results
            .into_iter()
            .filter_map(|(tool, mut outputs)| {
                (!outputs.is_empty()).then(|| (tool, outputs.swap_remove(0)))
            })
            .collect();
        tools.sort_by(|left, right| left.0.cmp(&right.0));
        for (tool, output) in &tools {
            written.extend(write_result(thorium, input, tool, output, &base).await?);
        }
    }
    Ok(written)
}

/// Describe how a run that did not complete ended
///
/// # Arguments
///
/// * `reaction` - The run's reaction
fn ended_message(reaction: &Reaction) -> String {
    // name the run and its status
    format!("Run {} {}", reaction.id, reaction.status)
}

/// Follow a started run as its command's flags ask
///
/// Without a follow flag only the run's reaction id is printed. `--watch` and `--wait-for` print
/// the run as they go or once the condition holds. `--wait`, `--output`, and `--logs` wait for the
/// run to finish, print it, and then print its logs and download its results; the command fails
/// unless the run completed, and exits 3 when the wait timed out.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `group` - The group the run is in
/// * `id` - The run's reaction id
/// * `common` - The run command's flags
pub async fn follow(
    thorium: &Thorium,
    group: &str,
    id: Uuid,
    common: &RunCommonArgs,
) -> Result<(), Error> {
    // only start the run when nothing asks to follow it
    if !common.follows() {
        println!("{id}");
        return Ok(());
    }
    // print the run in the requested format, human readable by default
    let format = common.format.format.unwrap_or(OutputFormat::Human);
    // watching and waiting on a condition share the commands' view loop
    if !common.waits() {
        return watch::show(format, &common.watch, Some(&common.wait_for), || async {
            Ok(RunView(thorium.reactions.get(group, id).await?))
        })
        .await;
    }
    // wait for the run to finish and print it
    let end = wait_finished(
        thorium,
        group,
        id,
        common.wait_for.timeout,
        common.watch.interval,
    )
    .await?;
    let (reaction, completed) = match &end {
        RunEnd::Completed(reaction) => (reaction, true),
        RunEnd::Failed(reaction) | RunEnd::TimedOut(reaction) => (reaction, false),
    };
    output::print(&RunView(reaction.clone()), format)?;
    // print the logs of a run that ended, completed or failed
    if common.logs && !matches!(end, RunEnd::TimedOut(_)) {
        let logs = reactions::run_logs_view(thorium, reaction.clone(), None).await?;
        output::print(&logs, format)?;
    }
    // a run that completed has results to download
    if completed && let Some(dir) = &common.output {
        let written = download_results(thorium, reaction, dir).await?;
        if written.is_empty() {
            eprintln!("Run {id} made no results to download");
        } else {
            eprintln!("Saved {} files to {}", written.len(), dir.display());
        }
    }
    // only a completed run succeeds
    match end {
        RunEnd::Completed(_) => Ok(()),
        RunEnd::Failed(reaction) => Err(Error::new(ended_message(&reaction))),
        RunEnd::TimedOut(reaction) => {
            // exit with the timeout code
            watch::mark_timed_out();
            Err(Error::new(format!(
                "Timed out after {:?} waiting for run {} to finish (it is {})",
                common.wait_for.timeout, reaction.id, reaction.status
            )))
        }
    }
}

/// Test helpers: a mock API and a reaction builder
#[cfg(test)]
pub(crate) mod testing {
    use std::sync::{Arc, Mutex};

    use thorium::Thorium;
    use thorium::models::{Reaction, ReactionStatus};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use uuid::Uuid;

    /// A mock API's request log: each request's line and body
    pub type Requests = Arc<Mutex<Vec<(String, String)>>>;

    /// Start a mock API that answers each request with its router
    ///
    /// Returns the mock's url and the requests it received.
    ///
    /// # Arguments
    ///
    /// * `router` - Picks the status and body of each answer from the request line and body
    pub async fn mock<F>(router: F) -> (String, Requests)
    where
        F: Fn(&str, &str) -> (u16, String) + Send + Sync + 'static,
    {
        // bind to a random local port and answer in the background
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let requests: Requests = Arc::new(Mutex::new(Vec::new()));
        let inner = requests.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                // read the request head and its body
                let mut buf = Vec::new();
                let mut chunk = [0_u8; 8192];
                loop {
                    let read = stream.read(&mut chunk).await.unwrap_or(0);
                    buf.extend_from_slice(&chunk[..read]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    let done = text.find("\r\n\r\n").is_some_and(|end| {
                        let length = text[..end]
                            .lines()
                            .find_map(|line| {
                                line.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|len| len.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        buf.len() >= end + 4 + length
                    });
                    if done || read == 0 {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&buf).to_string();
                let line = text.lines().next().unwrap_or_default().to_owned();
                let body = text
                    .split_once("\r\n\r\n")
                    .map_or_else(String::new, |(_, body)| body.to_owned());
                inner.lock().unwrap().push((line.clone(), body.clone()));
                let (code, answer) = router(&line, &body);
                let resp = format!(
                    "HTTP/1.1 {code} Mock\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{answer}",
                    answer.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        (host, requests)
    }

    /// Build a tool run's reaction with a status, on one input file
    ///
    /// # Arguments
    ///
    /// * `id` - The reaction's id
    /// * `status` - The reaction's status
    pub fn reaction(id: Uuid, status: ReactionStatus) -> Reaction {
        // a reaction of a tool run in the corn group
        let tool_instance = Uuid::new_v4();
        Reaction {
            id,
            group: "corn".to_owned(),
            creator: "mcarson".to_owned(),
            pipeline: "harvester".to_owned(),
            status,
            current_stage: 0,
            current_stage_progress: 0,
            current_stage_length: 1,
            args: std::collections::HashMap::default(),
            sla: chrono::Utc::now(),
            jobs: Vec::new(),
            tags: Vec::new(),
            parent: None,
            sub_reactions: 0,
            completed_sub_reactions: 0,
            generators: Vec::new(),
            samples: vec!["63b9c2cbd4f4f1f6".to_owned()],
            ephemeral: Vec::new(),
            parent_ephemeral: std::collections::HashMap::default(),
            repos: Vec::new(),
            trigger_depth: None,
            has_cache: false,
            source: thorium::models::ReactionSource::Tool {
                tool_instance,
                provenance: thorium::models::RunProvenance::default(),
            },
            plan: thorium::models::ReactionPlan {
                stages: vec![vec![thorium::models::PlannedStage {
                    name: "harvester".to_owned(),
                    tool_instance,
                }]],
            },
            run_token: None,
            run: None,
        }
    }

    /// Build a client for a mock API
    ///
    /// # Arguments
    ///
    /// * `host` - The mock's url
    pub async fn client(host: &str) -> Thorium {
        Thorium::build(host).token("token").build().await.unwrap()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use clap::Parser;
    use thorium::models::ReactionStatus;
    use uuid::Uuid;

    use super::testing::{client, mock, reaction};
    use super::{
        RunEnd, download_results, follow, is_finished, list_entries, safe_join, wait_finished,
    };
    use crate::args::runs::RunWorkflow;

    /// Parse the flags of a `workflows run` command
    ///
    /// # Arguments
    ///
    /// * `flags` - The flags after the workflow name
    fn flags(flags: &[&str]) -> crate::args::runs::RunCommonArgs {
        let mut argv = vec!["run", "wf"];
        argv.extend_from_slice(flags);
        RunWorkflow::try_parse_from(argv).unwrap().common
    }

    /// Statuses are final only once completed or failed
    #[test]
    fn only_completed_and_failed_are_finished() {
        // a run that has not finished is not final
        assert!(!is_finished(&ReactionStatus::Created));
        assert!(!is_finished(&ReactionStatus::Started));
        // both ways a run can end are final
        assert!(is_finished(&ReactionStatus::Completed));
        assert!(is_finished(&ReactionStatus::Failed));
    }

    /// List files skip comments and blanks and split bundles only when asked
    #[test]
    fn list_entries_skips_noise_and_splits_bundles() {
        let text = "\n# a comment\n  aaa  \nbbb, ccc\n\n";
        // samples may be bundled with commas
        assert_eq!(list_entries(text, true), ["aaa", "bbb", "ccc"]);
        // repos keep their line whole
        assert_eq!(list_entries(text, false), ["aaa", "bbb, ccc"]);
    }

    /// Result file names may not leave the download directory
    #[test]
    fn safe_join_refuses_names_that_escape() {
        let dir = Path::new("out");
        // plain and nested names stay inside
        assert_eq!(safe_join(dir, "a.txt").unwrap(), Path::new("out/a.txt"));
        assert_eq!(safe_join(dir, "d/a.txt").unwrap(), Path::new("out/d/a.txt"));
        // parents, absolute paths, and backslashes are refused
        for name in ["../a", "d/../../a", "/etc/passwd", "a\\b", ""] {
            assert!(safe_join(dir, name).is_err(), "{name} should be refused");
        }
    }

    /// Waiting reads the run until it finishes and reports how it ended
    #[tokio::test]
    async fn wait_finished_reports_each_ending() {
        let id = Uuid::new_v4();
        // a run that is started, then completed on the third read
        let reads = Arc::new(Mutex::new(0_u32));
        let counter = reads.clone();
        let (host, _) = mock(move |_, _| {
            let mut reads = counter.lock().unwrap();
            *reads += 1;
            let status = if *reads < 3 {
                ReactionStatus::Started
            } else {
                ReactionStatus::Completed
            };
            (200, serde_json::to_string(&reaction(id, status)).unwrap())
        })
        .await;
        let thorium = client(&host).await;
        let end = wait_finished(
            &thorium,
            "corn",
            id,
            Duration::from_secs(10),
            Duration::from_millis(20),
        )
        .await
        .unwrap();
        assert!(matches!(end, RunEnd::Completed(_)), "{end:?}");
        assert!(*reads.lock().unwrap() >= 3);
        // a failed run ends the wait as failed
        let (host, _) = mock(move |_, _| {
            (
                200,
                serde_json::to_string(&reaction(id, ReactionStatus::Failed)).unwrap(),
            )
        })
        .await;
        let thorium = client(&host).await;
        let end = wait_finished(
            &thorium,
            "corn",
            id,
            Duration::from_secs(10),
            Duration::from_millis(20),
        )
        .await
        .unwrap();
        assert!(matches!(end, RunEnd::Failed(_)), "{end:?}");
        // a run that never finishes times out with its last status
        let (host, _) = mock(move |_, _| {
            (
                200,
                serde_json::to_string(&reaction(id, ReactionStatus::Started)).unwrap(),
            )
        })
        .await;
        let thorium = client(&host).await;
        let end = wait_finished(
            &thorium,
            "corn",
            id,
            Duration::from_millis(150),
            Duration::from_millis(20),
        )
        .await
        .unwrap();
        assert!(matches!(end, RunEnd::TimedOut(_)), "{end:?}");
    }

    /// Downloading writes each tool's newest result under the run's result key, a hidden one too
    #[tokio::test]
    async fn download_results_writes_the_runs_result() {
        let id = Uuid::new_v4();
        let run = reaction(id, ReactionStatus::Completed);
        let outputs = serde_json::json!({"results": {"harvester": [{
            "id": Uuid::new_v4(), "groups": ["corn"], "cmd": null, "uploaded": chrono::Utc::now(),
            "result": {"functions": 412}, "entities": {}, "display_type": "Hidden", "children": {},
        }]}})
        .to_string();
        // the hidden result is only returned when hidden results are asked for, as the API does
        let (host, requests) = mock(move |line, _| {
            if line.starts_with("GET /api/files/results/63b9c2cbd4f4f1f6") {
                if line.contains("hidden=true") {
                    (200, outputs.clone())
                } else {
                    (200, r#"{"results": {}}"#.to_owned())
                }
            } else {
                (404, r#"{"error":"not found"}"#.to_owned())
            }
        })
        .await;
        let thorium = client(&host).await;
        let dir = std::env::temp_dir().join(format!("thorctl-run-wait-{}", Uuid::new_v4()));
        let written = download_results(&thorium, &run, &dir).await.unwrap();
        // the result is written as JSON named after the tool
        assert_eq!(written, [dir.join("harvester")]);
        let text = std::fs::read_to_string(dir.join("harvester")).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap()["functions"],
            412
        );
        // only the results route was read
        assert!(
            requests
                .lock()
                .unwrap()
                .iter()
                .all(|(line, _)| line.contains("/results/"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Following with `--wait` succeeds for a completed run and fails for a failed one
    #[tokio::test]
    async fn follow_wait_exits_by_status() {
        let id = Uuid::new_v4();
        let (host, _) = mock(move |_, _| {
            (
                200,
                serde_json::to_string(&reaction(id, ReactionStatus::Completed)).unwrap(),
            )
        })
        .await;
        let thorium = client(&host).await;
        follow(&thorium, "corn", id, &flags(&["--wait"]))
            .await
            .unwrap();
        // a failed run is an error whose text names the status
        let (host, _) = mock(move |_, _| {
            (
                200,
                serde_json::to_string(&reaction(id, ReactionStatus::Failed)).unwrap(),
            )
        })
        .await;
        let thorium = client(&host).await;
        let err = follow(&thorium, "corn", id, &flags(&["--wait"]))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Failed"), "{err}");
    }

    /// Following without any wait flag only prints the run's id and reads nothing
    #[tokio::test]
    async fn follow_without_flags_reads_nothing() {
        let (host, requests) = mock(|_, _| (500, "{}".to_owned())).await;
        let thorium = client(&host).await;
        follow(&thorium, "corn", Uuid::new_v4(), &flags(&[]))
            .await
            .unwrap();
        assert!(requests.lock().unwrap().is_empty());
    }
}
