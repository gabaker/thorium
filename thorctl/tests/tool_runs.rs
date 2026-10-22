//! Tests the thorctl `tools run` and `tools runs` commands against the test API

mod common;

use common::{TestUser, assert_success, stdout};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use thorium::client::ResultsClient;
use thorium::models::{
    HarnessToolEntry, ImageScaler, OnDiskFile, OutputDisplayType, OutputRequest, ReactionStatus,
    RunInputs, SampleRequest, ToolAnnotations, ToolCapabilities, ToolInvocation, ToolLoading,
    ToolPortabilityReport, ToolResultFormat, ToolRunRequest, ToolStrictness,
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

/// Parse the one JSON object a tool run command printed
///
/// # Arguments
///
/// * `output` - The command's output
fn object(output: &std::process::Output) -> Value {
    // the command prints exactly one line of JSON
    let printed = stdout(output);
    assert_eq!(printed.trim().lines().count(), 1, "{printed}");
    serde_json::from_str(printed.trim()).expect("stdout was not one JSON object")
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

/// `tools run` without waiting prints the run, `tools runs status` hints when to poll an
/// unfinished run and shows a finished one's final status, `tools run --wait` downloads the
/// results, `tools runs results` prints the same object, and an unknown run is the API's `404`
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn tools_run_and_runs_commands() -> Result<(), Error> {
    // a user with an Image tool and a file
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "tools-run",
        &generators::readable_name("leda", &suffix),
        &generators::readable_name("pomona", &suffix),
    )
    .await?;
    let (_, image) =
        runs::image_tool(&[user.group.as_str()], ImageScaler::K8s, &user.admin).await?;
    let sha256 = sample(&user).await?;
    let id = image.id.to_string();
    // start a run without waiting
    let args = ["tools", "run", &id, "--sample", &sha256];
    let out = user.run("tools-run-no-wait", &args).await;
    assert_success(&out);
    let started = object(&out);
    let reaction = Uuid::parse_str(started["reaction"].as_str().expect("a reaction id"))
        .expect("a reaction id");
    assert_eq!(started["status"], "Created");
    assert_eq!(started["result"], Value::Null);
    // its status hints when to check it again
    let reaction_id = reaction.to_string();
    let args = ["tools", "runs", "status", &reaction_id];
    let out = user.run("tools-runs-status-running", &args).await;
    assert_success(&out);
    let status = object(&out);
    assert_eq!(status["status"], "Created");
    assert!(status["retry_after_secs"].is_u64(), "{status}");
    // once completed its status is final
    complete(&user, reaction, &sha256).await?;
    let out = user.run("tools-runs-status-done", &args).await;
    assert_success(&out);
    let status = object(&out);
    assert_eq!(status["status"], "Completed");
    assert_eq!(status["retry_after_secs"], Value::Null);
    // a waiting run downloads its results once its worker completes it
    let wait_dir = std::env::temp_dir().join(common::unique("tools-run-wait-"));
    let wait_out = wait_dir.to_string_lossy().into_owned();
    let args = [
        "tools", "run", &id, "--sample", &sha256, "--wait", "--output", &wait_out,
    ];
    let waiting = user.spawn(&args);
    // find the new run and complete it
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let second = loop {
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
            .find(|found| *found != reaction);
        if let Some(found) = found {
            break found;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "tools run never created a run"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    complete(&user, second, &sha256).await?;
    let out = tokio::time::timeout(Duration::from_secs(90), waiting.wait_with_output())
        .await
        .expect("tools run --wait didn't return")?;
    user.record("tools-run-wait", &args, &out);
    assert_success(&out);
    let waited_line = stdout(&out);
    let waited = object(&out);
    assert_eq!(waited["status"], "Completed");
    assert_eq!(waited["reaction"], second.to_string());
    assert_eq!(waited["result"], serde_json::json!({"functions": 412}));
    let file = wait_dir.join("report.txt");
    assert_eq!(waited["files"], serde_json::json!([file.to_string_lossy()]));
    assert_eq!(std::fs::read_to_string(&file)?, "harvested");
    // results of the same run print the same object
    let second_id = second.to_string();
    let args = [
        "tools", "runs", "results", &second_id, "--output", &wait_out,
    ];
    let out = user.run("tools-runs-results", &args).await;
    assert_success(&out);
    assert_eq!(stdout(&out), waited_line);
    std::fs::remove_dir_all(&wait_dir)?;
    // an unknown run is the API's 404 for both commands
    let missing = Uuid::new_v4().to_string();
    for (name, cmd) in [
        ("tools-runs-status-missing", "status"),
        ("tools-runs-results-missing", "results"),
    ] {
        let args = ["tools", "runs", cmd, &missing];
        let out = user.run(name, &args).await;
        assert!(!out.status.success(), "{name} succeeded");
        let error = object(&out)["error"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(error.contains("404") && error.contains(&missing), "{error}");
    }
    Ok(())
}

/// Build a `tools.json` entry for a tool bound to the CLI wrapper
fn tools_entry() -> HarnessToolEntry {
    // a container tool with no limits
    HarnessToolEntry {
        function_name: "harvest".to_owned(),
        title: None,
        model_description: "Harvest a file".to_owned(),
        input_schema: serde_json::Map::new(),
        output_schema: None,
        input_examples: Vec::new(),
        strict: ToolStrictness::Auto,
        loading: ToolLoading::Eager,
        icons: Vec::new(),
        capabilities: ToolCapabilities::default(),
        annotations: ToolAnnotations::default(),
        portability: ToolPortabilityReport::default(),
        result_format: ToolResultFormat::Json,
        requires_approval: false,
        max_result_bytes: 0,
        timeout: None,
        invocation: ToolInvocation::Cli {
            command: vec!["/opt/thorium/thorium-agent".to_owned(), "tool".to_owned()],
        },
    }
}

/// Run thorctl with a keys file in a working directory
///
/// # Arguments
///
/// * `keys` - The keys file to authenticate with
/// * `cwd` - The directory to run in
/// * `args` - The thorctl arguments after the global flags
async fn thorctl_in(keys: &Path, cwd: &Path, args: &[&str]) -> std::process::Output {
    // run the thorctl binary with the keys file, giving up if it hangs
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_thorctl"));
    command
        .arg("--keys")
        .arg(keys)
        .arg("--skip-update")
        .args(args)
        .current_dir(cwd)
        .env("NO_PROXY", "127.0.0.1,localhost,::1")
        .env("no_proxy", "127.0.0.1,localhost,::1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(120), command.output())
        .await
        .expect("thorctl timed out")
        .expect("failed to run thorctl")
}

/// `tools run` with a run token relies on the API's token scope alone: a tool instance outside
/// the scope is sent and refused with the API's `401` even when `tools.json` lists only the other
/// one, a tool instance in the scope is run, and the results are the same without `tools.json`
#[tokio::test]
async fn tools_run_relies_on_the_token_scope() -> Result<(), Error> {
    // a user with two Image tools, a file, and a tool run of the first
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "tools-run-scope",
        &generators::readable_name("kale", &suffix),
        &generators::readable_name("faunus", &suffix),
    )
    .await?;
    let groups = [user.group.as_str()];
    let (_, first) = runs::image_tool(&groups, ImageScaler::K8s, &user.admin).await?;
    let (_, second) = runs::image_tool(&groups, ImageScaler::K8s, &user.admin).await?;
    let sha256 = sample(&user).await?;
    let created = user
        .client
        .reactions
        .run_tool(
            first.id,
            &ToolRunRequest::new(RunInputs::default().sample(&sha256)),
        )
        .await?;
    let token = user
        .admin
        .reactions
        .run_token(&user.group, created.id)
        .await?;
    // a keys file holding the run token and a working dir whose tools.json lists only the first
    let dir: PathBuf = std::env::temp_dir().join(common::unique("tools-run-scope-"));
    std::fs::create_dir_all(&dir)?;
    let keys = dir.join("keys.yml");
    std::fs::write(
        &keys,
        format!("api: {}\ntoken: {}\n", user.admin.host, token.token),
    )?;
    let tools = std::collections::BTreeMap::from([(first.id, tools_entry())]);
    std::fs::write(dir.join("tools.json"), serde_json::to_string(&tools)?)?;
    let first_id = first.id.to_string();
    let second_id = second.id.to_string();
    for with_tools in [true, false] {
        if !with_tools {
            std::fs::remove_file(dir.join("tools.json"))?;
        }
        // the second tool is sent and the API refuses it
        let out = thorctl_in(
            &keys,
            &dir,
            &["tools", "run", &second_id, "--sample", &sha256],
        )
        .await;
        assert!(
            !out.status.success(),
            "tools run of the second tool succeeded"
        );
        let error = object(&out)["error"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(error.contains("401"), "{error}");
        assert!(error.contains("isn't one of this run's tools"), "{error}");
        assert!(
            !stdout(&out).contains(&token.token),
            "thorctl printed the token"
        );
        // the first tool runs
        let out = thorctl_in(
            &keys,
            &dir,
            &["tools", "run", &first_id, "--sample", &sha256],
        )
        .await;
        assert_success(&out);
        let run = object(&out);
        assert!(run["reaction"].is_string(), "{run}");
        assert_eq!(run["error"], Value::Null);
    }
    std::fs::remove_dir_all(&dir)?;
    Ok(())
}

/// `thorium-agent tool results` prints byte for byte what `thorctl tools runs results` prints for
/// the same finished run
///
/// Set `THORIUM_AGENT_BIN` to a thorium-agent built from this workspace to run it.
#[tokio::test]
#[ignore = "needs THORIUM_AGENT_BIN, a thorium-agent binary built from this workspace"]
async fn tools_run_matches_the_wrapper() -> Result<(), Error> {
    // a user with an Image tool and a finished tool run
    let agent = std::env::var("THORIUM_AGENT_BIN")
        .map_err(|_| Error::new("THORIUM_AGENT_BIN must point at a thorium-agent binary"))?;
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "tools-run-wrapper",
        &generators::readable_name("carme", &suffix),
        &generators::readable_name("flora", &suffix),
    )
    .await?;
    let (_, image) =
        runs::image_tool(&[user.group.as_str()], ImageScaler::K8s, &user.admin).await?;
    let sha256 = sample(&user).await?;
    let created = user
        .client
        .reactions
        .run_tool(
            image.id,
            &ToolRunRequest::new(RunInputs::default().sample(&sha256)),
        )
        .await?;
    complete(&user, created.id, &sha256).await?;
    // both print the results of the run into the same directory
    let dir = std::env::temp_dir().join(common::unique("tools-run-wrapper-"));
    let out_dir = dir.join("out").to_string_lossy().into_owned();
    let reaction = created.id.to_string();
    let args = [
        "tools",
        "runs",
        "results",
        &reaction,
        "--group",
        &user.group,
        "--output",
        &out_dir,
    ];
    let thorctl = user.run("tools-runs-results-wrapper", &args).await;
    assert_success(&thorctl);
    let printed = object(&thorctl);
    assert_eq!(printed["status"], "Completed");
    assert_eq!(printed["files"].as_array().map(Vec::len), Some(1));
    let wrapper = tokio::process::Command::new(&agent)
        .args(["tool", "results", &reaction, "--output", &out_dir])
        .arg("--keys")
        .arg(&user.keys)
        .arg("--run-file")
        .arg(dir.join("run.json"))
        .arg("--tool-runs")
        .arg(dir.join("tool-runs.jsonl"))
        .env_remove("THORIUM_URL")
        .env_remove("THORIUM_TOKEN")
        .output()
        .await?;
    assert_eq!(stdout(&thorctl), String::from_utf8_lossy(&wrapper.stdout));
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
