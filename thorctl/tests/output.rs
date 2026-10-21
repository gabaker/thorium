//! Tests the shared output, watch, and wait-for helpers on the existing thorctl views

mod common;

use std::time::Duration;

use common::{TestUser, assert_success, stdout, unique};
use serde_json::Value;
use thorium::Error;
use thorium::client::ResultsClient;
use thorium::models::{OutputDisplayType, OutputRequest, SampleRequest, ScopedTokenRequest};

/// Parse a command's stdout as JSON
///
/// # Arguments
///
/// * `output` - The command's output
fn json(output: &std::process::Output) -> Value {
    serde_json::from_str(&stdout(output)).expect("stdout was not JSON")
}

/// Parse a command's stdout as YAML
///
/// # Arguments
///
/// * `output` - The command's output
fn yaml(output: &std::process::Output) -> Value {
    serde_norway::from_str(&stdout(output)).expect("stdout was not YAML")
}

/// `reactions get` and `reactions describe` print the API's response as JSON, the same
/// document as YAML, and render tables and text
#[tokio::test]
async fn reaction_formats() -> Result<(), Error> {
    // create a user, a pipeline, and a reaction
    let user = TestUser::new("output-reaction-formats").await?;
    let (pipe, reaction) = user.reaction().await?;
    let id = reaction.id.to_string();
    let target = format!("{id}:{}", user.group);
    // get the raw API response for this reaction
    let raw = user
        .raw_get(&format!("/api/reactions/{}/{id}", user.group))
        .await;
    // get --format json prints the API's response
    let args = [
        "reactions",
        "get",
        "-g",
        &user.group,
        &id,
        "--format",
        "json",
    ];
    let out = user.run("reactions-get-json", &args).await;
    assert_success(&out);
    let printed = json(&out);
    assert_eq!(printed, Value::Array(vec![raw.clone()]));
    // get --format yaml prints the same document
    let args = [
        "reactions",
        "get",
        "-g",
        &user.group,
        &id,
        "--format",
        "yaml",
    ];
    let out = user.run("reactions-get-yaml", &args).await;
    assert_success(&out);
    assert_eq!(yaml(&out), printed);
    // describe --format json and yaml print the same document
    let args = ["reactions", "describe", &target, "--format", "json"];
    let out = user.run("reactions-describe-json", &args).await;
    assert_success(&out);
    assert_eq!(json(&out), printed);
    let args = ["reactions", "describe", &target, "--format", "yaml"];
    let out = user.run("reactions-describe-yaml", &args).await;
    assert_success(&out);
    assert_eq!(yaml(&out), printed);
    // describe keeps printing JSON without a format flag
    let out = user
        .run(
            "reactions-describe-default",
            &["reactions", "describe", &target],
        )
        .await;
    assert_success(&out);
    assert_eq!(json(&out), printed);
    // tables and text render the reaction
    for (name, command, format) in [
        ("reactions-get-table", "get", "table"),
        ("reactions-get-human", "get", "human"),
        ("reactions-describe-table", "describe", "table"),
        ("reactions-describe-human", "describe", "human"),
    ] {
        let args: Vec<&str> = if command == "get" {
            vec![
                "reactions",
                "get",
                "-g",
                &user.group,
                &id,
                "--format",
                format,
            ]
        } else {
            vec!["reactions", "describe", &target, "--format", format]
        };
        let out = user.run(name, &args).await;
        assert_success(&out);
        let text = stdout(&out);
        assert!(text.contains(&id), "{name} is missing the reaction id");
        assert!(text.contains(&pipe.name), "{name} is missing the pipeline");
        if format == "table" {
            assert!(text.contains("STATUS"), "{name} has no table header");
        } else {
            assert!(text.contains("Status:"), "{name} has no status field");
        }
    }
    // -o still names the output path, and --format sets what is written there
    let dir = std::env::temp_dir().join(unique("thorctl-describe-"));
    let path = dir.join("reaction.yaml");
    let path_str = path.to_string_lossy().into_owned();
    let args = [
        "reactions",
        "describe",
        &target,
        "-o",
        &path_str,
        "--format",
        "yaml",
    ];
    let out = user.run("reactions-describe-output-path", &args).await;
    assert_success(&out);
    let written: Value = serde_norway::from_str(&std::fs::read_to_string(&path)?).unwrap();
    assert_eq!(written, printed);
    Ok(())
}

/// `reactions describe --condensed` prints the API's response on one line, and the
/// human format puts a rule between several described reactions
#[tokio::test]
async fn reaction_describe_condensed_and_many() -> Result<(), Error> {
    // create a user and a reaction
    let user = TestUser::new("output-reaction-describe-many").await?;
    let (_, reaction) = user.reaction().await?;
    let id = reaction.id.to_string();
    let target = format!("{id}:{}", user.group);
    // get the raw API response for this reaction, as a list like describe prints
    let raw = user
        .raw_get(&format!("/api/reactions/{}/{id}", user.group))
        .await;
    let printed = Value::Array(vec![raw]);
    // --condensed prints the same JSON document on a single line
    let args = ["reactions", "describe", &target, "--condensed"];
    let out = user.run("reactions-describe-condensed", &args).await;
    assert_success(&out);
    assert_eq!(json(&out), printed);
    assert_eq!(stdout(&out).trim_end().lines().count(), 1);
    // human output for several reactions puts a rule between them
    let (_, second) = user.reaction().await?;
    let second_id = second.id.to_string();
    let second_target = format!("{second_id}:{}", user.group);
    let args = [
        "reactions",
        "describe",
        &target,
        &second_target,
        "--format",
        "human",
    ];
    let out = user.run("reactions-describe-human-many", &args).await;
    assert_success(&out);
    let text = stdout(&out);
    assert!(text.contains(&id) && text.contains(&second_id));
    assert_eq!(text.matches(&"─".repeat(60)).count(), 1);
    Ok(())
}

/// `--wait-for` exits 0 once its condition holds and 3 on timeout
#[tokio::test]
async fn reaction_wait_for() -> Result<(), Error> {
    // create a user, a pipeline, and a reaction
    let user = TestUser::new("output-reaction-wait-for").await?;
    let (pipe, reaction) = user.reaction().await?;
    let id = reaction.id.to_string();
    let target = format!("{id}:{}", user.group);
    // a condition that doesn't hold before the timeout exits 3
    let args = [
        "reactions",
        "describe",
        &target,
        "--wait-for",
        "status=Completed",
        "--timeout",
        "2s",
    ];
    let out = user.run("reactions-wait-for-timeout", &args).await;
    assert_eq!(out.status.code(), Some(3), "a timeout should exit 3");
    assert!(String::from_utf8_lossy(&out.stderr).contains("Timed out"));
    assert!(
        stdout(&out).is_empty(),
        "a timeout should print no reactions"
    );
    // start waiting for the reaction to complete
    let args = [
        "reactions",
        "get",
        "-g",
        &user.group,
        &id,
        "--format",
        "json",
        "--wait-for",
        "status=Completed",
        "--timeout",
        "2m",
    ];
    let waiting = user.spawn(&args);
    // complete the reaction while thorctl polls
    tokio::time::sleep(Duration::from_secs(1)).await;
    user.complete(&pipe).await?;
    // thorctl exits 0 and prints the completed reaction once
    let out = tokio::time::timeout(Duration::from_secs(150), waiting.wait_with_output())
        .await
        .expect("--wait-for never returned")?;
    assert_success(&out);
    let printed = json(&out);
    assert_eq!(printed[0]["status"], "Completed");
    assert_eq!(printed[0]["id"], id.as_str());
    Ok(())
}

/// `reactions describe --wait-for` fails fast with a normal error when a listed reaction can't be
/// retrieved, alone or next to one the condition holds for
#[tokio::test]
async fn reaction_wait_for_missing() -> Result<(), Error> {
    // create a user, a pipeline, and a completed reaction
    let user = TestUser::new("output-reaction-wait-for-missing").await?;
    let (pipe, reaction) = user.reaction().await?;
    user.complete(&pipe).await?;
    let done = format!("{}:{}", reaction.id, user.group);
    let missing = format!("{}:{}", uuid::Uuid::new_v4(), user.group);
    // a missing reaction alone and next to a completed one both fail quickly without a timeout
    for (name, targets) in [
        ("reactions-wait-for-missing", vec![missing.as_str()]),
        (
            "reactions-wait-for-done-and-missing",
            vec![done.as_str(), missing.as_str()],
        ),
    ] {
        let mut args = vec!["reactions", "describe"];
        args.extend(targets);
        args.extend(["--wait-for", "status=Completed", "--timeout", "1m"]);
        let start = std::time::Instant::now();
        let out = user.run(name, &args).await;
        assert!(!out.status.success(), "{name} should fail");
        assert_ne!(out.status.code(), Some(3), "{name} should not time out");
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "{name} took {:?}",
            start.elapsed()
        );
        assert!(stdout(&out).is_empty(), "{name} should print no reactions");
    }
    Ok(())
}

/// `--watch` re-reads and re-prints until interrupted
#[tokio::test]
async fn reaction_watch() -> Result<(), Error> {
    // create a user, a pipeline, and a reaction
    let user = TestUser::new("output-reaction-watch").await?;
    let (_, reaction) = user.reaction().await?;
    let id = reaction.id.to_string();
    // watch the reaction with a short interval
    let args = [
        "reactions",
        "get",
        "-g",
        &user.group,
        &id,
        "--format",
        "json",
        "--watch",
        "--interval",
        "200ms",
    ];
    let mut watching = user.spawn(&args);
    // let it re-render a few times, then interrupt it
    tokio::time::sleep(Duration::from_secs(3)).await;
    watching.start_kill()?;
    let out = watching.wait_with_output().await?;
    // every render printed the reaction, so it appears more than once
    let renders = stdout(&out).matches(&format!("\"id\": \"{id}\"")).count();
    assert!(renders >= 2, "--watch rendered {renders} times");
    Ok(())
}

/// `reactions logs --format` prints each stage's API response, and `-o` still names the
/// output directory
#[tokio::test]
async fn reaction_logs() -> Result<(), Error> {
    // create a user and a completed reaction with stage logs
    let user = TestUser::new("output-reaction-logs").await?;
    let (pipe, reaction) = user.reaction().await?;
    user.complete(&pipe).await?;
    let id = reaction.id.to_string();
    let target = format!("{id}:{}", user.group);
    // print the logs as JSON
    let out = user
        .run(
            "reactions-logs-json",
            &["reactions", "logs", &target, "--format", "json"],
        )
        .await;
    assert_success(&out);
    let printed = json(&out);
    let stages = printed[0]["stages"].as_array().expect("no stages").clone();
    assert_ne!(stages.len(), 0, "the logs have no stages");
    // each stage holds the API's response for its logs
    for stage in &stages {
        let name = stage["stage"].as_str().unwrap();
        let raw = user
            .raw_get(&format!(
                "/api/reactions/logs/{}/{id}/{name}?cursor=0&limit=250000",
                user.group
            ))
            .await;
        assert_eq!(stage["logs"], raw["logs"], "stage {name} logs differ");
    }
    // the status logs are the API's response too
    let raw_status = user
        .raw_get(&format!("/api/reactions/logs/{}/{id}", user.group))
        .await;
    assert_eq!(printed[0]["status"], raw_status);
    // yaml prints the same document
    let out = user
        .run(
            "reactions-logs-yaml",
            &["reactions", "logs", &target, "--format", "yaml"],
        )
        .await;
    assert_success(&out);
    assert_eq!(yaml(&out), printed);
    // with -o and --format json, the document is written into the output directory
    let dir = std::env::temp_dir().join(unique("thorctl-logs-"));
    let dir_str = dir.to_string_lossy().into_owned();
    let args = [
        "reactions",
        "logs",
        &target,
        "-o",
        &dir_str,
        "--format",
        "json",
    ];
    let out = user.run("reactions-logs-output-json", &args).await;
    assert_success(&out);
    let written: Value = serde_json::from_str(&std::fs::read_to_string(
        dir.join(&pipe.name).join(format!("{id}.json")),
    )?)
    .unwrap();
    assert_eq!(written, printed[0]);
    // without --format, -o keeps writing the classic per-stage files
    let classic = std::env::temp_dir().join(unique("thorctl-logs-classic-"));
    let classic_str = classic.to_string_lossy().into_owned();
    let args = ["reactions", "logs", &target, "-o", &classic_str];
    let out = user.run("reactions-logs-output-classic", &args).await;
    assert_success(&out);
    let stage = stages[0]["stage"].as_str().unwrap();
    assert!(classic.join(&pipe.name).join(&id).join(stage).is_file());
    // --watch can't be combined with an output directory
    let args = ["reactions", "logs", &target, "-o", &dir_str, "--watch"];
    let out = user.run("reactions-logs-watch-output", &args).await;
    assert!(!out.status.success());
    Ok(())
}

/// `results get --format` prints the API's response for each target without downloading
#[tokio::test]
async fn result_formats() -> Result<(), Error> {
    // create a user, a sample, and a result for it
    let user = TestUser::new("output-result-formats").await?;
    let sample = SampleRequest::new("Cargo.toml", vec![user.group.clone()]);
    let sha256 = user.client.files.create(sample).await?.sha256;
    let tool = unique("tool");
    let result = OutputRequest::new(
        sha256.clone(),
        &tool,
        "a test result",
        OutputDisplayType::String,
    );
    user.client.files.create_result(result).await?;
    // get the raw API response for this sample's results
    let raw = user.raw_get(&format!("/api/files/results/{sha256}")).await;
    // print the results as JSON and YAML, keyed by the sample
    let dir = std::env::temp_dir().join(unique("thorctl-results-"));
    let dir_str = dir.to_string_lossy().into_owned();
    let args = [
        "results", "get", "--files", &sha256, "-o", &dir_str, "--format", "json",
    ];
    let out = user.run("results-get-json", &args).await;
    assert_success(&out);
    let printed = json(&out);
    assert_eq!(printed[&sha256], raw);
    let args = ["results", "get", "--files", &sha256, "--format", "yaml"];
    let out = user.run("results-get-yaml", &args).await;
    assert_success(&out);
    assert_eq!(yaml(&out), printed);
    // printing a format downloads nothing
    assert!(!dir.exists(), "--format should not download results");
    // tables and text render the tool's result
    for format in ["table", "human"] {
        let args = ["results", "get", "--files", &sha256, "--format", format];
        let out = user.run(&format!("results-get-{format}"), &args).await;
        assert_success(&out);
        assert!(stdout(&out).contains(&tool), "{format} is missing the tool");
    }
    // a condition on the tool's results holds right away
    let condition = format!("results.{tool}");
    let args = [
        "results",
        "get",
        "--files",
        &sha256,
        "--format",
        "json",
        "--wait-for",
        &condition,
        "--timeout",
        "30s",
    ];
    let out = user.run("results-get-wait-for", &args).await;
    assert_success(&out);
    // without --format, -o still names the download directory
    let args = ["results", "get", "--files", &sha256, "-o", &dir_str];
    let out = user.run("results-get-download", &args).await;
    assert_success(&out);
    assert!(dir.join(&sha256).join(&tool).exists());
    // the client sees the same result
    let map = user
        .client
        .files
        .get_results(&sha256, &thorium::models::ResultGetParams::default())
        .await?;
    assert!(map.results.contains_key(&tool));
    Ok(())
}

/// No format of the token views prints a token value
#[tokio::test]
async fn token_formats_never_print_tokens() -> Result<(), Error> {
    // create a user and a scoped token
    let user = TestUser::new("output-token-formats").await?;
    let name = unique("token");
    let req = ScopedTokenRequest::new(&name).groups(vec![user.group.clone()]);
    let scoped = user.client.users.create_scoped_token(&req).await?;
    // get the raw API response listing our scoped tokens
    let raw = user.raw_get("/api/users/tokens/").await;
    for format in ["table", "human", "json", "yaml"] {
        for command in ["get", "describe"] {
            // print our tokens in this format
            let args = ["tokens", command, "--format", format];
            let out = user.run(&format!("tokens-{command}-{format}"), &args).await;
            assert_success(&out);
            let text = stdout(&out);
            // the token is shown but its value never is
            assert!(
                text.contains(&name),
                "tokens {command} {format} lost the token"
            );
            assert!(
                !text.contains(&scoped.token),
                "tokens {command} {format} printed a token value"
            );
            // the JSON document is the API's response with each value redacted
            if format == "json" {
                let mut expected = raw.clone();
                for token in expected.as_array_mut().unwrap() {
                    token["token"] = Value::String("<redacted>".to_owned());
                }
                assert_eq!(json(&out), expected);
            }
        }
    }
    Ok(())
}
