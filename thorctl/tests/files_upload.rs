//! Tests the thorctl `files upload` command's runs (`--pipelines`, `--workflows`, `--tools`,
//! `--batch`, `--wait`, `--dry-run`, `-o json`) and its exit codes against the test API

mod common;

use common::{TestUser, assert_success, stdout};
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;
use thorium::models::{ImageScaler, Reaction, ToolMechanism};
use thorium::test_utilities::runs::{self, RunWorkflow};
use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium};
use uuid::Uuid;

/// Write files with unique contents into a new directory
///
/// # Arguments
///
/// * `count` - How many files to write
fn files(count: usize) -> Result<(PathBuf, Vec<PathBuf>), Error> {
    // a directory of random files so each upload is new
    let dir = std::env::temp_dir().join(common::unique("upload-runs-"));
    std::fs::create_dir_all(&dir)?;
    let mut paths = Vec::new();
    for index in 0..count {
        let path = dir.join(format!("file-{index}.bin"));
        std::fs::write(&path, format!("{} {index}", Uuid::new_v4()))?;
        paths.push(path);
    }
    Ok((dir, paths))
}

/// Create a ready workflow in the user's group whose agent calls one container tool
///
/// # Arguments
///
/// * `user` - The test user
/// * `name` - The workflow's name
async fn workflow(user: &TestUser, name: &str) -> Result<RunWorkflow, Error> {
    // an Image tool and a container tool that runs it
    let groups = [user.group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &user.admin).await?;
    let (_, boxed) = runs::container_tool(&groups, image.id, &user.admin).await?;
    runs::ready_workflow(name, &groups, &[(&boxed, ToolMechanism::Cli)], &user.admin).await
}

/// Get the authorization header of a token client for the user
///
/// # Arguments
///
/// * `user` - The test user
async fn auth(user: &TestUser) -> Result<String, Error> {
    // trade the user's password for a token
    let (token, _) = Thorium::auth(
        &user.admin.host,
        &user.username,
        &user.password,
        &reqwest::Client::new(),
    )
    .await?;
    Ok(test_utilities::auth_header(&token))
}

/// Read every run tagged with a batch name in the user's group
///
/// # Arguments
///
/// * `user` - The test user
/// * `batch` - The batch name
async fn batch_runs(user: &TestUser, batch: &str) -> Result<Vec<Reaction>, Error> {
    // read each page of the tag's reactions
    let mut cursor = user
        .client
        .reactions
        .list_tag(&user.group, batch)
        .page_size(100)
        .details();
    let mut found = Vec::new();
    while !cursor.exhausted {
        cursor.next().await?;
        found.append(&mut cursor.details);
    }
    Ok(found)
}

/// Wait for a batch to have a number of runs
///
/// # Arguments
///
/// * `user` - The test user
/// * `batch` - The batch name
/// * `count` - How many runs to wait for
async fn wait_for_runs(user: &TestUser, batch: &str, count: usize) -> Result<Vec<Reaction>, Error> {
    // poll the batch until it has all its runs
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let found = batch_runs(user, batch).await?;
        if found.len() >= count {
            return Ok(found);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "batch {batch} never had {count} runs"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Parse the JSON lines a command printed
///
/// # Arguments
///
/// * `output` - The command's output
fn lines(output: &std::process::Output) -> Vec<Value> {
    // every stdout line is one JSON object
    stdout(output)
        .lines()
        .map(|line| serde_json::from_str(line).expect("a JSON line"))
        .collect()
}

/// An upload starts a pipeline, a workflow, and a tool on each new file, tagged with one batch,
/// and runs nothing for a file that already exists
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn upload_starts_every_kind_of_run_on_new_files() -> Result<(), Error> {
    // a user with a pipeline, a workflow, and a tool
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "upload-runs-all",
        &generators::readable_name("ione", &suffix),
        &generators::readable_name("flax", &suffix),
    )
    .await?;
    let triage = workflow(&user, "Triage").await?;
    let (_, tool) = runs::image_tool(&[user.group.as_str()], ImageScaler::K8s, &user.admin).await?;
    let pipe = generators::pipelines(&user.group, 1, false, &user.client)
        .await?
        .remove(0);
    let pipeline = format!("{}/{}", user.group, pipe.name);
    let (dir, paths) = files(3)?;
    let instance = triage.id().to_string();
    let tool_id = tool.id.to_string();
    let dir_arg = dir.to_string_lossy().into_owned();
    // a JSON upload prints one line per file and one per run
    let args = [
        "files",
        "upload",
        "-g",
        &user.group,
        "-p",
        &pipeline,
        "--workflows",
        &instance,
        "--tools",
        &tool_id,
        "--batch",
        "all-kinds",
        "-o",
        "json",
        &dir_arg,
    ];
    let out = user.run("upload-runs-all-kinds", &args).await;
    assert_success(&out);
    let printed = lines(&out);
    let kind = |name: &str| printed.iter().filter(|line| line["type"] == name).count();
    assert_eq!((kind("upload"), kind("run")), (3, 9), "{}", stdout(&out));
    assert!(
        printed
            .iter()
            .filter(|line| line["type"] == "run")
            .all(|line| line["status"] == "Created"
                && Uuid::parse_str(line["id"].as_str().unwrap()).is_ok())
    );
    // every file has a run of each kind, tagged with the batch
    let runs = batch_runs(&user, "all-kinds").await?;
    assert_eq!(runs.len(), 9);
    for path in &paths {
        let sha = sha256(path);
        let on_file = runs
            .iter()
            .filter(|run| run.samples == [sha.clone()])
            .count();
        assert_eq!(on_file, 3, "{}", path.display());
    }
    // the same upload again finds every file and starts nothing
    let out = user.run("upload-runs-all-exist", &args).await;
    assert_success(&out);
    let printed = lines(&out);
    assert!(
        printed.iter().all(|line| line["status"] == "exists"),
        "{}",
        stdout(&out)
    );
    assert_eq!(batch_runs(&user, "all-kinds").await?.len(), 9);
    // a dry run says what would start and starts nothing
    let (dir, _) = files(2)?;
    let dir_arg = dir.to_string_lossy().into_owned();
    let args = [
        "files",
        "upload",
        "-g",
        &user.group,
        "--workflows",
        &instance,
        "--batch",
        "dry",
        "--dry-run",
        &dir_arg,
    ];
    let out = user.run("upload-runs-dry-run", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("Runs: workflow"), "{}", stdout(&out));
    assert!(batch_runs(&user, "dry").await?.is_empty());
    Ok(())
}

/// Get the SHA256 of a file as hex
///
/// # Arguments
///
/// * `path` - The file to hash
fn sha256(path: &std::path::Path) -> String {
    use sha2::{Digest, Sha256};
    // hash the whole file
    let bytes = std::fs::read(path).expect("a readable file");
    format!("{:x}", Sha256::digest(bytes))
}

/// A pipelines only upload with no flags creates untagged reactions through the bulk route and
/// prints the same table as before
#[tokio::test]
async fn upload_pipelines_only_stays_untagged() -> Result<(), Error> {
    // a user with a pipeline
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "upload-runs-pipelines",
        &generators::readable_name("cora", &suffix),
        &generators::readable_name("rye", &suffix),
    )
    .await?;
    let pipe = generators::pipelines(&user.group, 1, false, &user.client)
        .await?
        .remove(0);
    let pipeline = format!("{}/{}", user.group, pipe.name);
    let (dir, paths) = files(3)?;
    let dir_arg = dir.to_string_lossy().into_owned();
    let args = [
        "files",
        "upload",
        "-g",
        &user.group,
        "-p",
        &pipeline,
        &dir_arg,
    ];
    let out = user.run("upload-runs-pipelines-only", &args).await;
    assert_success(&out);
    let text = stdout(&out);
    // the table, one uploaded line and one reaction line per file, and no batch
    assert!(text.starts_with("CODE"), "{text}");
    assert!(!text.contains("Batch"), "{text}");
    for path in &paths {
        let on_file = text
            .lines()
            .filter(|line| line.contains(&sha256(path)))
            .count();
        assert_eq!(on_file, 2, "{text}");
    }
    // every reaction exists for its file with no tags
    let mut cursor = user.client.reactions.list(&user.group, &pipe.name);
    let mut reactions = Vec::new();
    while !cursor.exhausted {
        cursor.next().await?;
        reactions.append(&mut cursor.names);
    }
    assert_eq!(reactions.len(), 3, "{reactions:?}");
    Ok(())
}

/// Following the runs of an upload exits 0 when every run completed, 1 when any failed or any
/// file failed to upload, and 3 when runs had not finished before the timeout
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn upload_wait_exits_by_how_the_runs_ended() -> Result<(), Error> {
    // a user with a workflow
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "upload-runs-wait",
        &generators::readable_name("thea", &suffix),
        &generators::readable_name("oat", &suffix),
    )
    .await?;
    let triage = workflow(&user, "Triage").await?;
    let instance = triage.id().to_string();
    let auth = auth(&user).await?;
    // every run completing exits 0 and counts them
    let (dir, _) = files(3)?;
    let dir_arg = dir.to_string_lossy().into_owned();
    let args = [
        "files",
        "upload",
        "-g",
        &user.group,
        "--workflows",
        &instance,
        "--batch",
        "all-done",
        "--wait",
        "--interval",
        "200ms",
        "--timeout",
        "60s",
        &dir_arg,
    ];
    let waiting = user.spawn(&args);
    for run in wait_for_runs(&user, "all-done", 3).await? {
        runs::handle_reaction(&user.client, &auth, &user.group, run.id, "proceed").await?;
    }
    let out = tokio::time::timeout(Duration::from_secs(90), waiting.wait_with_output())
        .await
        .expect("upload --wait didn't return")?;
    user.record("upload-runs-wait-completed", &args, &out);
    assert_success(&out);
    assert!(
        stdout(&out).contains("3 inputs (3 Completed)"),
        "{}",
        stdout(&out)
    );
    // one run failing exits 1 and names the failure
    let (dir, _) = files(3)?;
    let dir_arg = dir.to_string_lossy().into_owned();
    let args = [
        "files",
        "upload",
        "-g",
        &user.group,
        "--workflows",
        &instance,
        "--batch",
        "one-failed",
        "--wait",
        "--interval",
        "200ms",
        "--timeout",
        "60s",
        &dir_arg,
    ];
    let waiting = user.spawn(&args);
    let started = wait_for_runs(&user, "one-failed", 3).await?;
    let failed = started[0].id;
    for (index, run) in started.iter().enumerate() {
        let cmd = if index == 0 { "fail" } else { "proceed" };
        runs::handle_reaction(&user.client, &auth, &user.group, run.id, cmd).await?;
    }
    let out = tokio::time::timeout(Duration::from_secs(90), waiting.wait_with_output())
        .await
        .expect("upload --wait didn't return")?;
    user.record("upload-runs-wait-failed", &args, &out);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stdout(&out).contains(&format!("Failed: {failed}")),
        "{}",
        stdout(&out)
    );
    // runs that don't finish before the timeout exit 3
    let (dir, _) = files(2)?;
    let dir_arg = dir.to_string_lossy().into_owned();
    let args = [
        "files",
        "upload",
        "-g",
        &user.group,
        "--workflows",
        &instance,
        "--wait",
        "--interval",
        "200ms",
        "--timeout",
        "1s",
        &dir_arg,
    ];
    let out = user.run("upload-runs-wait-timeout", &args).await;
    assert_eq!(out.status.code(), Some(3));
    assert!(stdout(&out).contains("Unfinished:"), "{}", stdout(&out));
    // a missing path is an upload failure and exits 1 even with nothing to run
    let args = ["files", "upload", "-g", &user.group, "/no/such/path"];
    let out = user.run("upload-runs-missing-path", &args).await;
    assert_eq!(out.status.code(), Some(1));
    Ok(())
}

/// Workflows and tools that can't run on the upload are refused before any file is uploaded
#[tokio::test]
async fn upload_refuses_unusable_targets_before_uploading() -> Result<(), Error> {
    // a user with a workflow in one group and an upload to another
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "upload-runs-refused",
        &generators::readable_name("lune", &suffix),
        &generators::readable_name("barley", &suffix),
    )
    .await?;
    let triage = workflow(&user, "Triage").await?;
    let instance = triage.id().to_string();
    let (dir, paths) = files(1)?;
    let dir_arg = dir.to_string_lossy().into_owned();
    // a workflow that doesn't exist
    let args = [
        "files",
        "upload",
        "-g",
        &user.group,
        "--workflows",
        "no-such-workflow",
        &dir_arg,
    ];
    let out = user.run("upload-runs-no-workflow", &args).await;
    assert_eq!(out.status.code(), Some(1));
    // a group the files are not uploaded to
    let args = [
        "files",
        "upload",
        "-g",
        &user.group,
        "--workflows",
        &format!("elsewhere/{instance}"),
        &dir_arg,
    ];
    let out = user.run("upload-runs-wrong-group", &args).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("elsewhere"));
    // nothing was uploaded by either
    assert!(user.client.files.get(&sha256(&paths[0])).await.is_err());
    Ok(())
}

/// More files than one bulk request holds each get their reaction, and `--workers` bounds nothing
/// but the speed
#[tokio::test]
async fn upload_pipeline_reactions_span_bulk_chunks() -> Result<(), Error> {
    // a user with a pipeline and 130 new files
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "upload-runs-chunks",
        &generators::readable_name("sela", &suffix),
        &generators::readable_name("emmer", &suffix),
    )
    .await?;
    let pipe = generators::pipelines(&user.group, 1, false, &user.client)
        .await?
        .remove(0);
    let pipeline = format!("{}/{}", user.group, pipe.name);
    let (dir, paths) = files(130)?;
    let dir_arg = dir.to_string_lossy().into_owned();
    let args = [
        "--workers",
        "4",
        "files",
        "upload",
        "-g",
        &user.group,
        "-p",
        &pipeline,
        "--batch",
        "chunks",
        &dir_arg,
    ];
    let out = user.run("upload-runs-bulk-chunks", &args).await;
    assert_success(&out);
    // every file has exactly one reaction, tagged with the batch
    let runs = batch_runs(&user, "chunks").await?;
    assert_eq!(runs.len(), 130);
    let mut samples: Vec<String> = runs.iter().map(|run| run.samples[0].clone()).collect();
    samples.sort();
    samples.dedup();
    assert_eq!(samples.len(), paths.len());
    Ok(())
}
