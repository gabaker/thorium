//! Tests the shared run flags of `thorctl tools run` against the test API

mod common;

use common::{TestUser, assert_success, stdout};
use serde_json::Value;
use std::time::Duration;
use thorium::client::ResultsClient;
use thorium::models::{
    ImageScaler, OnDiskFile, OutputDisplayType, OutputRequest, ReactionStatus, SampleRequest,
};
use thorium::test_utilities::{self, generators, runs};
use thorium::{Error, Thorium};
use uuid::Uuid;

/// Upload a random file to the user's group and get its SHA256
///
/// # Arguments
///
/// * `user` - The test user
async fn sample(user: &TestUser) -> Result<String, Error> {
    // upload a random file
    let req: SampleRequest = generators::gen_sample(&user.group);
    Ok(user.client.files.create(req).await?.sha256)
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

/// Create a user with an Image tool and a file to run it on
///
/// Returns the user, the tool's name, its first instance's id, and the file's SHA256.
///
/// # Arguments
///
/// * `claim` - The name of the test
async fn fixture(claim: &str) -> Result<(TestUser, String, String, String), Error> {
    // a readable user and group, an Image tool, and a file
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        claim,
        &generators::readable_name("leda", &suffix),
        &generators::readable_name("pomona", &suffix),
    )
    .await?;
    let (tool, instance) =
        runs::image_tool(&[user.group.as_str()], ImageScaler::K8s, &user.admin).await?;
    let sha256 = sample(&user).await?;
    Ok((user, tool.name, instance.id.to_string(), sha256))
}

/// Parse the one JSON object a tool run command printed
///
/// # Arguments
///
/// * `output` - The command's output
fn object(output: &std::process::Output) -> Value {
    // the command prints exactly one document
    serde_json::from_str(stdout(output).trim()).expect("stdout was not one JSON document")
}

/// Wait for a run that isn't one of the known ones to be created
///
/// # Arguments
///
/// * `user` - The test user
/// * `known` - The runs that already exist
async fn new_run(user: &TestUser, known: &[Uuid]) -> Result<Uuid, Error> {
    // poll the group's created reactions for a new one
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let cursor = user
            .client
            .reactions
            .list_group(&user.group, &ReactionStatus::Created)
            .exec()
            .await?;
        let found = cursor
            .names
            .iter()
            .filter_map(|name| Uuid::parse_str(name).ok())
            .find(|found| !known.contains(found));
        if let Some(found) = found {
            return Ok(found);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "tools run never created a run"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Complete a tool run the way its worker would: store a result with one file under the run's
/// key, then proceed the run
///
/// # Arguments
///
/// * `user` - The test user
/// * `reaction` - The tool run's reaction
/// * `sha256` - The file the run ran on
async fn complete(user: &TestUser, reaction: Uuid, sha256: &str) -> Result<(), Error> {
    // the run stores its results under its key
    let run = user.client.reactions.get(&user.group, reaction).await?;
    let key = run
        .result_key()
        .expect("a tool run has a result key")
        .to_owned();
    let dir = std::env::temp_dir().join(common::unique("tool-run-result-"));
    std::fs::create_dir_all(&dir)?;
    let file = dir.join("report.txt");
    std::fs::write(&file, "harvested")?;
    let req = OutputRequest::new(
        sha256.to_owned(),
        &key,
        "{\"functions\": 412}",
        OutputDisplayType::Json,
    )
    .file(OnDiskFile::new(&file).trim_prefix(&dir));
    user.client.files.create_result(req).await?;
    std::fs::remove_dir_all(&dir)?;
    // the run completes
    let auth = auth(user).await?;
    runs::handle_reaction(&user.client, &auth, &user.group, reaction, "proceed").await
}

/// Fail a tool run the way its worker would
///
/// # Arguments
///
/// * `user` - The test user
/// * `reaction` - The tool run's reaction
async fn fail(user: &TestUser, reaction: Uuid) -> Result<(), Error> {
    // the worker reports the failure
    let auth = auth(user).await?;
    runs::handle_reaction(&user.client, &auth, &user.group, reaction, "fail").await
}

/// `tools run` runs a bare tool name's latest version, and puts its tags, SLA, and parent on the
/// run's reaction and its job args on the job
#[tokio::test]
async fn tools_run_tags_sla_parent_and_args() -> Result<(), Error> {
    // a tool, a file, and a first plain run
    let (user, tool, _, sha256) = fixture("tools-run-flags").await?;
    let args = ["tools", "run", &tool, "--sample", &sha256];
    let out = user.run("tools-run-bare-name", &args).await;
    assert_success(&out);
    let first: Uuid = object(&out)["reaction"].as_str().unwrap().parse()?;
    // a second run carries every flag
    let parent = first.to_string();
    let args = [
        "tools",
        "run",
        &tool,
        "--sample",
        &sha256,
        "--tag",
        "one",
        "--tag",
        "two",
        "--sla",
        "900",
        "--parent",
        &parent,
        "--arg",
        "p",
        "--kwarg",
        "mode=fast",
        "--switch",
        "quiet",
    ];
    let out = user.run("tools-run-flags", &args).await;
    assert_success(&out);
    let second: Uuid = object(&out)["reaction"].as_str().unwrap().parse()?;
    let run = user.client.reactions.get(&user.group, second).await?;
    assert!(run.tags.contains(&"one".to_owned()) && run.tags.contains(&"two".to_owned()));
    assert_eq!(run.parent, Some(first));
    let until = (run.sla - chrono::Utc::now()).num_seconds();
    assert!((600..=900).contains(&until), "the SLA is {until}s away");
    let job = run.args.values().next().expect("the run has job args");
    assert_eq!(job.positionals, vec!["p"]);
    assert_eq!(job.kwargs["mode"], vec!["fast"]);
    assert_eq!(job.switches, vec!["quiet"]);
    // a keyword arg needs a value, and nothing is run
    let args = [
        "tools", "run", &tool, "--sample", &sha256, "--kwarg", "novalue",
    ];
    let out = user.run("tools-run-bad-kwarg", &args).await;
    assert_eq!(out.status.code(), Some(2));
    assert!(
        object(&out)["error"]
            .as_str()
            .unwrap()
            .contains("KEY=VALUE")
    );
    Ok(())
}

/// `tools run --wait` exits 0 when the run completes, 1 when it fails and 3 when the wait times
/// out, and `--output` alone waits and downloads the results
#[tokio::test]
async fn tools_run_wait_exit_codes() -> Result<(), Error> {
    // a tool and a file
    let (user, _, id, sha256) = fixture("tools-run-wait-codes").await?;
    // a wait that times out exits 3 and the run keeps going
    let args = [
        "tools",
        "run",
        &id,
        "--sample",
        &sha256,
        "--wait",
        "--timeout",
        "1s",
    ];
    let out = user.run("tools-run-wait-timeout", &args).await;
    assert_eq!(out.status.code(), Some(3), "{}", stdout(&out));
    assert!(
        object(&out)["error"]
            .as_str()
            .unwrap()
            .contains("timed out")
    );
    let timed_out = new_run(&user, &[]).await?;
    // a run that fails exits 1
    let args = ["tools", "run", &id, "--sample", &sha256, "--wait"];
    let failing = user.spawn(&args);
    let failed = new_run(&user, &[timed_out]).await?;
    fail(&user, failed).await?;
    let out = tokio::time::timeout(Duration::from_secs(90), failing.wait_with_output())
        .await
        .expect("tools run --wait didn't return")?;
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert_eq!(object(&out)["status"], "Failed");
    // --output waits by itself and downloads what a completed run made
    let dir = std::env::temp_dir().join(common::unique("tools-run-output-"));
    let dir_arg = dir.to_string_lossy().into_owned();
    let args = [
        "tools", "run", &id, "--sample", &sha256, "--output", &dir_arg,
    ];
    let waiting = user.spawn(&args);
    let completed = new_run(&user, &[timed_out, failed]).await?;
    complete(&user, completed, &sha256).await?;
    let out = tokio::time::timeout(Duration::from_secs(90), waiting.wait_with_output())
        .await
        .expect("tools run --output didn't return")?;
    assert_success(&out);
    assert_eq!(object(&out)["status"], "Completed");
    assert_eq!(
        std::fs::read_to_string(dir.join("report.txt"))?,
        "harvested"
    );
    std::fs::remove_dir_all(&dir)?;
    Ok(())
}

/// `tools run` prints the run like `workflows run` does with `--logs`, `--wait-for` and a format
/// for a waited run, with the same exit codes
#[tokio::test]
async fn tools_run_shared_view() -> Result<(), Error> {
    // a tool and a file
    let (user, _, id, sha256) = fixture("tools-run-view").await?;
    // a condition that is never met times out with exit 3
    let args = [
        "tools",
        "run",
        &id,
        "--sample",
        &sha256,
        "--wait-for",
        "status=Completed",
        "--timeout",
        "1s",
    ];
    let out = user.run("tools-run-wait-for-timeout", &args).await;
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let first = new_run(&user, &[]).await?;
    // a waited run with a format prints the run as a document, once it completes
    let args = [
        "tools", "run", &id, "--sample", &sha256, "--wait", "-o", "json",
    ];
    let waiting = user.spawn(&args);
    let second = new_run(&user, &[first]).await?;
    complete(&user, second, &sha256).await?;
    let out = tokio::time::timeout(Duration::from_secs(90), waiting.wait_with_output())
        .await
        .expect("tools run --wait didn't return")?;
    assert_success(&out);
    let run = object(&out);
    assert_eq!(run["status"], "Completed");
    assert_eq!(run["id"], second.to_string());
    // a run with --logs that fails exits 1 after printing the run
    let args = ["tools", "run", &id, "--sample", &sha256, "--logs"];
    let failing = user.spawn(&args);
    let third = new_run(&user, &[first, second]).await?;
    fail(&user, third).await?;
    let out = tokio::time::timeout(Duration::from_secs(90), failing.wait_with_output())
        .await
        .expect("tools run --logs didn't return")?;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stdout(&out).contains(&third.to_string()),
        "{}",
        stdout(&out)
    );
    Ok(())
}

/// `tools runs status` prints the run through the shared view with `--wait-for` and a format, and
/// `tools runs logs` prints the run's logs
#[tokio::test]
async fn tools_runs_status_view_and_logs() -> Result<(), Error> {
    // a started run
    let (user, _, id, sha256) = fixture("tools-runs-view").await?;
    let args = ["tools", "run", &id, "--sample", &sha256];
    let out = user.run("tools-runs-view-start", &args).await;
    assert_success(&out);
    let run = object(&out)["reaction"].as_str().unwrap().to_owned();
    // a condition that isn't met yet times out with exit 3
    let args = [
        "tools",
        "runs",
        "status",
        &run,
        "--wait-for",
        "status=Completed",
        "--timeout",
        "1s",
    ];
    let out = user.run("tools-runs-status-timeout", &args).await;
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // once the run completes the condition holds and the run prints as a document
    complete(&user, run.parse()?, &sha256).await?;
    let args = [
        "tools",
        "runs",
        "status",
        &run,
        "--wait-for",
        "status=Completed",
        "-o",
        "json",
    ];
    let out = user.run("tools-runs-status-wait-for", &args).await;
    assert_success(&out);
    assert_eq!(object(&out)["status"], "Completed");
    assert_eq!(object(&out)["id"], run.as_str());
    // a format alone prints the run too, and without any flag the one JSON object stays
    let args = ["tools", "runs", "status", &run, "-o", "yaml"];
    let out = user.run("tools-runs-status-yaml", &args).await;
    assert_success(&out);
    assert!(
        stdout(&out).contains("status: Completed"),
        "{}",
        stdout(&out)
    );
    let out = user
        .run(
            "tools-runs-status-plain",
            &["tools", "runs", "status", &run],
        )
        .await;
    assert_success(&out);
    assert_eq!(object(&out)["status"], "Completed");
    assert!(object(&out).get("retry_after_secs").is_some());
    // the run's logs print as text and as a document
    let out = user
        .run("tools-runs-logs", &["tools", "runs", "logs", &run])
        .await;
    assert_success(&out);
    let out = user
        .run(
            "tools-runs-logs-json",
            &["tools", "runs", "logs", &run, "-o", "json"],
        )
        .await;
    assert_success(&out);
    assert!(object(&out).is_object() || object(&out).is_array());
    // an unknown run is the API's refusal
    let missing = Uuid::new_v4().to_string();
    let out = user
        .run(
            "tools-runs-logs-missing",
            &["tools", "runs", "logs", &missing],
        )
        .await;
    assert!(!out.status.success());
    Ok(())
}
