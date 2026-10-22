//! Tests the thorium-agent binary against tool runs and workflow runs: the harness launch, the
//! per-run worker's exit, and the CLI wrapper
//!
//! Set `THORIUM_AGENT_BIN` to a thorium-agent built from this workspace to run them.

use std::path::{Path, PathBuf};
use thorium::client::ResultsClient;
use thorium::models::{
    HarnessRunFile, HarnessToolEntry, ImageScaler, Pools, ReactionListParams, ReactionStatus,
    Resources, ResultGetParams, RunInputs, ToolAnnotations, ToolCapabilities, ToolInvocation,
    ToolLoading, ToolMechanism, ToolPortabilityReport, ToolResultFormat, ToolRunRequest,
    ToolStrictness, UserRole, WorkerRegistrationList, WorkflowLimits, WorkflowRunRequest,
};
use thorium::test_utilities::generators::{self, GroupRoles, TestUser};
use thorium::test_utilities::{self, runs};
use thorium::{Error, Thorium, fail, is};
use uuid::Uuid;

/// A developer for every scaler
const DEVELOPER: UserRole = UserRole::Developer {
    k8s: true,
    bare_metal: true,
    windows: true,
    external: true,
    kvm: true,
};

/// Get the thorium-agent binary to run
fn agent_bin() -> Result<String, Error> {
    // the binary is built by the caller
    std::env::var("THORIUM_AGENT_BIN")
        .map_err(|_| Error::new("THORIUM_AGENT_BIN must point at a thorium-agent binary"))
}

/// A runner, the group they own, and an admin client
struct Runner {
    /// The admin client
    admin: Thorium,
    /// The runner
    user: TestUser,
    /// The group the runner owns
    group: String,
}

/// Create a developer named after a moon who owns a group named after a god
///
/// # Arguments
///
/// * `moon` - The moon the runner is named after
/// * `god` - The god the group is named after
async fn runner(moon: &str, god: &str) -> Result<Runner, Error> {
    // a developer for every scaler who owns one group
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let name = generators::readable_name(moon, &suffix);
    let user = generators::named_user_with_role(&admin, &name, DEVELOPER).await?;
    let group = generators::god_group(
        &admin,
        god,
        &suffix,
        &GroupRoles::default().owner(&user.username),
    )
    .await?;
    Ok(Runner { admin, user, group })
}

/// Write a keys file holding a token for the test API in a fresh directory
///
/// # Arguments
///
/// * `admin` - The admin client, for its host
/// * `token` - The token the keys file holds
async fn keys_dir(admin: &Thorium, token: &str) -> Result<PathBuf, Error> {
    // a fresh directory with a keys file in it
    let dir = std::env::temp_dir().join(format!("thorium-agent-runs-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&dir).await?;
    tokio::fs::write(
        dir.join("keys.yml"),
        format!("api: {}\ntoken: {token}\n", admin.host),
    )
    .await?;
    Ok(dir)
}

/// Run a thorium-agent `tool` command with the files in a directory
///
/// # Arguments
///
/// * `dir` - The directory holding the keys file and any run files
/// * `args` - The args after `tool`
async fn tool(dir: &Path, args: &[&str]) -> Result<std::process::Output, Error> {
    // the wrapper reads its keys and run files from the directory
    let output = tokio::process::Command::new(agent_bin()?)
        .arg("tool")
        .args(args)
        .arg("--keys")
        .arg(dir.join("keys.yml"))
        .arg("--run-file")
        .arg(dir.join("run.json"))
        .arg("--tool-runs")
        .arg(dir.join("tool-runs.jsonl"))
        .env_remove("THORIUM_URL")
        .env_remove("THORIUM_TOKEN")
        .output()
        .await?;
    Ok(output)
}

/// A failed workflow run: the harness exits 1, so its job errors, the run fails with the
/// harness's output in its stage logs and no result under its result key, its token is deleted,
/// and its per-run worker exits cleanly on the next claim's `401` without deleting its record
#[tokio::test]
#[ignore = "needs THORIUM_AGENT_BIN, a thorium-agent binary built from this workspace"]
#[allow(clippy::too_many_lines)]
async fn failed_harness_fails_the_run() -> Result<(), Error> {
    // a runner with a ready workflow and a file
    let runner = runner("thebe", "fortuna").await?;
    let admin = &runner.admin;
    generators::node("cluster0", "node0", Resources::default(), admin).await?;
    let groups = [runner.group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, admin).await?;
    let (_, boxed) = runs::container_tool(&groups, image.id, admin).await?;
    let triage =
        runs::ready_workflow("Triage", &groups, &[(&boxed, ToolMechanism::Cli)], admin).await?;
    let sha256 = runner
        .user
        .client
        .files
        .create(generators::gen_sample(&runner.group))
        .await?
        .sha256;
    // the runner starts a workflow run
    let created = runner
        .user
        .client
        .reactions
        .run_workflow(
            triage.id(),
            &WorkflowRunRequest::new(RunInputs::default().sample(&sha256)),
        )
        .await?;
    let run = runner
        .user
        .client
        .reactions
        .get(&runner.group, created.id)
        .await?;
    let token = admin.reactions.run_token(&runner.group, created.id).await?;
    // the scaler would register the run's harness worker and hand it the run token
    let worker = generators::gen_string(24);
    let list = WorkerRegistrationList::default().add(
        "cluster0",
        "node0",
        &worker,
        &runner.user.username,
        &runner.group,
        triage.harness.id,
        Some(created.id),
        Resources::default(),
        Pools::Deadline,
    );
    admin
        .system
        .register_workers(ImageScaler::K8s, &list)
        .await?;
    let dir = keys_dir(admin, &token.token).await?;
    // the harness prints a line to stderr and exits 1
    let output = tokio::process::Command::new(agent_bin()?)
        .args(["--cluster", "cluster0", "--node", "node0"])
        .args(["--group", &runner.group])
        .args(["--tool-instance", &triage.harness.id.to_string()])
        .args(["--name", &worker, "--limbo", "1"])
        .arg("--keys")
        .arg(dir.join("keys.yml"))
        .arg("--trace")
        .arg(dir.join("none.yml"))
        .args(["k8s", "--entrypoint", r#"["/bin/sh"]"#])
        .args(["--cmd", r#"["-c","echo harness gave up >&2; exit 1"]"#])
        .output()
        .await?;
    tokio::fs::remove_dir_all(&dir).await?;
    let printed = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // the per-run worker exited cleanly and never printed its token
    assert!(output.status.success(), "agent failed: {printed}");
    assert!(
        !printed.contains(&token.token),
        "the agent printed its token"
    );
    // the run failed with the harness's output in its stage logs
    let failed = runner
        .user
        .client
        .reactions
        .get(&runner.group, created.id)
        .await?;
    is!(failed.status, ReactionStatus::Failed);
    let stage = &run.plan.stages[0][0].name;
    let logs = runner
        .user
        .client
        .reactions
        .logs(
            &runner.group,
            &created.id,
            stage,
            &ReactionListParams::default(),
        )
        .await?;
    assert!(
        logs.logs
            .iter()
            .any(|line| line.contains("harness gave up")),
        "{:?}",
        logs.logs
    );
    // no result is stored under the run's result key
    let key = failed.result_key().unwrap_or_default().to_owned();
    let results = runner
        .user
        .client
        .files
        .get_results(&sha256, &ResultGetParams::default().tool(&key))
        .await?;
    assert!(!results.results.contains_key(&key), "{results:?}");
    // the run's token is gone
    let gone = admin.reactions.run_token(&runner.group, created.id).await;
    fail!(gone, 404);
    // the worker's record is left for the scaler to clear
    let record = admin.system.get_worker(&worker).await?;
    is!(record.run, Some(created.id));
    generators::delete_worker(&worker, admin).await?;
    Ok(())
}

/// The CLI wrapper with a stale `tools.json` sends a tool run the run token's scope doesn't hold,
/// gets the API's `401`, and creates nothing; `tool status` of a run outside the token's groups is
/// the API's `404`
#[tokio::test]
#[ignore = "needs THORIUM_AGENT_BIN, a thorium-agent binary built from this workspace"]
#[allow(clippy::too_many_lines)]
async fn wrapper_relies_on_the_token_scope() -> Result<(), Error> {
    // a runner with two Image tools and a tool run of the first
    let runner = runner("metis", "janus").await?;
    let admin = &runner.admin;
    let groups = [runner.group.as_str()];
    let (_, first) = runs::image_tool(&groups, ImageScaler::K8s, admin).await?;
    let (_, second) = runs::image_tool(&groups, ImageScaler::K8s, admin).await?;
    let client = &runner.user.client;
    let sha256 = client
        .files
        .create(generators::gen_sample(&runner.group))
        .await?
        .sha256;
    let created = client
        .reactions
        .run_tool(
            first.id,
            &ToolRunRequest::new(RunInputs::default().sample(&sha256)),
        )
        .await?;
    let token = admin.reactions.run_token(&runner.group, created.id).await?;
    // a run dir whose tools.json still lists the second tool
    let dir = keys_dir(admin, &token.token).await?;
    let run_file = HarnessRunFile {
        reaction: created.id,
        samples: vec![sha256.clone()],
        repos: Vec::new(),
        workflow_instance: Uuid::new_v4(),
        digest: "sha256:stale".to_owned(),
        agent: Uuid::new_v4(),
        limits: WorkflowLimits::default(),
        max_result_bytes: 0,
        models: Vec::new(),
    };
    tokio::fs::write(dir.join("run.json"), serde_json::to_string(&run_file)?).await?;
    let entry = HarnessToolEntry {
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
    };
    let tools = std::collections::BTreeMap::from([(second.id, entry)]);
    tokio::fs::write(dir.join("tools.json"), serde_json::to_string(&tools)?).await?;
    // the wrapper sends the run and the API refuses it
    let before = client
        .reactions
        .list_sub(&runner.group, &created.id)
        .exec()
        .await?;
    let second_id = second.id.to_string();
    let output = tool(&dir, &["run", &second_id, "--input", "{}"]).await?;
    let printed = String::from_utf8_lossy(&output.stdout).to_string();
    is!(output.status.code(), Some(1));
    let value: serde_json::Value = serde_json::from_str(printed.trim())?;
    let error = value["error"].as_str().unwrap_or_default().to_owned();
    assert!(error.contains("401"), "{printed}");
    assert!(error.contains("isn't one of this run's tools"), "{printed}");
    assert!(
        !printed.contains(&token.token),
        "the wrapper printed its token"
    );
    // no sub run was created
    let after = client
        .reactions
        .list_sub(&runner.group, &created.id)
        .exec()
        .await?;
    is!(after.names.len(), before.names.len());
    // a run in a group the token doesn't hold is the API's 404
    let suffix = generators::name_suffix();
    let other = generators::god_group(
        admin,
        "vesta",
        &suffix,
        &GroupRoles::default().owner(&runner.user.username),
    )
    .await?;
    let other_sha = client
        .files
        .create(generators::gen_sample(&other))
        .await?
        .sha256;
    let (_, outside_tool) = runs::image_tool(&[other.as_str()], ImageScaler::K8s, admin).await?;
    let outside = client
        .reactions
        .run_tool(
            outside_tool.id,
            &ToolRunRequest::new(RunInputs::default().sample(&other_sha)),
        )
        .await?;
    let outside_id = outside.id.to_string();
    for cmd in ["status", "results"] {
        let output = tool(&dir, &[cmd, &outside_id]).await?;
        let printed = String::from_utf8_lossy(&output.stdout).to_string();
        is!(output.status.code(), Some(1));
        assert!(printed.contains("404"), "{cmd}: {printed}");
    }
    tokio::fs::remove_dir_all(&dir).await?;
    Ok(())
}
