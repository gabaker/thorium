//! Tests the thorctl pipelines commands against the test API

mod common;

use std::collections::HashSet;
use std::process::{Output, Stdio};
use std::time::Duration;

use common::editor::{EditorScript, recovery_path};
use common::{TestUser, assert_success, json, stdout, unique};
use serde_json::{Value, json};
use thorium::models::{
    Entity, EntityMetadata, GroupRequest, GroupUsersRequest, Pipeline, PipelineRequest,
    ReactionRequest,
};
use thorium::test_utilities::{generators, runs};
use thorium::{Error, Thorium};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

/// The environment that lets thorctl open an editor script without a terminal
const EDIT_ENV: [(&str, &str); 1] = [("THORCTL_EDIT_ASSUME_TTY", "1")];

/// Run thorctl to completion with extra environment variables and record its transcript
///
/// # Arguments
///
/// * `user` - The user to run as
/// * `name` - The name of the transcript
/// * `args` - The thorctl arguments after the global flags
/// * `envs` - The environment variables to set
async fn run_with(user: &TestUser, name: &str, args: &[&str], envs: &[(&str, &str)]) -> Output {
    // set the variables and run, giving up if it hangs
    let mut command = user.command(args);
    command.envs(envs.iter().copied());
    let output = tokio::time::timeout(Duration::from_secs(120), command.output())
        .await
        .expect("thorctl timed out")
        .expect("failed to run thorctl");
    user.record(name, args, &output);
    output
}

/// Run thorctl with text piped to its stdin
///
/// # Arguments
///
/// * `user` - The user to run as
/// * `args` - The thorctl arguments after the global flags
/// * `input` - The text to write to stdin
async fn run_with_stdin(user: &TestUser, args: &[&str], input: &str) -> Output {
    // start thorctl with a pipe for stdin
    let mut child = user
        .command(args)
        .stdin(Stdio::piped())
        .spawn()
        .expect("failed to start thorctl");
    child
        .stdin
        .take()
        .expect("no stdin")
        .write_all(input.as_bytes())
        .await
        .expect("failed to write stdin");
    child.wait_with_output().await.expect("thorctl failed")
}

/// The text a command wrote to stderr
///
/// # Arguments
///
/// * `output` - The command's output
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Create a tool with one version in some groups
///
/// # Arguments
///
/// * `user` - The test user
/// * `groups` - The groups the tool is in
async fn tool(user: &TestUser, groups: &[&str]) -> Result<(Entity, Entity), Error> {
    // an Image tool with its first instance at 1.0.0
    generators::tool(groups, "1.0.0", &generators::gen_image_spec(), &user.admin).await
}

/// Get a short unique pipeline name
fn pipeline_name() -> String {
    // names are kept short for readable transcripts
    unique("pipe")[..20].to_owned()
}

/// Read a pipeline as the user
///
/// # Arguments
///
/// * `user` - The test user
/// * `name` - The pipeline's name
async fn stored(user: &TestUser, name: &str) -> Result<Pipeline, Error> {
    user.client.pipelines.get(&user.group, name).await
}

/// Create a two stage pipeline whose stages follow tools
///
/// # Arguments
///
/// * `user` - The test user
/// * `name` - The pipeline's name
/// * `first` - The tool of stage `fetch`
/// * `second` - The tool of stage `scan`
async fn two_stages(
    user: &TestUser,
    name: &str,
    first: &Entity,
    second: &Entity,
) -> Result<(), Error> {
    // fetch runs first, then scan
    let req = PipelineRequest::new(&user.group, name, json!([["fetch"], ["scan"]]))
        .sla(3600)
        .description("before")
        .stage_tool("fetch", first.id)
        .stage_tool("scan", second.id);
    user.client.pipelines.create(&req).await?;
    Ok(())
}

/// Get the id a printed reaction line holds
///
/// # Arguments
///
/// * `output` - The command's output
fn printed_id(output: &Output) -> Uuid {
    // the only line is the reaction id
    Uuid::parse_str(stdout(output).trim()).expect("thorctl didn't print a reaction id")
}

/// `pipelines stage-tool` sets a stage's tool to follow its latest version or pins a version,
/// prints the API's refusals, and converts a pipeline so reactions can run on it
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn pipelines_stage_tool() -> Result<(), Error> {
    // a pipeline whose stages have no tools and a tool in the user's group
    let user = TestUser::new("pipelines-stage-tool").await?;
    let (tool, instance) = generators::tool(
        &[&user.group],
        "1.0.0",
        &generators::gen_image_spec(),
        &user.admin,
    )
    .await?;
    let (_, pinned) = generators::tool(
        &[&user.group],
        "1.0.0",
        &generators::gen_image_spec(),
        &user.admin,
    )
    .await?;
    let name = unique("pipe")[..20].to_owned();
    let req = PipelineRequest::new(
        &user.group,
        &name,
        serde_json::json!([["plant"], ["harvest"]]),
    )
    .stage_tool("plant", instance.id)
    .stage_tool("harvest", instance.id);
    user.client.pipelines.create(&req).await?;
    // setting a stage to the tool follows its latest version
    let args = [
        "pipelines",
        "stage-tool",
        &name,
        "plant",
        &tool.name,
        "-g",
        &user.group,
    ];
    assert_success(&user.run("pipelines-stage-tool-latest", &args).await);
    let read = user.client.pipelines.get(&user.group, &name).await?;
    assert_eq!(read.stage_tools.get("plant"), Some(&tool.id));
    // pinning a stage sets it to the tool instance
    let EntityMetadata::ToolInstance(pinned_meta) = &pinned.metadata else {
        panic!("expected a tool instance");
    };
    let pinned_tool = pinned_meta.tool.to_string();
    let pinned_id = pinned.id.to_string();
    let args = [
        "pipelines",
        "stage-tool",
        &name,
        "harvest",
        &pinned_tool,
        "--pin",
        &pinned_id,
        "-g",
        &user.group,
    ];
    assert_success(&user.run("pipelines-stage-tool-pin", &args).await);
    let read = user.client.pipelines.get(&user.group, &name).await?;
    assert_eq!(read.stage_tools.get("harvest"), Some(&pinned.id));
    // a stage that isn't in the pipeline prints the API's refusal
    let args = [
        "pipelines",
        "stage-tool",
        &name,
        "missing",
        &tool.name,
        "-g",
        &user.group,
    ];
    let out = user.run("pipelines-stage-tool-missing-stage", &args).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("aren't in the order: `missing`"),
        "{stderr}"
    );
    // a tool the user can see outside the pipeline's group prints the API's refusal
    let elsewhere = unique("ctlother");
    user.admin
        .groups
        .create(
            &GroupRequest::new(&elsewhere).owners(
                GroupUsersRequest::default()
                    .direct("thorium")
                    .direct(&user.username),
            ),
        )
        .await?;
    let (outside, _) = generators::tool(
        &[&elsewhere],
        "1.0.0",
        &generators::gen_image_spec(),
        &user.admin,
    )
    .await?;
    let outside_id = outside.id.to_string();
    let args = [
        "pipelines",
        "stage-tool",
        &name,
        "plant",
        &outside_id,
        "-g",
        &user.group,
    ];
    let out = user.run("pipelines-stage-tool-outside", &args).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("is not in the pipeline's group"),
        "{stderr}"
    );
    // with every stage set a reaction runs on the pipeline
    user.client
        .reactions
        .create(&ReactionRequest::new(&user.group, &name))
        .await?;
    Ok(())
}

/// `pipelines create` builds a pipeline from stages and tools: steps keep their order, commas run
/// stages together, a bare tool follows its latest version and `TOOL@VERSION` pins one
#[tokio::test]
async fn pipelines_create_from_flags() -> Result<(), Error> {
    // two tools in the user's group
    let user = TestUser::new("pipelines-create").await?;
    let (scan, _) = tool(&user, &[&user.group]).await?;
    let (sort, sort_v1) = tool(&user, &[&user.group]).await?;
    let name = pipeline_name();
    // fetch runs first, then scan and hash together
    let fetch_tool = format!("fetch={}", scan.name);
    let scan_tool = format!("scan={}@1.0.0", sort.name);
    let hash_tool = format!("hash={}", scan.name);
    let args = [
        "pipelines",
        "create",
        &name,
        "-g",
        &user.group,
        "--stage",
        "fetch",
        "--stage",
        "scan,hash",
        "--stage-tool",
        &fetch_tool,
        "--stage-tool",
        &scan_tool,
        "--stage-tool",
        &hash_tool,
        "--sla",
        "100",
        "-d",
        "a test pipeline",
        "--trigger",
        "t=NewSample",
    ];
    let out = user.run("pipelines-create", &args).await;
    assert_success(&out);
    assert!(
        stdout(&out).contains("Created pipeline"),
        "{}",
        stdout(&out)
    );
    // the order, grouping, tools, settings and trigger are all stored
    let read = stored(&user, &name).await?;
    assert_eq!(
        read.order,
        vec![
            vec!["fetch".to_owned()],
            vec!["scan".to_owned(), "hash".to_owned()]
        ]
    );
    assert_eq!(read.stage_tools.get("fetch"), Some(&scan.id));
    assert_eq!(read.stage_tools.get("hash"), Some(&scan.id));
    assert_eq!(read.stage_tools.get("scan"), Some(&sort_v1.id));
    assert_eq!(read.sla, 100);
    assert_eq!(read.description.as_deref(), Some("a test pipeline"));
    assert!(read.triggers.contains_key("t"));
    // creating it again prints the API's refusal and exits 1
    let out = user.run("pipelines-create-twice", &args).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("409"), "{}", stderr(&out));
    // -o json prints the created pipeline
    let other = pipeline_name();
    let args = [
        "pipelines",
        "create",
        &other,
        "-g",
        &user.group,
        "--stage",
        "only",
        "--stage-tool",
        &format!("only={}", scan.name),
        "-o",
        "json",
    ];
    let out = user.run("pipelines-create-json", &args).await;
    assert_success(&out);
    let created = json(&out);
    assert_eq!(created["name"], other.as_str());
    assert_eq!(created["stage_tools"]["only"], scan.id.to_string());
    Ok(())
}

/// `pipelines create` refuses a stage without a tool, an unknown tool or version, and an empty
/// pipeline before anything is created, and `--dry-run` creates nothing
#[tokio::test]
async fn pipelines_create_refusals() -> Result<(), Error> {
    // a tool in the user's group
    let user = TestUser::new("pipelines-create-refused").await?;
    let (scan, _) = tool(&user, &[&user.group]).await?;
    let name = pipeline_name();
    let scan_tool = format!("a={}", scan.name);
    // a stage with no tool is refused client-side, naming the stage
    let args = [
        "pipelines",
        "create",
        &name,
        "-g",
        &user.group,
        "--stage",
        "a,b",
        "--stage-tool",
        &scan_tool,
    ];
    let out = user.run("pipelines-create-no-tool", &args).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("no tool is set for: b"),
        "{}",
        stderr(&out)
    );
    assert!(stored(&user, &name).await.is_err());
    // an unknown tool is refused naming the stage and the group
    let args = [
        "pipelines",
        "create",
        &name,
        "-g",
        &user.group,
        "--stage",
        "a",
        "--stage-tool",
        "a=nosuchtool",
    ];
    let out = user.run("pipelines-create-unknown-tool", &args).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("tool 'nosuchtool' was not found"),
        "{}",
        stderr(&out)
    );
    // an unknown version lists the versions the tool has
    let unknown_version = format!("a={}@9.9.9", scan.name);
    let args = [
        "pipelines",
        "create",
        &name,
        "-g",
        &user.group,
        "--stage",
        "a",
        "--stage-tool",
        &unknown_version,
    ];
    let out = user.run("pipelines-create-unknown-version", &args).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("(it has: 1.0.0)"), "{}", stderr(&out));
    // a pipeline with no stages is refused
    let args = ["pipelines", "create", &name, "-g", &user.group];
    let out = user.run("pipelines-create-empty", &args).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("at least one stage"),
        "{}",
        stderr(&out)
    );
    // a dry run shows the pipeline and creates nothing
    let args = [
        "pipelines",
        "create",
        &name,
        "-g",
        &user.group,
        "--stage",
        "a",
        "--stage-tool",
        &scan_tool,
        "--dry-run",
    ];
    let out = user.run("pipelines-create-dry-run", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("Dry run"), "{}", stdout(&out));
    assert!(stdout(&out).contains(&scan.name), "{}", stdout(&out));
    assert!(stored(&user, &name).await.is_err());
    Ok(())
}

/// `pipelines create --spec` reads a document from a file or stdin, and flags extend or replace
/// what it holds
#[tokio::test]
async fn pipelines_create_from_spec() -> Result<(), Error> {
    // two tools in the user's group
    let user = TestUser::new("pipelines-create-spec").await?;
    let (scan, _) = tool(&user, &[&user.group]).await?;
    let (sort, sort_v1) = tool(&user, &[&user.group]).await?;
    let spec = format!(
        "description: from a spec\nsla: 500\norder: [[fetch], [scan]]\nstage_tools:\n  fetch: {{tool: {}}}\n  scan: {{tool: {}, version: 1.0.0}}\n",
        scan.name, sort.name
    );
    // a spec on stdin creates the pipeline
    let name = pipeline_name();
    let args = [
        "pipelines",
        "create",
        &name,
        "-g",
        &user.group,
        "--spec",
        "-",
    ];
    let out = run_with_stdin(&user, &args, &spec).await;
    assert_success(&out);
    let read = stored(&user, &name).await?;
    assert_eq!(read.sla, 500);
    assert_eq!(read.description.as_deref(), Some("from a spec"));
    assert_eq!(read.stage_tools.get("fetch"), Some(&scan.id));
    assert_eq!(read.stage_tools.get("scan"), Some(&sort_v1.id));
    // a spec file with flags: the flags replace the SLA and a stage's tool
    let path = std::env::temp_dir().join(unique("pipeline-spec-"));
    std::fs::write(&path, &spec)?;
    let path_arg = path.to_string_lossy().into_owned();
    let other = pipeline_name();
    let replaced = format!("scan={}", scan.name);
    let args = [
        "pipelines",
        "create",
        &other,
        "-g",
        &user.group,
        "--spec",
        &path_arg,
        "--sla",
        "7",
        "--stage-tool",
        &replaced,
    ];
    assert_success(&user.run("pipelines-create-spec-flags", &args).await);
    let read = stored(&user, &other).await?;
    assert_eq!(read.sla, 7);
    assert_eq!(read.stage_tools.get("scan"), Some(&scan.id));
    // a spec with a typo is refused before anything is created
    std::fs::write(&path, "order: [[a]]\nstage_tool: {}\n")?;
    let third = pipeline_name();
    let args = [
        "pipelines",
        "create",
        &third,
        "-g",
        &user.group,
        "--spec",
        &path_arg,
    ];
    let out = user.run("pipelines-create-bad-spec", &args).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("Invalid pipeline spec"),
        "{}",
        stderr(&out)
    );
    assert!(stored(&user, &third).await.is_err());
    std::fs::remove_file(&path)?;
    Ok(())
}

/// `pipelines update` changes the SLA, description, order, triggers and stage tools from flags
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn pipelines_update_flags() -> Result<(), Error> {
    // a pipeline of two stages and a third tool for a new stage
    let user = TestUser::new("pipelines-update").await?;
    let (first, _) = tool(&user, &[&user.group]).await?;
    let (second, second_v1) = tool(&user, &[&user.group]).await?;
    let (third, _) = tool(&user, &[&user.group]).await?;
    let name = pipeline_name();
    two_stages(&user, &name, &first, &second).await?;
    let target = ["pipelines", "update", &name, "-g", &user.group];
    // scalar flags and a trigger
    let args = [
        target.as_slice(),
        &["--sla", "99", "-d", "after", "--add-trigger", "t=NewSample"],
    ]
    .concat();
    let out = user.run("pipelines-update-scalars", &args).await;
    assert_success(&out);
    assert!(
        stdout(&out).contains("updated successfully"),
        "{}",
        stdout(&out)
    );
    let read = stored(&user, &name).await?;
    assert_eq!(read.sla, 99);
    assert_eq!(read.description.as_deref(), Some("after"));
    assert!(read.triggers.contains_key("t"));
    // removing the trigger and clearing the description
    let args = [
        target.as_slice(),
        &["--remove-trigger", "t", "--clear-description"],
    ]
    .concat();
    assert_success(&user.run("pipelines-update-clear", &args).await);
    let read = stored(&user, &name).await?;
    assert!(read.triggers.is_empty());
    assert_eq!(read.description, None);
    // a pinned stage tool, then back to following the tool
    let pinned = format!("scan={}@1.0.0", second.name);
    let args = [target.as_slice(), &["--stage-tool", &pinned]].concat();
    assert_success(&user.run("pipelines-update-pin", &args).await);
    assert_eq!(read_tool(&user, &name, "scan").await?, second_v1.id);
    let latest = format!("scan={}", second.name);
    let args = [target.as_slice(), &["--stage-tool", &latest]].concat();
    assert_success(&user.run("pipelines-update-unpin", &args).await);
    assert_eq!(read_tool(&user, &name, "scan").await?, second.id);
    // a new stage with its tool, in the same update that changes the order
    let added = format!("report={}", third.name);
    let args = [
        target.as_slice(),
        &["--order", "fetch,scan;report", "--stage-tool", &added],
    ]
    .concat();
    assert_success(&user.run("pipelines-update-new-stage", &args).await);
    let read = stored(&user, &name).await?;
    assert_eq!(
        read.order,
        vec![
            vec!["fetch".to_owned(), "scan".to_owned()],
            vec!["report".to_owned()]
        ]
    );
    assert_eq!(read.stage_tools.get("report"), Some(&third.id));
    // dropping a stage drops its tool too
    let args = [
        target.as_slice(),
        &["--order", "fetch,scan", "--remove-stage-tool", "report"],
    ]
    .concat();
    assert_success(&user.run("pipelines-update-drop-stage", &args).await);
    let read = stored(&user, &name).await?;
    assert_eq!(read.order.len(), 1);
    assert!(!read.stage_tools.contains_key("report"));
    // a new stage without a tool is refused client-side and nothing changes
    let args = [target.as_slice(), &["--order", "fetch,scan;report"]].concat();
    let out = user.run("pipelines-update-no-tool", &args).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("no tool is set for: report"),
        "{}",
        stderr(&out)
    );
    assert_eq!(stored(&user, &name).await?.order.len(), 1);
    // an update with nothing to do is a usage error
    let out = user.run("pipelines-update-nothing", &target).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("Nothing to update"),
        "{}",
        stderr(&out)
    );
    // the same value again changes nothing
    let args = [target.as_slice(), &["--sla", "99"]].concat();
    let out = user.run("pipelines-update-same", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("No changes"), "{}", stdout(&out));
    Ok(())
}

/// Get the id a stage of a pipeline runs
///
/// # Arguments
///
/// * `user` - The test user
/// * `name` - The pipeline's name
/// * `stage` - The stage
async fn read_tool(user: &TestUser, name: &str, stage: &str) -> Result<Uuid, Error> {
    // read the pipeline and its stage's tool
    let pipeline = stored(user, name).await?;
    pipeline
        .stage_tools
        .get(stage)
        .copied()
        .ok_or_else(|| Error::new(format!("stage {stage} has no tool")))
}

/// `pipelines update` takes `--file`, `--patch`, `--set` and `--unset`, shows a diff, honors
/// `--dry-run`, prints a JSON result and refuses unknown fields before any change
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn pipelines_update_documents() -> Result<(), Error> {
    // a pipeline of two stages and a third tool
    let user = TestUser::new("pipelines-update-docs").await?;
    let (first, _) = tool(&user, &[&user.group]).await?;
    let (second, _) = tool(&user, &[&user.group]).await?;
    let (third, _) = tool(&user, &[&user.group]).await?;
    let name = pipeline_name();
    two_stages(&user, &name, &first, &second).await?;
    let target = ["pipelines", "update", &name, "-g", &user.group];
    // --set changes a stage's tool by name and shows the change
    let set = format!("stage_tools.scan.tool={}", third.name);
    let args = [target.as_slice(), &["--set", &set]].concat();
    let out = user.run("pipelines-update-set", &args).await;
    assert_success(&out);
    assert_eq!(read_tool(&user, &name, "scan").await?, third.id);
    // a dry run changes nothing
    let args = [target.as_slice(), &["--set", "sla=1", "--dry-run"]].concat();
    let out = user.run("pipelines-update-dry-run", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("Dry run"), "{}", stdout(&out));
    assert_eq!(stored(&user, &name).await?.sla, 3600);
    // --patch and --unset
    let args = [
        target.as_slice(),
        &["--patch", r#"{"sla": 12}"#, "--unset", "description"],
    ]
    .concat();
    assert_success(&user.run("pipelines-update-patch", &args).await);
    let read = stored(&user, &name).await?;
    assert_eq!(read.sla, 12);
    assert_eq!(read.description, None);
    // --file - replaces the document, adding a stage with its tool
    let doc = json!({
        "description": "from a file",
        "sla": 30,
        "order": [["fetch"], ["scan"], ["report"]],
        "stage_tools": {
            "fetch": {"tool": first.name},
            "scan": {"tool": second.name},
            "report": {"tool": third.name, "version": "1.0.0"},
        },
    });
    let args = [target.as_slice(), &["--file", "-"]].concat();
    let out = run_with_stdin(&user, &args, &doc.to_string()).await;
    assert_success(&out);
    let read = stored(&user, &name).await?;
    assert_eq!(read.order.len(), 3);
    assert_eq!(read.description.as_deref(), Some("from a file"));
    assert_eq!(read.stage_tools.get("scan"), Some(&second.id));
    assert_ne!(read.stage_tools.get("report"), Some(&third.id));
    // the same document again says there is nothing to change
    let out = run_with_stdin(&user, &args, &doc.to_string()).await;
    assert!(stdout(&out).contains("No changes"), "{}", stdout(&out));
    // -o json prints the documented result
    let args = [target.as_slice(), &["--set", "sla=31", "-o", "json"]].concat();
    let out = user.run("pipelines-update-json", &args).await;
    assert_success(&out);
    let result = json(&out);
    assert_eq!(result["kind"], "pipeline");
    assert_eq!(result["status"], "updated");
    assert_eq!(result["changed"], true);
    assert_eq!(result["diff"][0]["path"], "sla");
    assert_eq!(result["resource"]["sla"], 31);
    assert_eq!(
        result["resource"]["stage_tools"]["fetch"]["tool"],
        first.name
    );
    // fields the view doesn't have are refused before any change
    for bad in ["group=other", "stage_tool.x=1"] {
        let args = [target.as_slice(), &["--set", bad]].concat();
        let out = user.run("pipelines-update-unknown-field", &args).await;
        assert_eq!(out.status.code(), Some(1), "{bad}");
        assert!(stderr(&out).contains("unknown field"), "{}", stderr(&out));
    }
    assert_eq!(stored(&user, &name).await?.sla, 31);
    // a tool that does not exist is refused with the stage named
    let args = [
        target.as_slice(),
        &["--set", "stage_tools.scan.tool=nosuchtool"],
    ]
    .concat();
    let out = user.run("pipelines-update-missing-tool", &args).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("Stage 'scan'"), "{}", stderr(&out));
    Ok(())
}

/// A change made by someone else between the read and the apply is refused unless forced
#[tokio::test]
async fn pipelines_update_guards_remote_changes() -> Result<(), Error> {
    // a pipeline of two stages
    let user = TestUser::new("pipelines-update-guard").await?;
    let (first, _) = tool(&user, &[&user.group]).await?;
    let (second, _) = tool(&user, &[&user.group]).await?;
    let name = pipeline_name();
    two_stages(&user, &name, &first, &second).await?;
    // a hook that changes the pipeline with thorctl itself after the edit's first read
    let hook = format!(
        "env -u THORCTL_EDIT_BEFORE_APPLY {} --keys {} --skip-update pipelines update {name} -g {} --sla 999",
        env!("CARGO_BIN_EXE_thorctl"),
        user.keys.display(),
        user.group
    );
    let envs = [("THORCTL_EDIT_BEFORE_APPLY", hook.as_str())];
    let update = ["pipelines", "update", &name, "-g", &user.group];
    // the edit stops, and the remote change stays
    let args = [update.as_slice(), &["--set", "description=mine"]].concat();
    let out = run_with(&user, "pipelines-guard-refused", &args, &envs).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("changed remotely"),
        "{}",
        stderr(&out)
    );
    assert_eq!(stored(&user, &name).await?.sla, 999);
    assert_eq!(
        stored(&user, &name).await?.description.as_deref(),
        Some("before")
    );
    // --force applies it anyway
    let args = [update.as_slice(), &["--set", "description=mine", "--force"]].concat();
    let out = run_with(&user, "pipelines-guard-forced", &args, &envs).await;
    assert_success(&out);
    assert_eq!(
        stored(&user, &name).await?.description.as_deref(),
        Some("mine")
    );
    Ok(())
}

/// Build the editor document of a pipeline
///
/// # Arguments
///
/// * `sla` - The SLA
/// * `order` - The order
/// * `tools` - The tool of each stage by name
fn doc(sla: u64, order: &Value, tools: &Value) -> String {
    // the document the edit view uses
    let doc = json!({
        "description": "before",
        "sla": sla,
        "order": order,
        "triggers": {},
        "stage_tools": tools,
    });
    serde_norway::to_string(&doc).expect("failed to render the document")
}

/// `pipelines edit` changes a pipeline in an editor, including adding a stage with its tool, and
/// cancels, refuses and recovers as the shared edit flow does
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn pipelines_edit_interactive() -> Result<(), Error> {
    // a pipeline of two stages and a third tool for a new stage
    let user = TestUser::new("pipelines-edit").await?;
    let (first, _) = tool(&user, &[&user.group]).await?;
    let (second, second_v1) = tool(&user, &[&user.group]).await?;
    let (third, _) = tool(&user, &[&user.group]).await?;
    let name = pipeline_name();
    two_stages(&user, &name, &first, &second).await?;
    let target = ["pipelines", "edit", &name, "-g", &user.group];
    // an editor changing one field applies it, after showing the header and the tools by name
    let script = EditorScript::replace("sla: 3600", "sla: 7200");
    let editor = script.editor();
    let args = [target.as_slice(), &["--editor", &editor]].concat();
    let out = run_with(&user, "pipelines-edit-sla", &args, &EDIT_ENV).await;
    assert_success(&out);
    assert!(
        stdout(&out).contains("updated successfully"),
        "{}",
        stdout(&out)
    );
    let shown = script.seen(0).expect("the editor wasn't opened");
    assert!(shown.contains("# Editing pipeline"), "{shown}");
    assert!(shown.contains("stage_tools"), "{shown}");
    assert!(shown.contains(&first.name), "{shown}");
    assert_eq!(stored(&user, &name).await?.sla, 7200);
    // -e still names the editor, and the group can be the second argument
    let script = EditorScript::replace("sla: 7200", "sla: 7300");
    let editor = script.editor();
    let args = ["pipelines", "edit", &name, &user.group, "-e", &editor];
    assert_success(&run_with(&user, "pipelines-edit-short-flags", &args, &EDIT_ENV).await);
    assert_eq!(stored(&user, &name).await?.sla, 7300);
    // saving without a change says so and writes nothing
    let script = EditorScript::noop();
    let editor = script.editor();
    let args = [target.as_slice(), &["--editor", &editor]].concat();
    let out = run_with(&user, "pipelines-edit-noop", &args, &EDIT_ENV).await;
    assert_success(&out);
    assert!(stdout(&out).contains("No changes"), "{}", stdout(&out));
    // a new stage with its tool, and a pinned version, in one edit
    let tools = json!({
        "fetch": {"tool": first.name, "version": null},
        "scan": {"tool": second.name, "version": "1.0.0"},
        "report": {"tool": third.name, "version": null},
    });
    let order = json!([["fetch"], ["scan"], ["report"]]);
    let script = EditorScript::write(&doc(7300, &order, &tools));
    let editor = script.editor();
    let args = [target.as_slice(), &["--editor", &editor]].concat();
    assert_success(&run_with(&user, "pipelines-edit-add-stage", &args, &EDIT_ENV).await);
    let read = stored(&user, &name).await?;
    assert_eq!(read.order.len(), 3);
    assert_eq!(read.stage_tools.get("report"), Some(&third.id));
    assert_eq!(read.stage_tools.get("scan"), Some(&second_v1.id));
    // a new stage without a tool is refused, and cancelling keeps the text in a recovery file
    let tools = json!({
        "fetch": {"tool": first.name, "version": null},
        "scan": {"tool": second.name, "version": null},
        "report": {"tool": third.name, "version": null},
    });
    let order = json!([["fetch"], ["scan"], ["report"], ["extra"]]);
    let broken = doc(7300, &order, &tools);
    let script = EditorScript::write(&broken);
    let editor = script.editor();
    let args = [target.as_slice(), &["--editor", &editor]].concat();
    let envs = [EDIT_ENV[0], ("THORCTL_EDIT_PROMPT", "cancel")];
    let out = run_with(&user, "pipelines-edit-invalid", &args, &envs).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("no tool is set for: extra"),
        "{}",
        stderr(&out)
    );
    let path = recovery_path(&stderr(&out)).expect("no recovery file named");
    assert!(std::fs::read_to_string(&path)?.contains("extra"));
    let _ = std::fs::remove_file(path);
    assert_eq!(stored(&user, &name).await?.order.len(), 3);
    // a tool the group doesn't have is refused with the stage named, and recovered the same way
    let script = EditorScript::replace(&format!("tool: {}", third.name), "tool: nosuchtool");
    let editor = script.editor();
    let args = [target.as_slice(), &["--editor", &editor]].concat();
    let out = run_with(&user, "pipelines-edit-unknown-tool", &args, &envs).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("Stage 'report'"), "{}", stderr(&out));
    let path = recovery_path(&stderr(&out)).expect("no recovery file named");
    assert!(std::fs::read_to_string(&path)?.contains("nosuchtool"));
    let _ = std::fs::remove_file(path);
    // text that isn't YAML, cancelled, fails and keeps the text
    let script = EditorScript::write("order: [unterminated");
    let editor = script.editor();
    let args = [target.as_slice(), &["--editor", &editor]].concat();
    let out = run_with(&user, "pipelines-edit-syntax", &args, &envs).await;
    assert_eq!(out.status.code(), Some(1));
    let path = recovery_path(&stderr(&out)).expect("no recovery file named");
    let _ = std::fs::remove_file(path);
    // declining the confirmation applies nothing and keeps the edit
    let script = EditorScript::replace("sla: 7300", "sla: 1");
    let editor = script.editor();
    let args = [target.as_slice(), &["--editor", &editor]].concat();
    let out = run_with(&user, "pipelines-edit-declined", &args, &envs).await;
    assert_success(&out);
    assert!(stdout(&out).contains("Cancelled"), "{}", stdout(&out));
    let path = recovery_path(&stderr(&out)).expect("no recovery file named");
    let _ = std::fs::remove_file(path);
    assert_eq!(stored(&user, &name).await?.sla, 7300);
    // an empty buffer cancels without an error
    let script = EditorScript::write("");
    let editor = script.editor();
    let args = [target.as_slice(), &["--editor", &editor]].concat();
    let out = run_with(&user, "pipelines-edit-empty", &args, &EDIT_ENV).await;
    assert_success(&out);
    assert!(stdout(&out).contains("Cancelled"), "{}", stdout(&out));
    // editing without a terminal and without a change flag points at the flags
    let out = user.run("pipelines-edit-no-tty", &target).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("--set"), "{}", stderr(&out));
    Ok(())
}

/// `pipelines edit` takes the non-interactive sources too, and the edit guards remote changes
#[tokio::test]
async fn pipelines_edit_non_interactive() -> Result<(), Error> {
    // a pipeline of two stages and a third tool
    let user = TestUser::new("pipelines-edit-flags").await?;
    let (first, _) = tool(&user, &[&user.group]).await?;
    let (second, _) = tool(&user, &[&user.group]).await?;
    let (third, _) = tool(&user, &[&user.group]).await?;
    let name = pipeline_name();
    two_stages(&user, &name, &first, &second).await?;
    let target = ["pipelines", "edit", &name, "-g", &user.group];
    // --set edits without opening an editor, and the group can be the positional argument
    let set = format!("stage_tools.fetch.tool={}", third.name);
    let args = [target.as_slice(), &["--set", &set, "--yes"]].concat();
    assert_success(&user.run("pipelines-edit-set", &args).await);
    assert_eq!(read_tool(&user, &name, "fetch").await?, third.id);
    // --dry-run shows the diff and writes nothing
    let args = [target.as_slice(), &["--set", "sla=5", "--dry-run"]].concat();
    let out = user.run("pipelines-edit-dry-run", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("Dry run"), "{}", stdout(&out));
    assert_eq!(stored(&user, &name).await?.sla, 3600);
    // the pipeline can be named with its group
    let qualified = format!("{}/{name}", user.group);
    let args = ["pipelines", "edit", &qualified, "--set", "sla=6"];
    assert_success(&user.run("pipelines-edit-qualified", &args).await);
    assert_eq!(stored(&user, &name).await?.sla, 6);
    // the guard refuses a change made after the read
    let hook = format!(
        "env -u THORCTL_EDIT_BEFORE_APPLY {} --keys {} --skip-update pipelines update {name} -g {} --sla 999",
        env!("CARGO_BIN_EXE_thorctl"),
        user.keys.display(),
        user.group
    );
    let envs = [("THORCTL_EDIT_BEFORE_APPLY", hook.as_str())];
    let args = [target.as_slice(), &["--set", "description=mine"]].concat();
    let out = run_with(&user, "pipelines-edit-guard", &args, &envs).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("changed remotely"),
        "{}",
        stderr(&out)
    );
    Ok(())
}

/// `pipelines stage-tool` pins `TOOL@VERSION` and `--remove` drops a stage with its tool
#[tokio::test]
async fn pipelines_stage_tool_versions_and_remove() -> Result<(), Error> {
    // a pipeline of two stages
    let user = TestUser::new("pipelines-stage-tool-more").await?;
    let (first, _) = tool(&user, &[&user.group]).await?;
    let (second, second_v1) = tool(&user, &[&user.group]).await?;
    let name = pipeline_name();
    two_stages(&user, &name, &first, &first).await?;
    // TOOL@VERSION pins the stage to that version's instance
    let pinned = format!("{}@1.0.0", second.name);
    let args = [
        "pipelines",
        "stage-tool",
        &name,
        "scan",
        &pinned,
        "-g",
        &user.group,
    ];
    let out = user.run("pipelines-stage-tool-at-version", &args).await;
    assert_success(&out);
    assert!(
        stdout(&out).contains("pinned to version"),
        "{}",
        stdout(&out)
    );
    assert_eq!(read_tool(&user, &name, "scan").await?, second_v1.id);
    // an unknown version names the versions the tool has
    let missing = format!("{}@9.9.9", second.name);
    let args = [
        "pipelines",
        "stage-tool",
        &name,
        "scan",
        &missing,
        "-g",
        &user.group,
    ];
    let out = user
        .run("pipelines-stage-tool-unknown-version", &args)
        .await;
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("(it has: 1.0.0)"), "{}", stderr(&out));
    // a version in two places is refused
    let pin = second_v1.id.to_string();
    let args = [
        "pipelines",
        "stage-tool",
        &name,
        "scan",
        &pinned,
        "--pin",
        &pin,
        "-g",
        &user.group,
    ];
    let out = user.run("pipelines-stage-tool-both", &args).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("not both"), "{}", stderr(&out));
    // --remove drops the stage from the order and its tool
    let args = [
        "pipelines",
        "stage-tool",
        &name,
        "scan",
        "--remove",
        "-g",
        &user.group,
    ];
    let out = user.run("pipelines-stage-tool-remove", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("Removed stage"), "{}", stdout(&out));
    let read = stored(&user, &name).await?;
    assert_eq!(read.order, vec![vec!["fetch".to_owned()]]);
    assert!(!read.stage_tools.contains_key("scan"));
    // a stage that is not there, and the last stage, are refused
    let out = user.run("pipelines-stage-tool-remove-missing", &args).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("has no stage 'scan'"),
        "{}",
        stderr(&out)
    );
    let args = [
        "pipelines",
        "stage-tool",
        &name,
        "fetch",
        "--remove",
        "-g",
        &user.group,
    ];
    let out = user.run("pipelines-stage-tool-remove-last", &args).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("only stage"), "{}", stderr(&out));
    Ok(())
}

/// Write an export directory for a pipeline and read its config
///
/// # Arguments
///
/// * `dir` - The export directory
/// * `name` - The pipeline's name
fn exported_config(dir: &std::path::Path, name: &str) -> Value {
    // the config is a JSON file named for the pipeline
    let text = std::fs::read_to_string(dir.join("pipelines").join(format!("{name}.json")))
        .expect("the export wrote no config");
    serde_json::from_str(&text).expect("the config was not JSON")
}

/// An export carries each stage's tool by name, and importing it into another group whose tools
/// have those names recreates the pipeline; an export from before stage tools is refused
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn pipelines_export_import_round_trip() -> Result<(), Error> {
    // a second group the user owns, and tools in both groups
    let user = TestUser::new("pipelines-round-trip").await?;
    let elsewhere = unique("ctlgroup");
    user.admin
        .groups
        .create(
            &GroupRequest::new(&elsewhere).owners(
                GroupUsersRequest::default()
                    .direct("thorium")
                    .direct(&user.username),
            ),
        )
        .await?;
    let (first, _) = tool(&user, &[&user.group, &elsewhere]).await?;
    let (second, second_v1) = tool(&user, &[&user.group, &elsewhere]).await?;
    // a pipeline following one tool and pinning a version of another
    let name = pipeline_name();
    let req = PipelineRequest::new(&user.group, &name, json!([["fetch"], ["scan", "hash"]]))
        .sla(1234)
        .description("round trip")
        .trigger("t", thorium::models::EventTrigger::NewSample)
        .stage_tool("fetch", first.id)
        .stage_tool("scan", second_v1.id)
        .stage_tool("hash", second.id);
    user.client.pipelines.create(&req).await?;
    // export it
    let dir = std::env::temp_dir().join(unique("pipeline-export-"));
    let dir_arg = dir.to_string_lossy().into_owned();
    let args = [
        "pipelines",
        "export",
        &name,
        "-g",
        &user.group,
        "-o",
        &dir_arg,
        "--overwrite",
    ];
    let out = user.run("pipelines-export", &args).await;
    assert_success(&out);
    // the config names the tools and holds no id
    let config = exported_config(&dir, &name);
    assert_eq!(config["stage_tools"]["fetch"]["tool"], first.name);
    assert_eq!(config["stage_tools"]["fetch"]["version"], Value::Null);
    assert_eq!(config["stage_tools"]["scan"]["tool"], second.name);
    assert_eq!(config["stage_tools"]["scan"]["version"], "1.0.0");
    let text = config.to_string();
    assert!(!text.contains(&first.id.to_string()), "{text}");
    assert!(!text.contains(&second_v1.id.to_string()), "{text}");
    // import it into the other group, which has the same tools
    let args = [
        "pipelines",
        "import",
        "-g",
        &elsewhere,
        "-i",
        &dir_arg,
        "--overwrite",
        "--skip-push",
    ];
    let out = user.run("pipelines-import", &args).await;
    assert_success(&out);
    let imported = user.client.pipelines.get(&elsewhere, &name).await?;
    assert_eq!(imported.order, req_order(&user, &name).await?);
    assert_eq!(imported.sla, 1234);
    assert_eq!(imported.description.as_deref(), Some("round trip"));
    assert!(imported.triggers.contains_key("t"));
    assert_eq!(imported.stage_tools.get("fetch"), Some(&first.id));
    assert_eq!(imported.stage_tools.get("scan"), Some(&second_v1.id));
    assert_eq!(imported.stage_tools.get("hash"), Some(&second.id));
    // importing it again changes nothing
    let out = user.run("pipelines-import-again", &args).await;
    assert_success(&out);
    // an export from before pipelines had stage tools is refused before anything is created
    let old = json!({
        "name": "oldpipe", "group": user.group, "order": [["a"], ["b"]], "sla": 10,
        "triggers": {}, "description": null,
    });
    std::fs::write(
        dir.join("pipelines").join("oldpipe.json"),
        serde_json::to_string_pretty(&old)?,
    )?;
    let args = [
        "pipelines",
        "import",
        "oldpipe",
        "-g",
        &elsewhere,
        "-i",
        &dir_arg,
        "--overwrite",
        "--skip-push",
    ];
    let out = user.run("pipelines-import-old", &args).await;
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("stage(s): a, b"), "{err}");
    assert!(err.contains("pipelines stage-tool"), "{err}");
    assert!(err.contains("pipelines create"), "{err}");
    assert!(
        user.client
            .pipelines
            .get(&elsewhere, "oldpipe")
            .await
            .is_err()
    );
    // a config naming a tool the target group lacks is refused with the tool and group named
    let mut missing = config.clone();
    missing["name"] = json!("missingtool");
    missing["stage_tools"]["fetch"] = json!({"tool": "nosuchtool", "version": null});
    std::fs::write(
        dir.join("pipelines").join("missingtool.json"),
        serde_json::to_string_pretty(&missing)?,
    )?;
    let args = [
        "pipelines",
        "import",
        "missingtool",
        "-g",
        &elsewhere,
        "-i",
        &dir_arg,
        "--overwrite",
        "--skip-push",
    ];
    let out = user.run("pipelines-import-missing-tool", &args).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("nosuchtool"), "{}", stderr(&out));
    assert!(stderr(&out).contains(&elsewhere), "{}", stderr(&out));
    std::fs::remove_dir_all(&dir)?;
    Ok(())
}

/// Without a terminal, importing or exporting over differing resources refuses unless
/// `--skip-conflicts` or `--overwrite` says what to do, and the flags skip or replace them
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn pipelines_import_export_conflicts_need_a_flag_without_a_terminal() -> Result<(), Error> {
    // a pipeline, exported to a directory
    let user = TestUser::new("pipelines-conflicts").await?;
    let (first, _) = tool(&user, &[&user.group]).await?;
    let (second, _) = tool(&user, &[&user.group]).await?;
    let name = pipeline_name();
    two_stages(&user, &name, &first, &second).await?;
    let dir = std::env::temp_dir().join(unique("pipeline-conflicts-"));
    let dir_arg = dir.to_string_lossy().into_owned();
    let export = [
        "pipelines",
        "export",
        &name,
        "-g",
        &user.group,
        "-o",
        &dir_arg,
    ];
    assert_success(&user.run("pipelines-export-new", &export).await);
    // the exported copy now asks for a different SLA than the stored pipeline has
    let path = dir.join("pipelines").join(format!("{name}.json"));
    let mut config: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    config["sla"] = json!(7200);
    let changed = serde_json::to_string_pretty(&config)?;
    std::fs::write(&path, &changed)?;
    // importing it with nothing to settle the difference refuses and changes nothing
    let import = [
        "pipelines",
        "import",
        "-g",
        &user.group,
        "-i",
        &dir_arg,
        "--skip-push",
    ];
    let out = user.run("pipelines-import-refused", &import).await;
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("--skip-conflicts"), "{err}");
    assert!(err.contains(&name), "{err}");
    assert_eq!(stored(&user, &name).await?.sla, 3600);
    // --skip-conflicts leaves it alone, says how many were skipped, and succeeds (also with -q)
    for quiet in [false, true] {
        let mut args = import.to_vec();
        args.push("--skip-conflicts");
        if quiet {
            args.insert(0, "-q");
        }
        let out = user.run("pipelines-import-skip", &args).await;
        assert_success(&out);
        let err = stderr(&out);
        assert!(err.contains("Skipped 1 existing"), "quiet={quiet}: {err}");
        assert!(err.contains(&name), "quiet={quiet}: {err}");
        assert_eq!(stored(&user, &name).await?.sla, 3600);
    }
    // --overwrite applies the difference
    let mut args = import.to_vec();
    args.push("--overwrite");
    assert_success(&user.run("pipelines-import-overwrite", &args).await);
    assert_eq!(stored(&user, &name).await?.sla, 7200);
    // exporting over the edited file: the stored pipeline now matches it, so edit the file again
    config["description"] = json!("edited on disk");
    let edited = serde_json::to_string_pretty(&config)?;
    std::fs::write(&path, &edited)?;
    // with nothing to settle the difference the export refuses and leaves the file alone
    let out = user.run("pipelines-export-refused", &export).await;
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(
        err.contains("--skip-conflicts") && err.contains("--overwrite"),
        "{err}"
    );
    assert_eq!(std::fs::read_to_string(&path)?, edited);
    // --skip-conflicts keeps the file, says how many were skipped, and succeeds (also with -q)
    for quiet in [false, true] {
        let mut args = export.to_vec();
        args.push("--skip-conflicts");
        if quiet {
            args.insert(0, "-q");
        }
        let out = user.run("pipelines-export-skip", &args).await;
        assert_success(&out);
        let err = stderr(&out);
        assert!(err.contains("Skipped 1 existing"), "quiet={quiet}: {err}");
        assert_eq!(std::fs::read_to_string(&path)?, edited);
    }
    // --overwrite replaces the file with the stored pipeline
    let mut args = export.to_vec();
    args.push("--overwrite");
    assert_success(&user.run("pipelines-export-overwrite", &args).await);
    assert_ne!(std::fs::read_to_string(&path)?, edited);
    std::fs::remove_dir_all(&dir)?;
    Ok(())
}

/// Read the order of a pipeline in the user's group
///
/// # Arguments
///
/// * `user` - The test user
/// * `name` - The pipeline's name
async fn req_order(user: &TestUser, name: &str) -> Result<Vec<Vec<String>>, Error> {
    // the order is part of the stored pipeline
    Ok(stored(user, name).await?.order)
}

/// Create a pipeline the test API can run, with no reactions yet
///
/// # Arguments
///
/// * `user` - The test user
async fn runnable(user: &TestUser) -> Result<Pipeline, Error> {
    // generated pipelines have a tool for every stage
    let req = common::cli_pipeline(&user.group, &user.client).await?;
    user.client.pipelines.get(&user.group, &req.name).await
}

/// Upload a random file to the user's group and get its SHA256
///
/// # Arguments
///
/// * `user` - The test user
async fn sample(user: &TestUser) -> Result<String, Error> {
    // upload a random file
    let req = generators::gen_sample(&user.group);
    Ok(user.client.files.create(req).await?.sha256)
}

/// Get the ids of a pipeline's reactions
///
/// # Arguments
///
/// * `user` - The test user
/// * `pipeline` - The pipeline's name
async fn reactions_of(user: &TestUser, pipeline: &str) -> Result<Vec<Uuid>, Error> {
    // read every page of the pipeline's reactions
    let mut cursor = user.client.reactions.list(&user.group, pipeline).details();
    let mut ids = Vec::new();
    loop {
        cursor.next().await?;
        ids.extend(cursor.details.drain(..).map(|reaction| reaction.id));
        if cursor.exhausted {
            return Ok(ids);
        }
    }
}

/// Wait for a pipeline to have a reaction that isn't in a set of known ones
///
/// # Arguments
///
/// * `user` - The test user
/// * `pipeline` - The pipeline's name
/// * `known` - The reactions already seen; the new one is added to it
async fn new_reaction(
    user: &TestUser,
    pipeline: &str,
    known: &mut HashSet<Uuid>,
) -> Result<Uuid, Error> {
    // poll the pipeline's reactions until one is new
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        for id in reactions_of(user, pipeline).await? {
            if known.insert(id) {
                return Ok(id);
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "pipelines run never created a reaction"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
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
    Ok(thorium::test_utilities::auth_header(&token))
}

/// `pipelines run` creates a reaction with tags, an SLA, a sample list and a parent, and waits by
/// how the reaction ends: 0 completed (with logs and results), 1 failed, 3 timed out
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn pipelines_run_common_flags() -> Result<(), Error> {
    // a runnable pipeline and two files
    let user = TestUser::new("pipelines-run").await?;
    let pipe = runnable(&user).await?;
    let first = sample(&user).await?;
    let second = sample(&user).await?;
    let mut known = HashSet::new();
    // a run with tags, an SLA and a list of samples prints only its reaction id
    let list = std::env::temp_dir().join(unique("pipeline-run-samples-"));
    std::fs::write(&list, format!("# the files\n{first}\n\n{second}\n"))?;
    let list_arg = list.to_string_lossy().into_owned();
    let args = [
        "pipelines",
        "run",
        &pipe.name,
        "-g",
        &user.group,
        "--sample-list",
        &list_arg,
        "--tag",
        "t1",
        "--tag",
        "t2",
        "--sla",
        "60",
    ];
    let out = user.run("pipelines-run-flags", &args).await;
    assert_success(&out);
    let started = printed_id(&out);
    known.insert(started);
    let reaction = user.client.reactions.get(&user.group, started).await?;
    assert_eq!(reaction.pipeline, pipe.name);
    assert!(
        reaction.tags.contains(&"t1".to_owned()),
        "{:?}",
        reaction.tags
    );
    assert!(
        reaction.tags.contains(&"t2".to_owned()),
        "{:?}",
        reaction.tags
    );
    assert!(reaction.samples.contains(&first) && reaction.samples.contains(&second));
    std::fs::remove_file(&list)?;
    // a run can be a child of another reaction, and the pipeline can be named with its group
    let parent = started.to_string();
    let qualified = format!("{}/{}", user.group, pipe.name);
    let args = [
        "pipelines",
        "run",
        &qualified,
        "--sample",
        &first,
        "--parent",
        &parent,
    ];
    let out = user.run("pipelines-run-parent", &args).await;
    assert_success(&out);
    let child = printed_id(&out);
    known.insert(child);
    let reaction = user.client.reactions.get(&user.group, child).await?;
    assert_eq!(reaction.parent, Some(started));
    // a wait that outlasts its timeout prints the run and exits 3
    let args = [
        "pipelines",
        "run",
        &pipe.name,
        "-g",
        &user.group,
        "--sample",
        &first,
        "--wait",
        "--timeout",
        "1s",
        "--interval",
        "200ms",
    ];
    let out = user.run("pipelines-run-wait-timeout", &args).await;
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(stderr(&out).contains("Timed out"), "{}", stderr(&out));
    known.extend(reactions_of(&user, &pipe.name).await?);
    // a wait that ends with the run failed exits 1 naming the status
    let args = [
        "pipelines",
        "run",
        &pipe.name,
        "-g",
        &user.group,
        "--sample",
        &first,
        "--wait",
        "--timeout",
        "60s",
        "--interval",
        "200ms",
    ];
    let waiting = user.spawn(&args);
    let run = new_reaction(&user, &pipe.name, &mut known).await?;
    let token = auth(&user).await?;
    runs::handle_reaction(&user.client, &token, &user.group, run, "fail").await?;
    let out = tokio::time::timeout(Duration::from_secs(90), waiting.wait_with_output())
        .await
        .expect("pipelines run --wait didn't return")?;
    user.record("pipelines-run-wait-failed", &args, &out);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stdout(&out).contains("Failed"), "{}", stdout(&out));
    // a wait that ends with the run completed prints its logs, and exits 0; a pipeline of its own
    // keeps the workers' jobs from belonging to earlier runs
    let pipe = runnable(&user).await?;
    let dir = std::env::temp_dir().join(unique("pipeline-run-output-"));
    let dir_arg = dir.to_string_lossy().into_owned();
    let args = [
        "pipelines",
        "run",
        &pipe.name,
        "-g",
        &user.group,
        "--sample",
        &first,
        "--wait",
        "--logs",
        "--output",
        &dir_arg,
        "--timeout",
        "60s",
        "--interval",
        "200ms",
    ];
    let waiting = user.spawn(&args);
    let run = loop {
        // the pipeline's only reaction is the run the wait created
        let all = reactions_of(&user, &pipe.name).await?;
        if let Some(found) = all.into_iter().next() {
            break found;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    // finish the run's jobs as its workers would
    let reaction = user.client.reactions.get(&user.group, run).await?;
    assert_eq!(reaction.pipeline, pipe.name);
    user.complete(&pipe).await?;
    let out = tokio::time::timeout(Duration::from_secs(90), waiting.wait_with_output())
        .await
        .expect("pipelines run --wait didn't return")?;
    user.record("pipelines-run-wait-completed", &args, &out);
    assert_success(&out);
    let text = stdout(&out);
    assert!(text.contains("Completed"), "{text}");
    assert!(text.contains("Stage '"), "no logs in {text}");
    let _ = std::fs::remove_dir_all(&dir);
    // flags that can't be combined are a usage error
    let args = ["pipelines", "run", &pipe.name, "--wait", "--watch"];
    let out = user.run("pipelines-run-wait-watch", &args).await;
    assert_eq!(out.status.code(), Some(2));
    // a pipeline that doesn't exist is an error
    let args = ["pipelines", "run", "nosuchpipeline", "-g", &user.group];
    let out = user.run("pipelines-run-missing", &args).await;
    assert_eq!(out.status.code(), Some(1));
    Ok(())
}
