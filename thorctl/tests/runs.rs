//! Tests the thorctl workflow run, runs, and results show commands against the test API

mod common;

use common::{TestUser, assert_success, json, stdout};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::time::Duration;
use thorium::client::ResultsClient;
use thorium::models::{
    ImageScaler, OutputDisplayType, OutputRequest, PipelineRequest, ReactionCursorOpts,
    ReactionRequest, ReactionStatus, RunInputs, SampleRequest, ScopedTokenRequest, ToolMechanism,
    ToolRunRequest,
};
use thorium::test_utilities::runs::{self, RunWorkflow};
use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium};
use uuid::Uuid;

/// Create a ready workflow in some groups whose agent calls one container tool
///
/// # Arguments
///
/// * `user` - The test user
/// * `name` - The workflow's name
/// * `groups` - The groups to create the workflow in
async fn workflow(user: &TestUser, name: &str, groups: &[&str]) -> Result<RunWorkflow, Error> {
    // an Image tool and a container tool that runs it
    let (_, image) = runs::image_tool(groups, ImageScaler::K8s, &user.admin).await?;
    let (_, boxed) = runs::container_tool(groups, image.id, &user.admin).await?;
    runs::ready_workflow(name, groups, &[(&boxed, ToolMechanism::Cli)], &user.admin).await
}

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

/// Get the ids of a workflow instance's runs in the user's group
///
/// # Arguments
///
/// * `user` - The test user
/// * `instance` - The workflow instance
async fn instance_runs(user: &TestUser, instance: Uuid) -> Result<Vec<String>, Error> {
    // runs are tagged with their instance
    let opts = ReactionCursorOpts::default().group(&user.group);
    let cursor = user
        .client
        .reactions
        .list_tag_new(&format!("WorkflowInstance={instance}"), &opts)
        .await?;
    Ok(cursor.data)
}

/// Parse the reaction id thorctl printed
///
/// # Arguments
///
/// * `output` - The command's output
fn printed_id(output: &std::process::Output) -> Uuid {
    // the only line is the reaction id
    Uuid::parse_str(stdout(output).trim()).expect("thorctl didn't print a reaction id")
}

/// `runs describe` prints every hash and image of a workflow run's provenance matching its
/// JSON, a tool run's tool instance version, hash, and image, and a pipeline reaction's stage
/// tool instances
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn runs_describe_provenance() -> Result<(), Error> {
    // a user with a ready workflow calling a container tool, and a file
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "runs-describe-provenance",
        &generators::readable_name("kale", &suffix),
        &generators::readable_name("faunus", &suffix),
    )
    .await?;
    let groups = [user.group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &user.admin).await?;
    let (_, boxed) = runs::container_tool(&groups, image.id, &user.admin).await?;
    let triage = Box::pin(runs::ready_workflow(
        "Triage",
        &groups,
        &[(&boxed, ToolMechanism::Cli)],
        &user.admin,
    ))
    .await?;
    let sha256 = sample(&user).await?;
    let inputs = RunInputs::default().sample(&sha256);
    // a workflow run prints its digest and every agent and tool hash and image
    let created = user
        .client
        .reactions
        .run_workflow(
            triage.id(),
            &thorium::models::WorkflowRunRequest::new(inputs.clone()),
        )
        .await?;
    let run_id = created.id.to_string();
    let args = ["runs", "describe", &run_id];
    let out = user.run("runs-describe-provenance-workflow", &args).await;
    assert_success(&out);
    let text = stdout(&out);
    let args = ["runs", "describe", &run_id, "-o", "json"];
    let described = json(&user.run("runs-describe-provenance-json", &args).await);
    let source = &described["source"]["WorkflowRun"];
    let provenance = &source["provenance"];
    let digest = source["digest"].as_str().unwrap_or_default();
    assert_ne!(digest, "");
    assert!(
        text.contains("Provenance"),
        "Provenance missing from {text}"
    );
    assert!(text.contains(digest), "{digest} missing from {text}");
    let agents = provenance["agents"].as_array().cloned().unwrap_or_default();
    assert!(!agents.is_empty(), "no agents in {provenance}");
    for agent in &agents {
        for field in ["agent", "config_hash", "toolset_digest"] {
            let value = agent[field].as_str().unwrap_or_default();
            assert!(!value.is_empty(), "{field} empty in {agent}");
            assert!(text.contains(value), "{field} {value} missing from {text}");
        }
    }
    let tools = provenance["tools"].as_array().cloned().unwrap_or_default();
    assert_eq!(tools.len(), 3, "{provenance}");
    for tool in &tools {
        for field in ["tool_instance", "spec_hash", "version"] {
            let value = tool[field].as_str().unwrap_or_default();
            assert!(!value.is_empty(), "{field} empty in {tool}");
            assert!(text.contains(value), "{field} {value} missing from {text}");
        }
        if let Some(image) = tool["image"].as_str() {
            assert!(text.contains(image), "{image} missing from {text}");
        }
    }
    // the harness is marked
    let harness = triage.harness.id.to_string();
    let harness_line = text
        .lines()
        .find(|line| line.contains(&harness) && line.contains("harness"))
        .unwrap_or_default();
    assert!(!harness_line.is_empty(), "harness unmarked in {text}");
    // a tool run prints its tool instances' versions, hashes, and images
    let created = user
        .client
        .reactions
        .run_tool(boxed.id, &ToolRunRequest::new(inputs.clone()))
        .await?;
    let tool_run = user.client.reactions.get(&user.group, created.id).await?;
    let tool_id = tool_run.id.to_string();
    let args = ["runs", "describe", &tool_id];
    let out = user.run("runs-describe-provenance-tool", &args).await;
    assert_success(&out);
    let text = stdout(&out);
    assert!(
        text.contains("Provenance"),
        "Provenance missing from {text}"
    );
    let recorded = &tool_run.source.provenance().tools;
    assert_eq!(recorded.len(), 2);
    for tool in recorded {
        for value in [
            tool.tool_instance.to_string(),
            tool.version.clone(),
            tool.spec_hash.clone(),
        ] {
            assert!(text.contains(&value), "{value} missing from {text}");
        }
    }
    let pinned = recorded
        .iter()
        .find(|tool| tool.tool_instance == image.id)
        .and_then(|tool| tool.image.clone())
        .expect("the pinned image's reference");
    assert!(text.contains(&pinned), "{pinned} missing from {text}");
    // a pipeline reaction prints each stage's tool instance
    let pipeline = generators::gen_string(20).to_lowercase();
    let req = PipelineRequest::new(&user.group, &pipeline, serde_json::json!([["harvest"]]))
        .stage_tool("harvest", image.id);
    user.client.pipelines.create(&req).await?;
    let created = user
        .client
        .reactions
        .create(&ReactionRequest::new(&user.group, &pipeline))
        .await?;
    let reaction_id = created.id.to_string();
    let args = ["runs", "describe", &reaction_id];
    let out = user.run("runs-describe-provenance-pipeline", &args).await;
    assert_success(&out);
    let text = stdout(&out);
    let section = text.split("Provenance").nth(1).unwrap_or_default();
    let stage = image.id.to_string();
    assert!(section.contains(&stage), "{stage} missing from {text}");
    let spec_hash = match &image.metadata {
        thorium::models::EntityMetadata::ToolInstance(meta) => meta.spec_hash.clone(),
        _ => String::new(),
    };
    assert!(
        section.contains(&spec_hash),
        "{spec_hash} missing from {text}"
    );
    Ok(())
}

/// `tokens get` and `tokens describe` show a run token's run, tool instances, and expiry in
/// every format without its value, and show no run for an ordinary scoped token
#[tokio::test]
async fn tokens_show_run_scope() -> Result<(), Error> {
    // a user with an Image tool, a file, a tool run, and an ordinary scoped token
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "runs-tokens",
        &generators::readable_name("aitne", &suffix),
        &generators::readable_name("liber", &suffix),
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
    let token = user
        .admin
        .reactions
        .run_token(&user.group, created.id)
        .await?;
    let plain = format!("plain-{suffix}");
    user.client
        .users
        .create_scoped_token(&ScopedTokenRequest::new(&plain).group(&user.group))
        .await?;
    // every format shows the run and its tool but never the token's value
    let run_name = format!("run-{}", created.id);
    for args in [
        vec!["tokens", "get"],
        vec!["tokens", "get", "--format", "json"],
        vec!["tokens", "get", "--format", "yaml"],
        vec!["tokens", "describe", &run_name],
        vec!["tokens", "describe", &run_name, "--format", "human"],
    ] {
        let output = user.run("tokens-run-scope", &args).await;
        assert_success(&output);
        let printed = stdout(&output);
        assert!(
            printed.contains(&image.id.to_string()),
            "{args:?}: {printed}"
        );
        assert!(
            printed.matches(&created.id.to_string()).count() >= 2,
            "{args:?}: {printed}"
        );
        assert!(
            !printed.contains(&token.token),
            "{args:?} printed the token"
        );
    }
    // the run token's expiry is shown and the ordinary token has no run section
    let output = user
        .run(
            "tokens-run-scope",
            &["tokens", "describe", "--format", "json"],
        )
        .await;
    assert_success(&output);
    let described = json(&output);
    let tokens = described.as_array().cloned().unwrap_or_default();
    let run = tokens
        .iter()
        .find(|token| token["name"] == run_name.as_str())
        .expect("the run token is described");
    assert_eq!(run["run"]["reaction"], created.id.to_string());
    assert!(run["expires"].is_string());
    let ordinary = tokens
        .iter()
        .find(|token| token["name"] == plain.as_str())
        .expect("the plain token is described");
    assert!(ordinary.get("run").is_none());
    Ok(())
}

/// `workflows run --wait-for status=Completed` waits for the run, `runs describe` shows its
/// source, plan, and result key, `results show` shows its results under that key, and a failed
/// run shows no results and is retried by `runs retry`
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn workflows_run_describe_retry_results() -> Result<(), Error> {
    // a user with a ready workflow and a file
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "runs-describe-retry",
        &generators::readable_name("io", &suffix),
        &generators::readable_name("vulcan", &suffix),
    )
    .await?;
    let triage = workflow(&user, "Triage", &[&user.group]).await?;
    let sha256 = sample(&user).await?;
    let auth = auth(&user).await?;
    // start a run and wait for it to complete while it's completed as its worker would
    let id = triage.id().to_string();
    let args = [
        "workflows",
        "run",
        &id,
        "--sample",
        &sha256,
        "--wait-for",
        "status=Completed",
        "--timeout",
        "60s",
        "--interval",
        "200ms",
        "-o",
        "json",
    ];
    let waiting = user.spawn(&args);
    // find the run once it's created and complete it
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let run = loop {
        if let Some(run) = instance_runs(&user, triage.id()).await?.pop() {
            break Uuid::parse_str(&run).expect("a reaction id");
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "workflows run never created a run"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    runs::handle_reaction(&user.client, &auth, &user.group, run, "proceed").await?;
    let out = tokio::time::timeout(Duration::from_secs(90), waiting.wait_with_output())
        .await
        .expect("workflows run didn't return")?;
    assert_success(&out);
    let waited = json(&out);
    assert_eq!(waited["id"], Value::String(run.to_string()));
    assert_eq!(waited["status"], Value::String("Completed".to_owned()));
    // store the run's result the way its worker does, under the instance's name and id
    let key = format!("Triage#r1~{}", triage.id());
    let req = OutputRequest::new(
        sha256.clone(),
        key.as_str(),
        "{\"answer\": \"benign\"}",
        OutputDisplayType::Json,
    );
    user.client.files.create_result(req).await?;
    // describe shows the run's source, plan, and result key
    let run_id = run.to_string();
    let args = ["runs", "describe", &run_id, "-o", "json"];
    let described = json(&user.run("runs-describe-json", &args).await);
    let source = &described["source"]["WorkflowRun"];
    assert_eq!(source["result_key"], Value::String(key.clone()));
    assert_eq!(source["workflow_instance"], Value::String(id.clone()));
    assert_eq!(
        described["plan"]["stages"][0][0]["tool_instance"],
        Value::String(triage.harness.id.to_string())
    );
    let args = ["runs", "describe", &run_id];
    let out = user.run("runs-describe", &args).await;
    assert_success(&out);
    let text = stdout(&out);
    for expected in [
        "workflow run",
        "Result key",
        key.as_str(),
        "Harness",
        "Plan",
    ] {
        assert!(text.contains(expected), "{expected} missing from {text}");
    }
    // describe shows the model the run's agent resolved to
    let model = format!(
        "{} -> {} (OpenAiChat) at Group on group endpoint {}, keyless",
        triage.agent.id,
        runs::TEST_MODEL,
        triage.endpoint.id
    );
    assert!(text.contains("Models"), "Models missing from {text}");
    assert!(text.contains(&model), "{model} missing from {text}");
    assert_eq!(
        source["models"][0]["model_id"],
        Value::String(runs::TEST_MODEL.to_owned())
    );
    // results show prints the run's results on its file under its key
    let args = ["results", "show", &run_id, "--format", "json"];
    let shown = json(&user.run("results-show-completed", &args).await);
    assert_eq!(
        shown[&sha256]["results"][key.as_str()]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    // a second run fails
    let args = ["workflows", "run", &id, "--sample", &sha256];
    let out = user.run("workflows-run-failing", &args).await;
    assert_success(&out);
    let failed = printed_id(&out);
    runs::handle_reaction(&user.client, &auth, &user.group, failed, "fail").await?;
    let failed_id = failed.to_string();
    // results show still finds the first run's result under the shared key
    let args = ["results", "show", &failed_id, "--format", "json"];
    let shown = json(&user.run("results-show-failed", &args).await);
    assert!(shown.get(&sha256).is_some(), "{shown}");
    // runs retry starts a new run linked to the failed one
    let args = ["runs", "retry", &failed_id];
    let out = user.run("runs-retry", &args).await;
    assert_success(&out);
    let retried = user
        .client
        .reactions
        .get(&user.group, printed_id(&out))
        .await?;
    assert_eq!(retried.parent, Some(failed));
    assert_eq!(retried.status, ReactionStatus::Created);
    // runs get shows each run's key as the instance's name, without the id it carries
    let args = ["runs", "get", "-g", &user.group];
    let out = user.run("runs-get-table", &args).await;
    assert_success(&out);
    let table = stdout(&out);
    assert!(table.contains("Triage#r1"), "{table}");
    assert!(!table.contains('~'), "{table}");
    // runs get lists the three runs
    let args = ["runs", "get", "-g", &user.group, "-o", "json"];
    let listed = json(&user.run("runs-get", &args).await);
    let mut ids = listed.as_array().map_or_else(Vec::new, |runs| {
        runs.iter()
            .filter_map(|run| run["id"].as_str().map(ToOwned::to_owned))
            .collect::<Vec<String>>()
    });
    ids.sort();
    let mut expected = vec![run_id, failed_id, retried.id.to_string()];
    expected.sort();
    assert_eq!(ids, expected);
    Ok(())
}

/// `workflows run` without `--group` uses an instance's one runnable group, prints the API's
/// refusal listing several, and runs in the group `--group` names
#[tokio::test]
async fn workflows_run_groups() -> Result<(), Error> {
    // a user who owns a second group, with a workflow in one group and one in both
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "runs-groups",
        &generators::readable_name("europa", &suffix),
        &generators::readable_name("juno", &suffix),
    )
    .await?;
    let second = generators::god_group(
        &user.admin,
        "ceres",
        &suffix,
        &generators::GroupRoles::default().owner(&user.username),
    )
    .await?;
    let single = workflow(&user, "Single", &[&user.group]).await?;
    let both = workflow(&user, "Both", &[&user.group, &second]).await?;
    let sha256 = sample(&user).await?;
    // the one runnable group is used
    let id = single.id().to_string();
    let args = ["workflows", "run", &id, "--sample", &sha256];
    let out = user.run("workflows-run-single-group", &args).await;
    assert_success(&out);
    let run = user
        .client
        .reactions
        .get(&user.group, printed_id(&out))
        .await?;
    assert_eq!(run.group, user.group);
    // several runnable groups print the API's refusal listing them
    let id = both.id().to_string();
    let args = ["workflows", "run", &id, "--sample", &sha256];
    let out = user.run("workflows-run-several-groups", &args).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("400") && stderr.contains(&user.group) && stderr.contains(&second),
        "{stderr}"
    );
    // a named group is used
    let args = [
        "workflows",
        "run",
        &id,
        "--group",
        &second,
        "--sample",
        &sha256,
    ];
    let out = user.run("workflows-run-named-group", &args).await;
    assert_success(&out);
    let run = user.client.reactions.get(&second, printed_id(&out)).await?;
    assert_eq!(run.group, second);
    Ok(())
}

/// `workflows run --file` uploads the file and runs on its SHA256, `--repo` takes a url with or
/// without a commitish, a run without inputs prints the API's refusal, and a missing file is
/// refused before anything is created
#[tokio::test]
async fn workflows_run_inputs() -> Result<(), Error> {
    // a user with a workflow, a repo, and a local file
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "runs-inputs",
        &generators::readable_name("ganymede", &suffix),
        &generators::readable_name("minerva", &suffix),
    )
    .await?;
    let triage = workflow(&user, "Triage", &[&user.group]).await?;
    let repo = generators::repos(&user.group, 1, &user.client)
        .await?
        .remove(0);
    let dir = std::env::temp_dir().join(common::unique("thorctl-run-"));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("input.bin");
    let contents = generators::gen_string(256);
    std::fs::write(&path, &contents)?;
    let expected_sha = format!("{:x}", Sha256::digest(contents.as_bytes()));
    let id = triage.id().to_string();
    let path_arg = path.to_string_lossy().to_string();
    // a local file is uploaded and run on, and the run's reaction id is printed
    let args = ["workflows", "run", &id, "--file", &path_arg];
    let out = user.run("workflows-run-file", &args).await;
    assert_success(&out);
    let run = user
        .client
        .reactions
        .get(&user.group, printed_id(&out))
        .await?;
    assert_eq!(run.samples, vec![expected_sha]);
    // repos parse with and without a commitish
    let pinned = format!("{}:main", repo.url);
    let args = [
        "workflows",
        "run",
        &id,
        "--repo",
        &pinned,
        "--repo",
        &repo.url,
    ];
    let out = user.run("workflows-run-repos", &args).await;
    assert_success(&out);
    let run = user
        .client
        .reactions
        .get(&user.group, printed_id(&out))
        .await?;
    assert_eq!(run.repos.len(), 2);
    assert_eq!(run.repos[0].url, repo.url);
    assert_eq!(run.repos[0].commitish.as_deref(), Some("main"));
    // a run without inputs prints the API's refusal
    let before = instance_runs(&user, triage.id()).await?.len();
    let args = ["workflows", "run", &id];
    let out = user.run("workflows-run-no-inputs", &args).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("400") && stderr.contains("at least one sample or repo"),
        "{stderr}"
    );
    // a missing local file is refused before anything is created
    let missing = dir.join("missing.bin").to_string_lossy().to_string();
    let args = ["workflows", "run", &id, "--file", &missing];
    let out = user.run("workflows-run-missing-file", &args).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("missing.bin"), "{stderr}");
    assert_eq!(instance_runs(&user, triage.id()).await?.len(), before);
    Ok(())
}
