//! The tool runs a harness starts through the CLI wrapper, and what happens to the ones it never
//! collected
//!
//! The wrapper appends a line to `tool-runs.jsonl` when it starts a tool run and another when it
//! downloads the run's results. When the harness exits, the thorium-agent applies the harness's
//! orphaned run policy to every run that was never collected and to every unfinished sub reaction
//! of the workflow run, including runs started without the wrapper.

use chrono::{DateTime, Utc};
use crossbeam::channel::Sender;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use thorium::client::tool_runs::ToolRunHooks;
use thorium::models::{GenericJob, OrphanedRunPolicy, Reaction, ReactionStatus};
use thorium::{Error, Thorium};
use uuid::Uuid;

use crate::log;

/// Where the CLI wrapper records the tool runs it starts
pub const DEFAULT_TOOL_RUNS: &str = "/tmp/thorium/tool-runs.jsonl";

/// One line of `tool-runs.jsonl`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolRunLine {
    /// The tool run's reaction
    pub reaction: Uuid,
    /// The group the tool run is in
    pub group: String,
    /// The tool instance the run runs
    #[serde(default)]
    pub tool_instance: Option<Uuid>,
    /// When the wrapper started the run
    pub started: DateTime<Utc>,
    /// Whether the run's results were downloaded
    #[serde(default)]
    pub collected: bool,
    /// Where the run's results are downloaded to
    #[serde(default)]
    pub output: Option<PathBuf>,
    /// The most bytes of the run's result returned to the harness (0 = unlimited)
    #[serde(default)]
    pub max_result_bytes: u64,
}

/// Append one line to a `tool-runs.jsonl` file
///
/// # Arguments
///
/// * `path` - The `tool-runs.jsonl` file
/// * `line` - The line to append
pub fn append(path: &Path, line: &ToolRunLine) -> Result<(), Error> {
    // create the file's directory if needed
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // write the whole line at once so lines from concurrent wrappers never interleave
    let mut text = serde_json::to_string(line)?;
    text.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(text.as_bytes())?;
    Ok(())
}

/// Read the tool runs a `tool-runs.jsonl` file records, by reaction
///
/// Later lines for a run update earlier ones, and a run stays collected once a line marks it.
/// A missing file records no runs.
///
/// # Arguments
///
/// * `path` - The `tool-runs.jsonl` file
pub fn read(path: &Path) -> Result<BTreeMap<Uuid, ToolRunLine>, Error> {
    // a missing file records no runs
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(Error::from(error)),
    };
    // merge each run's lines in order, keeping it collected once a line marks it
    let mut runs: BTreeMap<Uuid, ToolRunLine> = BTreeMap::new();
    for raw in text.lines().filter(|raw| !raw.trim().is_empty()) {
        let line: ToolRunLine = serde_json::from_str(raw)?;
        let collected = line.collected || runs.get(&line.reaction).is_some_and(|run| run.collected);
        let reaction = line.reaction;
        runs.insert(reaction, ToolRunLine { collected, ..line });
    }
    Ok(runs)
}

/// Records each tool run the CLI wrapper starts and collects in `tool-runs.jsonl`
pub struct JsonlHooks {
    /// The `tool-runs.jsonl` file
    pub path: PathBuf,
    /// The tool instance being run
    pub tool_instance: Option<Uuid>,
    /// Where the run's results are downloaded to
    pub output: PathBuf,
    /// The most bytes of the run's result returned to the harness
    pub max_result_bytes: u64,
}

impl ToolRunHooks for JsonlHooks {
    /// Record a started run
    ///
    /// # Arguments
    ///
    /// * `reaction` - The run's reaction
    fn started(&mut self, reaction: &Reaction) -> Result<(), Error> {
        // record the run and where its results go
        let line = ToolRunLine {
            reaction: reaction.id,
            group: reaction.group.clone(),
            tool_instance: self.tool_instance,
            started: Utc::now(),
            collected: false,
            output: Some(self.output.clone()),
            max_result_bytes: self.max_result_bytes,
        };
        append(&self.path, &line)
    }

    /// Record that a run's results were downloaded
    ///
    /// # Arguments
    ///
    /// * `reaction` - The run's reaction
    fn collected(&mut self, reaction: &Reaction) -> Result<(), Error> {
        // keep what the run's start recorded, if it was recorded here
        let recorded = read(&self.path)?.remove(&reaction.id);
        let line = ToolRunLine {
            collected: true,
            ..recorded.unwrap_or(ToolRunLine {
                reaction: reaction.id,
                group: reaction.group.clone(),
                tool_instance: self.tool_instance,
                started: Utc::now(),
                collected: true,
                output: Some(self.output.clone()),
                max_result_bytes: self.max_result_bytes,
            })
        };
        append(&self.path, &line)
    }
}

/// Apply a harness's orphaned run policy once it exits
///
/// Every run in `tool-runs.jsonl` that was never collected and every unfinished sub reaction of
/// the workflow run is an orphan. `Delete` fails each unfinished orphan, so its parent counts it,
/// then deletes each orphan; `Warn` only logs each one. Each orphan gets one stage log line.
///
/// # Arguments
///
/// * `thorium` - The client to handle the orphans with
/// * `job` - The harness's job
/// * `policy` - The harness's orphaned run policy
/// * `path` - The `tool-runs.jsonl` file
/// * `logs` - Where to send stage log lines
pub async fn clean_up(
    thorium: &Thorium,
    job: &GenericJob,
    policy: OrphanedRunPolicy,
    path: &Path,
    logs: &mut Sender<String>,
) -> Result<(), Error> {
    // the runs the wrapper started and never collected
    let recorded = read(path)?;
    // the workflow run's sub reactions, including runs started without the wrapper
    let mut cursor = thorium
        .reactions
        .list_sub(&job.group, &job.reaction)
        .details();
    let mut subs: BTreeMap<Uuid, Reaction> = BTreeMap::new();
    loop {
        cursor.next().await?;
        subs.extend(cursor.details.drain(..).map(|sub| (sub.id, sub)));
        if cursor.exhausted {
            break;
        }
    }
    // an orphan is an uncollected run or an unfinished sub reaction, by reaction
    let mut orphans: BTreeMap<Uuid, (String, Option<ReactionStatus>)> = BTreeMap::new();
    for run in recorded.values().filter(|run| !run.collected) {
        let status = subs.get(&run.reaction).map(|sub| sub.status.clone());
        orphans.insert(run.reaction, (run.group.clone(), status));
    }
    for sub in subs.values().filter(|sub| !finished(&sub.status)) {
        orphans
            .entry(sub.id)
            .or_insert_with(|| (sub.group.clone(), Some(sub.status.clone())));
    }
    for (id, (group, status)) in orphans {
        let shown = status
            .as_ref()
            .map_or_else(|| "unknown".to_owned(), ToString::to_string);
        match policy {
            // only log the orphan and leave it running
            OrphanedRunPolicy::Warn => {
                log!(
                    logs,
                    "Orphaned tool run {} is {}; leaving it running",
                    id,
                    shown
                );
            }
            OrphanedRunPolicy::Delete => {
                // count an unfinished orphan in its parent before deleting it
                if !status.as_ref().is_some_and(finished) {
                    match thorium.reactions.fail(&group, &id).await {
                        Ok(_) => (),
                        // another caller failed it first, and a deleted run needs no failing
                        Err(error)
                            if matches!(
                                error.status(),
                                Some(StatusCode::CONFLICT | StatusCode::NOT_FOUND)
                            ) => {}
                        Err(error) => return Err(error),
                    }
                }
                // delete it, which also stops its jobs
                match thorium.reactions.delete(&group, &id).await {
                    Ok(_) => (),
                    Err(error) if error.status() == Some(StatusCode::NOT_FOUND) => (),
                    Err(error) => return Err(error),
                }
                log!(logs, "Deleted orphaned tool run {} ({})", id, shown);
            }
        }
    }
    Ok(())
}

/// Whether a reaction status is final
///
/// # Arguments
///
/// * `status` - The status to check
fn finished(status: &ReactionStatus) -> bool {
    matches!(status, ReactionStatus::Completed | ReactionStatus::Failed)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use std::path::Path;
    use thorium::Thorium;
    use thorium::client::tool_runs::ToolRunHooks;
    use thorium::models::{OrphanedRunPolicy, ReactionStatus};
    use uuid::Uuid;

    use super::{JsonlHooks, ToolRunLine, append, clean_up, read};
    use crate::libs::test_server::{self, MockThorium};

    /// Record a started tool run in a `tool-runs.jsonl` file
    ///
    /// # Arguments
    ///
    /// * `path` - The `tool-runs.jsonl` file
    /// * `reaction` - The run's reaction
    /// * `collected` - Whether the run's results were downloaded
    fn record(path: &Path, reaction: Uuid, collected: bool) {
        // one line for the start and one more once collected
        let mut line = ToolRunLine {
            reaction,
            group: "corn".to_owned(),
            tool_instance: Some(Uuid::new_v4()),
            started: Utc::now(),
            collected: false,
            output: None,
            max_result_bytes: 0,
        };
        append(path, &line).unwrap();
        if collected {
            line.collected = true;
            append(path, &line).unwrap();
        }
    }

    /// The wrapper's hooks write one line per started run and mark it collected after its results
    /// are downloaded
    #[test]
    fn hooks_record_started_and_collected_runs() {
        // hooks writing to a fresh file
        let dir = test_server::scratch();
        let path = dir.join("tool-runs.jsonl");
        let mut hooks = JsonlHooks {
            path: path.clone(),
            tool_instance: Some(Uuid::new_v4()),
            output: dir.join("out"),
            max_result_bytes: 64,
        };
        let first = test_server::reaction(Uuid::new_v4(), ReactionStatus::Created, None);
        let second = test_server::reaction(Uuid::new_v4(), ReactionStatus::Created, None);
        // two runs start and only the first is collected
        hooks.started(&first).unwrap();
        hooks.started(&second).unwrap();
        hooks.collected(&first).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 3, "{text}");
        let runs = read(&path).unwrap();
        assert_eq!(runs.len(), 2);
        assert!(runs[&first.id].collected);
        assert!(!runs[&second.id].collected);
        assert_eq!(runs[&second.id].group, "corn");
        assert_eq!(runs[&second.id].tool_instance, hooks.tool_instance);
        assert_eq!(runs[&second.id].output, Some(dir.join("out")));
        assert_eq!(runs[&second.id].max_result_bytes, 64);
        // a missing file records no runs
        assert!(read(&dir.join("missing.jsonl")).unwrap().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The runs and sub reactions an orphan cleanup sees, by role
    struct Orphans {
        /// The workflow run's harness job
        job: thorium::models::GenericJob,
        /// An uncollected run that is still started
        started: Uuid,
        /// An uncollected run that completed
        completed: Uuid,
        /// A collected run
        collected: Uuid,
        /// A started sub reaction the wrapper didn't start
        direct: Uuid,
        /// A completed sub reaction the wrapper didn't start
        direct_done: Uuid,
        /// An uncollected started run another caller fails first
        conflict: Uuid,
        /// The mock API
        mock: MockThorium,
        /// The scratch directory holding `tool-runs.jsonl`
        dir: std::path::PathBuf,
    }

    /// Set up a harness job with every kind of orphan and a mock API that lists them
    async fn orphans() -> Orphans {
        // the harness job of a workflow run in the corn group
        let job = test_server::job(Uuid::new_v4());
        let parent = job.reaction;
        let ids: Vec<Uuid> = (0..6).map(|_| Uuid::new_v4()).collect();
        let (started, completed, collected, direct, direct_done, conflict) =
            (ids[0], ids[1], ids[2], ids[3], ids[4], ids[5]);
        // the wrapper started four runs and collected one
        let dir = test_server::scratch();
        let path = dir.join("tool-runs.jsonl");
        record(&path, started, false);
        record(&path, completed, false);
        record(&path, collected, true);
        record(&path, conflict, false);
        // the workflow run's sub reactions
        let subs = vec![
            test_server::reaction(started, ReactionStatus::Started, Some(parent)),
            test_server::reaction(completed, ReactionStatus::Completed, Some(parent)),
            test_server::reaction(collected, ReactionStatus::Completed, Some(parent)),
            test_server::reaction(direct, ReactionStatus::Started, Some(parent)),
            test_server::reaction(direct_done, ReactionStatus::Completed, Some(parent)),
            test_server::reaction(conflict, ReactionStatus::Created, Some(parent)),
        ];
        let listing = serde_json::json!({"cursor": null, "details": subs}).to_string();
        let sub_path = format!("GET /api/reactions/sub/corn/{parent}/details/");
        let conflict_fail = format!("POST /api/reactions/handle/corn/{conflict}/fail ");
        let mock = MockThorium::start(move |line| {
            if line.starts_with(&sub_path) {
                (200, listing.clone())
            } else if line.starts_with(&conflict_fail) {
                (409, r#"{"error":"already failed"}"#.to_owned())
            } else if line.contains("/fail ") {
                (202, r#"{"status":"Failed"}"#.to_owned())
            } else if line.starts_with("DELETE /api/reactions/") {
                (204, String::new())
            } else {
                (500, "{}".to_owned())
            }
        })
        .await;
        Orphans {
            job,
            started,
            completed,
            collected,
            direct,
            direct_done,
            conflict,
            mock,
            dir,
        }
    }

    /// Get where a request line first appears, if it does
    ///
    /// # Arguments
    ///
    /// * `lines` - The request lines in order
    /// * `pattern` - The text the line must contain
    fn position(lines: &[String], pattern: &str) -> Option<usize> {
        lines.iter().position(|line| line.contains(pattern))
    }

    /// `Delete` fails each unfinished orphan before deleting it, deletes uncollected finished runs
    /// without failing them, ignores a `409` from a fail, leaves collected and finished direct runs
    /// alone, and logs one line per deletion
    #[tokio::test]
    async fn delete_policy_fails_then_deletes_orphans() {
        // every kind of orphan
        let setup = orphans().await;
        let thorium = Thorium::build(&setup.mock.host)
            .token("token")
            .build()
            .await
            .unwrap();
        let (mut sender, receiver) = crossbeam::channel::unbounded();
        let path = setup.dir.join("tool-runs.jsonl");
        // clean up after the harness
        clean_up(
            &thorium,
            &setup.job,
            OrphanedRunPolicy::Delete,
            &path,
            &mut sender,
        )
        .await
        .unwrap();
        let lines = setup.mock.lines();
        // each unfinished orphan is failed, then deleted
        for id in [setup.started, setup.direct, setup.conflict] {
            let fail = position(&lines, &format!("/handle/corn/{id}/fail ")).expect("failed");
            let delete =
                position(&lines, &format!("DELETE /api/reactions/corn/{id} ")).expect("deleted");
            assert!(fail < delete, "{lines:?}");
        }
        // an uncollected completed run is deleted without being failed
        assert!(position(&lines, &format!("/handle/corn/{}/fail ", setup.completed)).is_none());
        assert!(
            position(
                &lines,
                &format!("DELETE /api/reactions/corn/{} ", setup.completed)
            )
            .is_some()
        );
        // collected runs and finished direct runs are left alone
        for id in [setup.collected, setup.direct_done] {
            assert!(
                !lines.iter().any(|line| line.contains(&id.to_string())),
                "{lines:?}"
            );
        }
        // one stage log line per deletion, each naming its run
        let logs: Vec<String> = receiver.try_iter().collect();
        assert_eq!(logs.len(), 4, "{logs:?}");
        for id in [setup.started, setup.completed, setup.direct, setup.conflict] {
            assert!(
                logs.iter().any(|log| log.contains(&id.to_string())),
                "{logs:?}"
            );
        }
        std::fs::remove_dir_all(&setup.dir).unwrap();
    }

    /// `Warn` only logs each orphan and leaves it running
    #[tokio::test]
    async fn warn_policy_only_logs_orphans() {
        // every kind of orphan
        let setup = orphans().await;
        let thorium = Thorium::build(&setup.mock.host)
            .token("token")
            .build()
            .await
            .unwrap();
        let (mut sender, receiver) = crossbeam::channel::unbounded();
        let path = setup.dir.join("tool-runs.jsonl");
        // clean up after the harness
        clean_up(
            &thorium,
            &setup.job,
            OrphanedRunPolicy::Warn,
            &path,
            &mut sender,
        )
        .await
        .unwrap();
        // nothing was failed or deleted
        let lines = setup.mock.lines();
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("/fail ") || line.starts_with("DELETE")),
            "{lines:?}"
        );
        // one line per orphan
        let logs: Vec<String> = receiver.try_iter().collect();
        assert_eq!(logs.len(), 4, "{logs:?}");
        for id in [setup.started, setup.completed, setup.direct, setup.conflict] {
            assert!(
                logs.iter().any(|log| log.contains(&id.to_string())),
                "{logs:?}"
            );
        }
        std::fs::remove_dir_all(&setup.dir).unwrap();
    }
}
