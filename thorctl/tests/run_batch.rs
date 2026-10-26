//! Tests the thorctl `run <kind>` command's batch mode (`--one-per-input`, `--batch`, `--dry-run`,
//! lists on stdin, following a batch) and `runs get`/`runs cancel` by batch against the test API

mod common;

use common::{TestUser, assert_success, stdout};
use serde_json::Value;
use std::process::Stdio;
use std::time::Duration;
use thorium::models::{ImageScaler, Reaction, ReactionStatus, SampleRequest, ToolMechanism};
use thorium::test_utilities::runs::{self, RunWorkflow};
use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

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

/// Upload some random files to the user's group and get their SHA256s
///
/// # Arguments
///
/// * `user` - The test user
/// * `count` - How many files to upload
async fn samples(user: &TestUser, count: usize) -> Result<Vec<String>, Error> {
    // upload random files
    let mut found = Vec::new();
    for _ in 0..count {
        let req: SampleRequest = generators::gen_sample(&user.group);
        found.push(user.client.files.create(req).await?.sha256);
    }
    Ok(found)
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

/// Parse the JSON lines a batch printed
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

/// `run workflow --one-per-input` starts one tagged run per file, `--dry-run` starts none, a list
/// can come from stdin, and `runs get --batch` and `--status` list the batch
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn run_one_per_input_starts_a_tagged_batch() -> Result<(), Error> {
    // a user with a ready workflow and files
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "run-batch-fan-out",
        &generators::readable_name("iris", &suffix),
        &generators::readable_name("flora", &suffix),
    )
    .await?;
    let triage = workflow(&user, "Triage").await?;
    let files = samples(&user, 3).await?;
    let id = triage.id().to_string();
    // a fan-out prints one JSON line per input and tags every run with the batch
    let args = [
        "run",
        "workflow",
        &id,
        "-g",
        &user.group,
        "--sample",
        &files[0],
        "--sample",
        &files[1],
        "--sample",
        &files[2],
        "--one-per-input",
        "--batch",
        "nightly",
        "--tag",
        "extra",
        "-o",
        "json",
    ];
    let out = user.run("run-batch-fan-out", &args).await;
    assert_success(&out);
    let printed = lines(&out);
    assert_eq!(printed.len(), 3, "{}", stdout(&out));
    let inputs: Vec<&str> = printed
        .iter()
        .map(|line| line["input"].as_str().unwrap())
        .collect();
    assert_eq!(inputs, [&files[0], &files[1], &files[2]]);
    assert!(
        printed.iter().all(|line| line["status"] == "Created"
            && Uuid::parse_str(line["id"].as_str().unwrap()).is_ok())
    );
    // each run has one file and both tags
    let runs = batch_runs(&user, "nightly").await?;
    assert_eq!(runs.len(), 3);
    for run in &runs {
        assert_eq!(run.samples.len(), 1, "{:?}", run.samples);
        assert!(run.tags.contains(&"extra".to_owned()), "{:?}", run.tags);
    }
    // a dry run shows the plan and starts nothing
    let args = [
        "run",
        "workflow",
        &id,
        "--sample",
        &files[0],
        "--sample",
        &files[1],
        "--one-per-input",
        "--batch",
        "dry",
        "--dry-run",
    ];
    let out = user.run("run-batch-dry-run", &args).await;
    assert_success(&out);
    let text = stdout(&out);
    assert!(text.contains("would start 2 runs of workflow"), "{text}");
    assert!(text.contains("Nothing was started."), "{text}");
    assert!(batch_runs(&user, "dry").await?.is_empty());
    // a list on stdin is read for the samples
    let args = [
        "run",
        "workflow",
        &id,
        "-g",
        &user.group,
        "--sample-list",
        "-",
        "--one-per-input",
        "--batch",
        "piped",
        "-o",
        "json",
    ];
    let mut command = user.command(&args);
    command.stdin(Stdio::piped());
    let mut child = command.spawn().expect("failed to start thorctl");
    let mut stdin = child.stdin.take().expect("a stdin pipe");
    stdin
        .write_all(format!("# the files\n{}\n{},{}\n", files[0], files[1], files[2]).as_bytes())
        .await?;
    drop(stdin);
    let out = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .expect("thorctl didn't finish")?;
    assert_success(&out);
    assert_eq!(lines(&out).len(), 3, "{}", stdout(&out));
    assert_eq!(batch_runs(&user, "piped").await?.len(), 3);
    // runs get lists a batch, narrowed by status
    let args = ["runs", "get", "--batch", "nightly", "-o", "json"];
    let out = user.run("runs-get-batch", &args).await;
    assert_success(&out);
    let listed: Value = serde_json::from_str(&stdout(&out)).expect("a JSON list");
    assert_eq!(listed.as_array().unwrap().len(), 3, "{listed}");
    let args = [
        "runs", "get", "--batch", "nightly", "--status", "failed", "-o", "json",
    ];
    let out = user.run("runs-get-batch-failed", &args).await;
    assert_success(&out);
    let listed: Value = serde_json::from_str(&stdout(&out)).expect("a JSON list");
    assert!(listed.as_array().unwrap().is_empty(), "{listed}");
    // a fan-out with no input is refused
    let args = ["run", "workflow", &id, "-g", &user.group, "--one-per-input"];
    let out = user.run("run-batch-no-input", &args).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("at least one input"));
    // a run kind is required, and flags for one run are refused with a fan-out
    let out = user.run("run-bare", &["run"]).await;
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("<tool|workflow|pipeline>"));
    let args = ["run", "workflow", &id, "--one-per-input", "--logs"];
    let out = user.run("run-batch-logs", &args).await;
    assert_eq!(out.status.code(), Some(2));
    Ok(())
}

/// Following a batch exits 0 when every run completed, 1 when any failed (naming the failures),
/// and 3 when some had not finished before the timeout
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn run_batch_follow_exits_by_outcome() -> Result<(), Error> {
    // a user with a ready workflow and files
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "run-batch-follow",
        &generators::readable_name("hebe", &suffix),
        &generators::readable_name("vesta", &suffix),
    )
    .await?;
    let triage = workflow(&user, "Triage").await?;
    let files = samples(&user, 3).await?;
    let id = triage.id().to_string();
    let auth = auth(&user).await?;
    let base = |batch: &'static str| -> Vec<String> {
        let mut args = vec!["run", "workflow", &id, "-g", &user.group];
        for file in &files {
            args.extend(["--sample", file]);
        }
        args.extend([
            "--one-per-input",
            "--batch",
            batch,
            "--wait",
            "--interval",
            "200ms",
        ]);
        args.into_iter().map(str::to_owned).collect()
    };
    // every run completing exits 0 and counts them
    let mut owned = base("all-done");
    owned.extend(["--timeout", "60s"].map(str::to_owned));
    let args: Vec<&str> = owned.iter().map(String::as_str).collect();
    let waiting = user.spawn(&args);
    for run in wait_for_runs(&user, "all-done", 3).await? {
        runs::handle_reaction(&user.client, &auth, &user.group, run.id, "proceed").await?;
    }
    let out = tokio::time::timeout(Duration::from_secs(90), waiting.wait_with_output())
        .await
        .expect("run --wait didn't return")?;
    assert_success(&out);
    let text = stdout(&out);
    assert!(text.contains("3 inputs (3 Completed)"), "{text}");
    // one run failing exits 1, prints JSON lines with each final status, and names the failure
    let mut owned = base("one-failed");
    owned.extend(["--timeout", "60s", "-o", "json"].map(str::to_owned));
    let args: Vec<&str> = owned.iter().map(String::as_str).collect();
    let waiting = user.spawn(&args);
    let started = wait_for_runs(&user, "one-failed", 3).await?;
    let failed = started[0].id;
    for (index, run) in started.iter().enumerate() {
        let cmd = if index == 0 { "fail" } else { "proceed" };
        runs::handle_reaction(&user.client, &auth, &user.group, run.id, cmd).await?;
    }
    let out = tokio::time::timeout(Duration::from_secs(90), waiting.wait_with_output())
        .await
        .expect("run --wait didn't return")?;
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let printed = lines(&out);
    assert_eq!(printed.len(), 3, "{}", stdout(&out));
    let by_status = |status: &str| {
        printed
            .iter()
            .filter(|line| line["status"] == status)
            .count()
    };
    assert_eq!((by_status("Completed"), by_status("Failed")), (2, 1));
    let failing = printed
        .iter()
        .find(|line| line["status"] == "Failed")
        .unwrap();
    assert_eq!(failing["id"], failed.to_string());
    let errors = String::from_utf8_lossy(&out.stderr);
    assert!(errors.contains(&format!("Failed: {failed}")), "{errors}");
    // runs that don't finish before the timeout exit 3 and are listed as unfinished
    let mut owned = base("slow");
    owned.extend(["--timeout", "1s"].map(str::to_owned));
    let args: Vec<&str> = owned.iter().map(String::as_str).collect();
    let out = user.run("run-batch-wait-timeout", &args).await;
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = stdout(&out);
    assert!(text.contains("Unfinished:"), "{text}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("Timed out"));
    Ok(())
}

/// `runs cancel --batch` lists the batch on a dry run, needs `--skip-confirm` without a terminal,
/// and fails every unfinished run of the batch only
#[tokio::test]
async fn runs_cancel_by_batch() -> Result<(), Error> {
    // a user with a ready workflow and two batches of runs
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "run-batch-cancel",
        &generators::readable_name("maia", &suffix),
        &generators::readable_name("ops", &suffix),
    )
    .await?;
    let triage = workflow(&user, "Triage").await?;
    let files = samples(&user, 3).await?;
    let id = triage.id().to_string();
    for batch in ["doomed", "spared"] {
        let args = [
            "run",
            "workflow",
            &id,
            "-g",
            &user.group,
            "--sample",
            &files[0],
            "--sample",
            &files[1],
            "--sample",
            &files[2],
            "--one-per-input",
            "--batch",
            batch,
        ];
        assert_success(&user.run("run-batch-for-cancel", &args).await);
    }
    // a dry run changes nothing
    let args = ["runs", "cancel", "--batch", "doomed", "--dry-run"];
    let out = user.run("runs-cancel-batch-dry", &args).await;
    assert_success(&out);
    assert!(
        stdout(&out).contains("Would fail 3 runs"),
        "{}",
        stdout(&out)
    );
    assert!(
        batch_runs(&user, "doomed")
            .await?
            .iter()
            .all(|run| run.status != ReactionStatus::Failed)
    );
    // without a terminal the confirmation can't be asked
    let out = user
        .run(
            "runs-cancel-batch-confirm",
            &["runs", "cancel", "--batch", "doomed"],
        )
        .await;
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--skip-confirm"));
    // skipping the confirmation fails the batch's runs and leaves the other batch alone
    let args = ["runs", "cancel", "--batch", "doomed", "--skip-confirm"];
    let out = user.run("runs-cancel-batch", &args).await;
    assert_success(&out);
    assert!(
        stdout(&out).contains("Failed 3 of 3 runs"),
        "{}",
        stdout(&out)
    );
    assert!(
        batch_runs(&user, "doomed")
            .await?
            .iter()
            .all(|run| run.status == ReactionStatus::Failed)
    );
    assert!(
        batch_runs(&user, "spared")
            .await?
            .iter()
            .all(|run| run.status != ReactionStatus::Failed)
    );
    // a second cancel finds nothing to do
    let out = user.run("runs-cancel-batch-again", &args).await;
    assert_success(&out);
    assert!(
        stdout(&out).contains("nothing to cancel"),
        "{}",
        stdout(&out)
    );
    Ok(())
}

/// The pipeline kind and the tool kind fan out like the workflow kind
#[tokio::test]
async fn run_batch_covers_pipelines_and_tools() -> Result<(), Error> {
    // a user with a pipeline, a tool, and files
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "run-batch-kinds",
        &generators::readable_name("rhea", &suffix),
        &generators::readable_name("tethys", &suffix),
    )
    .await?;
    let (pipe, _) = user.reaction().await?;
    let (_, image) =
        runs::image_tool(&[user.group.as_str()], ImageScaler::K8s, &user.admin).await?;
    let files = samples(&user, 2).await?;
    // a pipeline fan-out creates one reaction per file
    let args = [
        "run",
        "pipeline",
        &pipe.name,
        "-g",
        &user.group,
        "--sample",
        &files[0],
        "--sample",
        &files[1],
        "--one-per-input",
        "--batch",
        "pipes",
        "-o",
        "json",
    ];
    let out = user.run("run-batch-pipeline", &args).await;
    assert_success(&out);
    assert_eq!(lines(&out).len(), 2, "{}", stdout(&out));
    let runs = batch_runs(&user, "pipes").await?;
    assert_eq!(runs.len(), 2);
    assert!(
        runs.iter()
            .all(|run| run.samples.len() == 1 && run.pipeline == pipe.name)
    );
    // a tool fan-out creates one tool run per file
    let tool = image.id.to_string();
    let args = [
        "run",
        "tool",
        &tool,
        "-g",
        &user.group,
        "--sample",
        &files[0],
        "--sample",
        &files[1],
        "--one-per-input",
        "--batch",
        "tools",
        "-o",
        "json",
    ];
    let out = user.run("run-batch-tool", &args).await;
    assert_success(&out);
    assert_eq!(lines(&out).len(), 2, "{}", stdout(&out));
    let runs = batch_runs(&user, "tools").await?;
    assert_eq!(runs.len(), 2);
    assert!(runs.iter().all(|run| run.samples.len() == 1));
    // a dry run of a tool starts nothing
    let args = [
        "run",
        "tool",
        &tool,
        "--sample",
        &files[0],
        "--batch",
        "toolsdry",
        "--dry-run",
    ];
    let out = user.run("run-batch-tool-dry", &args).await;
    assert_success(&out);
    assert!(
        stdout(&out).contains("would start 1 run of tool"),
        "{}",
        stdout(&out)
    );
    assert!(batch_runs(&user, "toolsdry").await?.is_empty());
    Ok(())
}
