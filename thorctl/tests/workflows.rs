//! Tests the thorctl workflows and instances commands against the test API

mod common;

use common::editor::{EditorScript, Step, recovery_path};
use common::{TestUser, assert_success, json, stdout};
use serde_json::Value;
use std::process::{Output, Stdio};
use std::time::Duration;
use thorium::client::ResultsClient;
use thorium::models::{
    AgentEntityRequest, Entity, EntityKinds, EntityListOpts, EntityMetadata, EntityMetadataUpdate,
    EntityUpdate, ImageScaler, ModelAuthLevel, OutputDisplayType, OutputRequest,
    ReactionCursorOpts, SampleRequest, ToolMechanism, ToolSetEntityRequest, ToolSetExclusion,
    ToolSetExclusionReason, ToolSetInclusion, WorkflowCatalogEntity, WorkflowInstanceStatus,
};
use thorium::test_utilities::{self, generators, runs};
use thorium::{Error, Thorium};
use tokio::io::AsyncWriteExt as _;
use uuid::Uuid;

/// The environment that lets thorctl open the editor script without a terminal; without a
/// terminal a failed edit is cancelled and saved, and the confirmation is accepted
const EDITOR_ENV: [(&str, &str); 1] = [("THORCTL_EDIT_ASSUME_TTY", "1")];

/// Run thorctl with environment variables and optional text on its stdin, recording the run
///
/// # Arguments
///
/// * `user` - The test user to run as
/// * `name` - The name of the run's transcript
/// * `args` - The thorctl arguments after the global flags
/// * `envs` - The environment variables to set
/// * `input` - The text to write to stdin, if any
async fn run_with(
    user: &TestUser,
    name: &str,
    args: &[&str],
    envs: &[(&str, &str)],
    input: Option<&str>,
) -> Output {
    // start thorctl with a pipe for stdin when there is input
    let mut command = user.command(args);
    command.envs(envs.iter().copied());
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn().expect("failed to start thorctl");
    // write the input and close stdin so thorctl sees the end
    if let Some(input) = input {
        let mut stdin = child.stdin.take().expect("no stdin pipe");
        stdin
            .write_all(input.as_bytes())
            .await
            .expect("failed to write stdin");
        drop(stdin);
    }
    // wait for the run to finish
    let output = tokio::time::timeout(Duration::from_secs(120), child.wait_with_output())
        .await
        .expect("thorctl timed out")
        .expect("failed to run thorctl");
    // record this run
    user.record(name, args, &output);
    output
}

/// A shell command that runs thorctl as the user, for the hook that changes a workflow between
/// the first read and the apply
///
/// # Arguments
///
/// * `user` - The test user
/// * `args` - The thorctl arguments
fn hook(user: &TestUser, args: &str) -> String {
    // run the same binary with the same keys file, without the hook itself
    format!(
        "env -u THORCTL_EDIT_BEFORE_APPLY {} --keys {} --skip-update {args}",
        env!("CARGO_BIN_EXE_thorctl"),
        user.keys.display()
    )
}

/// Read a workflow as the user's client sees it
///
/// # Arguments
///
/// * `user` - The test user
/// * `id` - The workflow's id
async fn read_workflow(
    user: &TestUser,
    id: Uuid,
) -> Result<(Entity, WorkflowCatalogEntity), Error> {
    // read the entity and take its workflow metadata
    let entity = user.client.entities.get(id).await?;
    let EntityMetadata::WorkflowCatalog(meta) = entity.metadata.clone() else {
        panic!("expected a workflow");
    };
    Ok((entity, meta))
}

/// Write a file with some contents to the temp dir
///
/// # Arguments
///
/// * `name` - The file's name
/// * `contents` - What to write
fn write_file(name: &str, contents: &str) -> String {
    // write the file under a unique name
    let path = std::env::temp_dir().join(format!("{}-{name}", Uuid::new_v4().simple()));
    std::fs::write(&path, contents).expect("failed to write the file");
    path.to_string_lossy().into_owned()
}

/// Create a second group owned by the user
///
/// # Arguments
///
/// * `user` - The test user
/// * `name` - The group's name
async fn extra_group(user: &TestUser, name: &str) -> Result<String, Error> {
    // the admin creates the group with the user as an owner
    let owners = thorium::models::GroupUsersRequest::default()
        .direct("thorium")
        .direct(&user.username);
    user.admin
        .groups
        .create(&thorium::models::GroupRequest::new(name).owners(owners))
        .await?;
    Ok(name.to_owned())
}

/// Get the sorted names of a workflow's agents
///
/// # Arguments
///
/// * `meta` - The workflow's metadata
fn agent_names(meta: &WorkflowCatalogEntity) -> Vec<String> {
    // name and sort the agents
    let mut names: Vec<String> = meta.agents.iter().map(|agent| agent.name.clone()).collect();
    names.sort();
    names
}

/// Create an agentic tool in the user's group
///
/// # Arguments
///
/// * `user` - The test user
async fn agentic_tool(user: &TestUser) -> Result<(Entity, Entity), Error> {
    // publish a random agentic tool at 1.0.0
    generators::tool(
        &[&user.group],
        "1.0.0",
        &generators::gen_agentic_spec(),
        &user.client,
    )
    .await
}

/// Create a harness in the user's group and get its first instance
///
/// # Arguments
///
/// * `user` - The test user
async fn harness_instance(user: &TestUser) -> Result<Entity, Error> {
    // publish a random harness at 1.0.0 and keep its instance
    let (_, instance) = generators::tool(
        &[&user.group],
        "1.0.0",
        &generators::gen_harness_spec(),
        &user.client,
    )
    .await?;
    Ok(instance)
}

/// Create an agent in the user's group that calls some tools
///
/// # Arguments
///
/// * `user` - The test user
/// * `name` - The agent's name and delegation function name
/// * `tools` - The tools and tool instances it calls
async fn agent(user: &TestUser, name: &str, tools: &[Uuid]) -> Result<Entity, Error> {
    // build a config named after the agent with its tool edges
    let mut req = AgentEntityRequest::new(generators::gen_agent_config(name));
    req.tools = tools.to_vec();
    let resp = user
        .client
        .entities
        .create(generators::gen_agent(name, &[&user.group], req))
        .await?;
    user.client.entities.get(resp.id).await
}

/// Find the id of an entity of a kind by name in the user's group
///
/// # Arguments
///
/// * `user` - The test user
/// * `kind` - The kind of entity
/// * `name` - The entity's name
async fn find_id(user: &TestUser, kind: EntityKinds, name: &str) -> Result<Uuid, Error> {
    // list the entities of this kind in the user's group and find the name
    let opts = EntityListOpts::default()
        .groups(vec![user.group.clone()])
        .kind(kind)
        .limit(100);
    let listed = user.client.entities.list(&opts).await?;
    Ok(listed
        .data
        .iter()
        .find(|line| line.name == name)
        .map(|line| line.id)
        .expect("the entity wasn't created"))
}

/// Sort a list of ids as strings, the way JSON output holds them
///
/// # Arguments
///
/// * `ids` - The ids to sort
fn sorted_strings(ids: &[Uuid]) -> Vec<String> {
    // stringify and sort the ids
    let mut ids = ids.iter().map(ToString::to_string).collect::<Vec<String>>();
    ids.sort();
    ids
}

/// Get the sorted ids of the agents in a workflow's JSON
///
/// # Arguments
///
/// * `meta` - The workflow's metadata JSON
fn agent_ids(meta: &Value) -> Vec<String> {
    // collect and sort the agent ids
    let mut ids = meta["agents"].as_array().map_or_else(Vec::new, |agents| {
        agents
            .iter()
            .filter_map(|agent| agent["id"].as_str().map(ToOwned::to_owned))
            .collect::<Vec<String>>()
    });
    ids.sort();
    ids
}

/// `workflows create`, `add-agent`, `remove-agent`, and `entry` change the workflow's agents
/// and entry agent, `describe` shows them, `instances get --wait-for` waits for a revision, and
/// `workflows toolset` prints an agent's tools with their specs
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn workflows_create_and_edit() -> Result<(), Error> {
    // a user named after europa in a group named after minerva, with three agents
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "workflows-create-edit",
        &generators::readable_name("europa", &suffix),
        &generators::readable_name("minerva", &suffix),
    )
    .await?;
    let (tool_root, tool) = agentic_tool(&user).await?;
    let triage = agent(&user, "triage", &[tool_root.id]).await?;
    let reverser = agent(&user, "reverser", &[]).await?;
    let late = agent(&user, "late", &[]).await?;
    // create a workflow with two agents, an entry agent, and two default models
    let args = [
        "workflows",
        "create",
        "malware-triage",
        "-g",
        &user.group,
        "--agent",
        "triage",
        "--agent",
        "reverser",
        "--entry",
        "triage",
        "--default-model",
        "claude-sonnet",
        "--default-model",
        "gpt-5",
        "--max-alive-sub-agents",
        "4",
    ];
    assert_success(&user.run("workflows-create", &args).await);
    let id = find_id(&user, EntityKinds::WorkflowCatalog, "malware-triage").await?;
    // its agents, entry agent, and default models read back
    let read = || async {
        let args = ["workflows", "describe", "malware-triage", "-o", "json"];
        json(&user.run("workflows-describe-json", &args).await)["metadata"]["WorkflowCatalog"]
            .clone()
    };
    let meta = read().await;
    assert_eq!(agent_ids(&meta), sorted_strings(&[triage.id, reverser.id]));
    assert_eq!(meta["entry_agent"], Value::String(triage.id.to_string()));
    assert_eq!(
        meta["default_models"][1]["model_id"],
        Value::String("gpt-5".into())
    );
    assert_eq!(meta["limits"]["max_alive_sub_agents"], Value::from(4));
    // add and remove agents
    let args = ["workflows", "add-agent", "malware-triage", "late"];
    assert_success(&user.run("workflows-add-agent", &args).await);
    assert_eq!(
        agent_ids(&read().await),
        sorted_strings(&[triage.id, reverser.id, late.id])
    );
    let args = ["workflows", "remove-agent", "malware-triage", "reverser"];
    assert_success(&user.run("workflows-remove-agent", &args).await);
    assert_eq!(
        agent_ids(&read().await),
        sorted_strings(&[triage.id, late.id])
    );
    // set the entry agent, and refuse one that isn't in the workflow
    let args = ["workflows", "entry", "malware-triage", "late"];
    assert_success(&user.run("workflows-entry", &args).await);
    assert_eq!(
        read().await["entry_agent"],
        Value::String(late.id.to_string())
    );
    let args = ["workflows", "entry", "malware-triage", "reverser"];
    let out = user.run("workflows-entry-refused", &args).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("400") && stderr.contains("entry agent"),
        "{stderr}"
    );
    assert_eq!(
        read().await["entry_agent"],
        Value::String(late.id.to_string())
    );
    // describe shows the agents, entry agent, default models, and latest instance
    let out = user
        .run(
            "workflows-describe",
            &["workflows", "describe", "malware-triage"],
        )
        .await;
    assert_success(&out);
    let text = stdout(&out);
    for expected in [
        "triage",
        "late",
        "Entry agent",
        "claude-sonnet",
        "gpt-5",
        "Latest instance",
    ] {
        assert!(
            text.contains(expected),
            "describe is missing {expected}:\n{text}"
        );
    }
    // get lists the workflow
    let args = ["workflows", "get", "-g", &user.group, "-o", "json"];
    let listed = json(&user.run("workflows-get-json", &args).await);
    assert_eq!(listed.as_array().map(Vec::len), Some(1));
    // submit r1 now and r2 while instances get waits for it
    let workflow = user.client.entities.get(id).await?;
    let harness = harness_instance(&user).await?;
    let r1 =
        generators::gen_workflow_instance(&workflow, 1, None, &[(&triage, &[&tool, &harness])]);
    let r1_id = user
        .client
        .entities
        .create(generators::gen_workflow_instance_req(&r1))
        .await?
        .id;
    let args = [
        "instances",
        "get",
        "workflow",
        "malware-triage",
        "--wait-for",
        "instance-revision=2",
        "--interval",
        "1s",
        "--timeout",
        "60s",
        "-o",
        "json",
    ];
    let waiting = user.spawn(&args);
    tokio::time::sleep(Duration::from_secs(3)).await;
    let r2 = generators::gen_workflow_instance(
        &workflow,
        2,
        Some(r1_id),
        &[(&triage, &[&tool, &harness])],
    );
    let r2_id = user
        .client
        .entities
        .create(generators::gen_workflow_instance_req(&r2))
        .await?
        .id;
    let out = tokio::time::timeout(Duration::from_secs(90), waiting.wait_with_output())
        .await
        .expect("instances get didn't return")?;
    user.record("instances-get-wait", &args, &out);
    assert_success(&out);
    let waited = json(&out);
    assert_eq!(waited["instance_revision"], Value::from(2));
    assert_eq!(waited["latest_instance"], Value::String(r2_id.to_string()));
    assert_eq!(waited["instances"].as_array().map(Vec::len), Some(2));
    // workflows toolset prints the agent's tools with their full specs
    let r2_arg = r2_id.to_string();
    let args = ["workflows", "toolset", &r2_arg, "--agent", "triage"];
    let out = user.run("workflows-toolset", &args).await;
    assert_success(&out);
    let text = stdout(&out);
    assert!(text.contains(&tool.name), "{text}");
    assert!(text.contains("input_schema"), "{text}");
    let args = [
        "workflows",
        "toolset",
        &r2_arg,
        "--agent",
        "triage",
        "-o",
        "json",
    ];
    let toolset = json(&user.run("workflows-toolset-json", &args).await);
    assert_eq!(
        toolset["agent"]["agent"],
        Value::String(triage.id.to_string())
    );
    assert_eq!(
        toolset["tools"][0]["tool"]["tool_instance"],
        Value::String(tool.id.to_string())
    );
    assert!(
        toolset["tools"][0]["spec"]["Agentic"].is_object(),
        "{toolset}"
    );
    // an agent not in the instance prints the API's 404
    let args = ["workflows", "toolset", &r2_arg, "--agent", "late"];
    let out = user.run("workflows-toolset-missing", &args).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("404"), "{stderr}");
    Ok(())
}

/// `instances describe` renders a workflow instance's status, errors, digest, and tool table,
/// and `instances diff` shows the fields that differ between two revisions
#[tokio::test]
async fn instances_describe_and_diff() -> Result<(), Error> {
    // a user named after io in a group named after mars, and a workflow with two revisions
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "instances-describe-diff",
        &generators::readable_name("io", &suffix),
        &generators::readable_name("mars", &suffix),
    )
    .await?;
    let (tool_root, tool) = agentic_tool(&user).await?;
    let solo = agent(&user, "solo", &[tool_root.id]).await?;
    let args = [
        "workflows",
        "create",
        "diffed",
        "-g",
        &user.group,
        "--agent",
        "solo",
        "--entry",
        "solo",
    ];
    assert_success(&user.run("workflows-create-diffed", &args).await);
    let id = find_id(&user, EntityKinds::WorkflowCatalog, "diffed").await?;
    let workflow = user.client.entities.get(id).await?;
    let harness = harness_instance(&user).await?;
    let r1 = generators::gen_workflow_instance(&workflow, 1, None, &[(&solo, &[&tool, &harness])]);
    let r1_id = user
        .client
        .entities
        .create(generators::gen_workflow_instance_req(&r1))
        .await?
        .id;
    let mut r2 = generators::gen_workflow_instance(
        &workflow,
        2,
        Some(r1_id),
        &[(&solo, &[&tool, &harness])],
    );
    r2.status = WorkflowInstanceStatus::Invalid {
        errors: vec!["agent solo has no harness".into()],
    };
    let r2_id = user
        .client
        .entities
        .create(generators::gen_workflow_instance_req(&r2))
        .await?
        .id;
    // describe shows the status, errors, digest, and the tool table
    let r2_arg = r2_id.to_string();
    let out = user
        .run("instances-describe", &["instances", "describe", &r2_arg])
        .await;
    assert_success(&out);
    let text = stdout(&out);
    for expected in [
        "Invalid",
        "agent solo has no harness",
        r2.digest.as_str(),
        tool.name.as_str(),
        "SPEC HASH",
        "solo",
    ] {
        assert!(
            text.contains(expected),
            "describe is missing {expected}:\n{text}"
        );
    }
    // describe by name reads the same instance as JSON
    let args = ["instances", "describe", "diffed#r1", "-o", "json"];
    let described = json(&user.run("instances-describe-name-json", &args).await);
    assert_eq!(described["id"], Value::String(r1_id.to_string()));
    // diff shows the fields that differ, and not the ones that don't
    let r1_arg = r1_id.to_string();
    let out = user
        .run("instances-diff", &["instances", "diff", &r1_arg, &r2_arg])
        .await;
    assert_success(&out);
    let text = stdout(&out);
    for expected in ["revision", "previous", "status"] {
        assert!(
            text.contains(expected),
            "diff is missing {expected}:\n{text}"
        );
    }
    assert!(!text.contains("workflow_groups"), "{text}");
    let args = ["instances", "diff", &r1_arg, &r2_arg, "-o", "json"];
    let rows = json(&user.run("instances-diff-json", &args).await);
    let revision = rows
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["field"] == "revision"))
        .expect("the revision differs");
    assert_eq!(revision["first"], Value::from(1));
    assert_eq!(revision["second"], Value::from(2));
    // instances get lists both revisions of the workflow
    let args = ["instances", "get", "workflow", "diffed", "-o", "json"];
    let listed = json(&user.run("instances-get-workflow", &args).await);
    assert_eq!(listed["instance_revision"], Value::from(2));
    assert_eq!(listed["latest_instance"], Value::String(r1_id.to_string()));
    Ok(())
}

/// `instances describe` renders a toolset instance's members, exclusions with their reasons,
/// status, and digest, and a tool instance's version and hash; `instances get` lists both kinds
#[tokio::test]
async fn instances_toolset_and_tool_views() -> Result<(), Error> {
    // a user named after callisto in a group named after juno, and a toolset with one instance
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "instances-toolset-tool",
        &generators::readable_name("callisto", &suffix),
        &generators::readable_name("juno", &suffix),
    )
    .await?;
    let (member_root, member) = agentic_tool(&user).await?;
    let (left_out_root, _) = agentic_tool(&user).await?;
    let resp = user
        .client
        .entities
        .create(generators::gen_toolset(
            "kit",
            &[&user.group],
            ToolSetEntityRequest::default().tool(member_root.id),
        ))
        .await?;
    let tool_set = user.client.entities.get(resp.id).await?;
    let mut instance = generators::gen_toolset_instance(&tool_set, 1, None, &[&member]);
    instance.excluded.push(ToolSetExclusion {
        tool: left_out_root.id,
        tool_instance: None,
        name: left_out_root.name.clone(),
        inclusion: ToolSetInclusion::Selector,
        reason: ToolSetExclusionReason::NotTaggedInAllGroups,
        missing_groups: vec!["vesta".into()],
    });
    let instance_id = user
        .client
        .entities
        .create(generators::gen_toolset_instance_req(&instance))
        .await?
        .id;
    // describe shows the members, exclusions with reasons, status, and digest
    let instance_arg = instance_id.to_string();
    let out = user
        .run(
            "instances-describe-toolset",
            &["instances", "describe", &instance_arg],
        )
        .await;
    assert_success(&out);
    let text = stdout(&out);
    for expected in [
        member.name.as_str(),
        "ExplicitLatest",
        left_out_root.name.as_str(),
        "not tagged in `vesta`",
        "Ready",
        instance.digest.as_str(),
    ] {
        assert!(
            text.contains(expected),
            "describe is missing {expected}:\n{text}"
        );
    }
    // a tool instance describes its version and spec hash
    let member_arg = member.id.to_string();
    let out = user
        .run(
            "instances-describe-tool",
            &["instances", "describe", &member_arg],
        )
        .await;
    assert_success(&out);
    let text = stdout(&out);
    assert!(text.contains("1.0.0"), "{text}");
    let thorium::models::EntityMetadata::ToolInstance(stored) = &member.metadata else {
        panic!("expected a tool instance");
    };
    assert!(text.contains(&stored.spec_hash), "{text}");
    // instances get lists the toolset's and the tool's instances
    let args = ["instances", "get", "toolset", "kit", "-o", "json"];
    let listed = json(&user.run("instances-get-toolset", &args).await);
    assert_eq!(listed["instance_revision"], Value::from(1));
    assert_eq!(
        listed["latest_instance"],
        Value::String(instance_id.to_string())
    );
    let root_arg = member_root.id.to_string();
    let args = ["instances", "get", "tool", &root_arg, "-o", "json"];
    let listed = json(&user.run("instances-get-tool", &args).await);
    assert_eq!(
        listed["instances"][0]["id"],
        Value::String(member.id.to_string())
    );
    Ok(())
}

/// `workflows toolset` finds the agent among the instance's own agents, so an old instance
/// still shows a deleted agent's snapshot, and `instances describe` finds tool, toolset, and
/// workflow instances by name
#[tokio::test]
async fn instance_lookups_use_snapshots() -> Result<(), Error> {
    // a user named after ganymede in a group named after ceres, and a workflow with one instance
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "instance-lookups",
        &generators::readable_name("ganymede", &suffix),
        &generators::readable_name("ceres", &suffix),
    )
    .await?;
    let (tool_root, tool) = agentic_tool(&user).await?;
    let gone = agent(&user, "gone", &[tool_root.id]).await?;
    let args = [
        "workflows",
        "create",
        "lookups",
        "-g",
        &user.group,
        "--agent",
        "gone",
        "--entry",
        "gone",
    ];
    assert_success(&user.run("workflows-create-lookups", &args).await);
    let id = find_id(&user, EntityKinds::WorkflowCatalog, "lookups").await?;
    let workflow = user.client.entities.get(id).await?;
    let harness = harness_instance(&user).await?;
    let r1 = generators::gen_workflow_instance(&workflow, 1, None, &[(&gone, &[&tool, &harness])]);
    let r1_id = user
        .client
        .entities
        .create(generators::gen_workflow_instance_req(&r1))
        .await?
        .id;
    // the workflow drops the agent, then the agent is deleted, and its snapshot is still read by
    // name and by id
    let drop_agent = EntityMetadataUpdate::WorkflowCatalog {
        entry_agent: None,
        clear_entry_agent: Some(true),
        auto_instance: None,
        limits: None,
        default_models: None,
        model_auth: None,
        add_agents: Vec::new(),
        remove_agents: vec![gone.id],
        extend_target_groups: false,
    };
    user.client
        .entities
        .update(id, EntityUpdate::default().metadata(drop_agent))
        .await?;
    user.client.entities.delete(gone.id).await?;
    let gone_id = gone.id.to_string();
    for agent_arg in ["gone", gone_id.as_str()] {
        let args = [
            "workflows",
            "toolset",
            "lookups#r1",
            "--agent",
            agent_arg,
            "-o",
            "json",
        ];
        let out = user.run("workflows-toolset-deleted-agent", &args).await;
        assert_success(&out);
        let read = json(&out);
        assert_eq!(read["agent"]["agent"], Value::String(gone_id.clone()));
        assert_eq!(read["agent"]["name"], Value::String("gone".to_owned()));
    }
    // each kind of instance is found by its name
    let tool_name = tool.name.clone();
    for (name, expected) in [("lookups#r1", r1_id), (tool_name.as_str(), tool.id)] {
        let args = ["instances", "describe", name, "-o", "json"];
        let described = json(&user.run("instances-describe-by-name", &args).await);
        assert_eq!(
            described["id"],
            Value::String(expected.to_string()),
            "{name}"
        );
    }
    // a name that names no instance is refused
    let args = ["instances", "describe", "lookups#r9"];
    let out = user.run("instances-describe-missing", &args).await;
    assert!(!out.status.success());
    Ok(())
}

/// Create a workflow named after itself in the user's group with some agents
///
/// # Arguments
///
/// * `user` - The test user
/// * `name` - The workflow's name
/// * `agents` - The names of the agents it includes
async fn workflow_with(user: &TestUser, name: &str, agents: &[&str]) -> Result<Uuid, Error> {
    // create the workflow with each agent, starting with the first
    let mut args = vec!["workflows", "create", name, "-g", &user.group];
    for agent in agents {
        args.extend(["--agent", agent]);
    }
    if let Some(first) = agents.first() {
        args.extend(["--entry", first]);
    }
    assert_success(&user.run(&format!("workflows-create-{name}"), &args).await);
    find_id(user, EntityKinds::WorkflowCatalog, name).await
}

/// `workflows update` changes each setting from flags, `--set`, `--unset`, `--patch` and
/// `--file`, `workflows spec` round trips, and `-o json` pins the result
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn workflows_update_forms() -> Result<(), Error> {
    // a user named after rhea in a group named after ceres, and a second group
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "workflows-update-forms",
        &generators::readable_name("rhea", &suffix),
        &generators::readable_name("ceres", &suffix),
    )
    .await?;
    let other = extra_group(&user, &generators::readable_name("vesta", &suffix)).await?;
    // the agents are in both groups so the workflow can move between them
    let groups = [user.group.as_str(), other.as_str()];
    for name in ["triage", "reverser", "late"] {
        let req = AgentEntityRequest::new(generators::gen_agent_config(name));
        user.client
            .entities
            .create(generators::gen_agent(name, &groups, req))
            .await?;
    }
    let id = workflow_with(&user, "forms", &["triage", "reverser"]).await?;
    // a rename and a description
    let args = [
        "workflows",
        "update",
        "forms",
        "--rename",
        "forms2",
        "-d",
        "triages files",
    ];
    let out = user.run("workflows-update-rename", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("updated"), "{}", stdout(&out));
    let (entity, _) = read_workflow(&user, id).await?;
    assert_eq!(entity.name, "forms2");
    assert_eq!(entity.description.as_deref(), Some("triages files"));
    // a group is added and removed
    let args = ["workflows", "update", "forms2", "--add-group", &other];
    assert_success(&user.run("workflows-update-add-group", &args).await);
    let (entity, _) = read_workflow(&user, id).await?;
    assert!(entity.groups.contains(&other) && entity.groups.contains(&user.group));
    let args = [
        "workflows",
        "update",
        "forms2",
        "--remove-group",
        &user.group,
    ];
    assert_success(&user.run("workflows-update-remove-group", &args).await);
    assert_eq!(
        read_workflow(&user, id).await?.0.groups,
        vec![other.clone()]
    );
    let args = [
        "workflows",
        "update",
        "forms2",
        "--add-group",
        &user.group,
        "--remove-group",
        &other,
    ];
    assert_success(&user.run("workflows-update-swap-groups", &args).await);
    assert_eq!(
        read_workflow(&user, id).await?.0.groups,
        vec![user.group.clone()]
    );
    // the limit, the default models, model auth and the instance switch
    let args = [
        "workflows",
        "update",
        "forms2",
        "--max-alive-sub-agents",
        "6",
        "--default-model",
        "claude-sonnet",
        "--default-model",
        "gpt-5",
        "--model-auth",
        "group",
        "--model-auth",
        "user",
        "--auto-instance",
        "off",
    ];
    assert_success(&user.run("workflows-update-settings", &args).await);
    let (_, meta) = read_workflow(&user, id).await?;
    assert_eq!(meta.limits.max_alive_sub_agents, Some(6));
    let models: Vec<&str> = meta
        .default_models
        .iter()
        .map(|m| m.model_id.as_str())
        .collect();
    assert_eq!(models, vec!["claude-sonnet", "gpt-5"]);
    assert_eq!(
        meta.model_auth.levels.iter().copied().collect::<Vec<_>>(),
        vec![ModelAuthLevel::Group, ModelAuthLevel::User]
    );
    assert!(!meta.auto_instance);
    // the clearing flags undo them
    let args = [
        "workflows",
        "update",
        "forms2",
        "--clear-default-models",
        "--clear-model-auth",
        "--auto-instance",
        "on",
        "--clear-description",
    ];
    assert_success(&user.run("workflows-update-clear", &args).await);
    let (entity, meta) = read_workflow(&user, id).await?;
    assert!(meta.default_models.is_empty() && meta.model_auth.levels.is_empty());
    assert!(meta.auto_instance && entity.description.is_none());
    // a --set with the JSON result pinned
    let args = [
        "workflows",
        "update",
        "forms2",
        "--set",
        "limits.max_alive_sub_agents=3",
        "-o",
        "json",
    ];
    let result = json(&user.run("workflows-update-set-json", &args).await);
    assert_eq!(result["kind"], "workflow");
    assert_eq!(result["name"], "forms2");
    assert_eq!(result["status"], "updated");
    assert_eq!(result["changed"], true);
    assert_eq!(result["diff"][0]["path"], "limits.max_alive_sub_agents");
    assert_eq!(result["diff"][0]["old"], 6);
    assert_eq!(result["diff"][0]["new"], 3);
    assert_eq!(result["resource"]["limits"]["max_alive_sub_agents"], 3);
    // a patch and an unset
    let args = [
        "workflows",
        "update",
        "forms2",
        "--patch",
        r#"{"description": "patched", "limits": {"max_alive_sub_agents": null}}"#,
    ];
    assert_success(&user.run("workflows-update-patch", &args).await);
    let (entity, meta) = read_workflow(&user, id).await?;
    assert_eq!(entity.description.as_deref(), Some("patched"));
    assert_eq!(meta.limits.max_alive_sub_agents, None);
    let args = ["workflows", "update", "forms2", "--unset", "description"];
    assert_success(&user.run("workflows-update-unset", &args).await);
    assert_eq!(read_workflow(&user, id).await?.0.description, None);
    // agents are diffed from the view's names: one added, one removed
    let args = [
        "workflows",
        "update",
        "forms2",
        "--set",
        "agents=[triage, late]",
    ];
    assert_success(&user.run("workflows-update-agents", &args).await);
    let (_, meta) = read_workflow(&user, id).await?;
    assert_eq!(agent_names(&meta), vec!["late", "triage"]);
    // the entry agent moves to another agent of the workflow and is cleared
    let args = ["workflows", "update", "forms2", "--set", "entry_agent=late"];
    assert_success(&user.run("workflows-update-entry", &args).await);
    let (_, meta) = read_workflow(&user, id).await?;
    let late = meta
        .agents
        .iter()
        .find(|agent| agent.name == "late")
        .map(|a| a.id);
    assert_eq!(meta.entry_agent, late);
    let args = ["workflows", "update", "forms2", "--unset", "entry_agent"];
    assert_success(&user.run("workflows-update-clear-entry", &args).await);
    assert_eq!(read_workflow(&user, id).await?.1.entry_agent, None);
    // the spec lists everything by name, and updating with it changes nothing
    let spec = stdout(
        &user
            .run("workflows-spec", &["workflows", "spec", "forms2"])
            .await,
    );
    for expected in [
        "forms2",
        "*id*",
        "late",
        "triage",
        "limits",
        "auto_instance",
    ] {
        assert!(spec.contains(expected), "spec lacks {expected}:\n{spec}");
    }
    let args = ["workflows", "update", "forms2", "--file", "-"];
    let out = run_with(&user, "workflows-update-spec-same", &args, &[], Some(&spec)).await;
    assert_success(&out);
    assert!(stdout(&out).contains("No changes"), "{}", stdout(&out));
    // a changed spec applies from stdin and from a file
    let changed = spec.replace("auto_instance: true", "auto_instance: false");
    let out = run_with(
        &user,
        "workflows-update-spec-stdin",
        &args,
        &[],
        Some(&changed),
    )
    .await;
    assert_success(&out);
    assert!(!read_workflow(&user, id).await?.1.auto_instance);
    let path = write_file("spec.yaml", &spec);
    let args = ["workflows", "update", "forms2", "--file", &path];
    assert_success(&user.run("workflows-update-spec-file", &args).await);
    assert!(read_workflow(&user, id).await?.1.auto_instance);
    // spec as JSON is a document, and -o writes a file
    let args = ["workflows", "spec", "forms2", "--format", "json"];
    let doc = json(&user.run("workflows-spec-json", &args).await);
    assert_eq!(doc["name"], "forms2");
    assert_eq!(doc["agents"][0], "late");
    let out_path = std::env::temp_dir().join(format!("{}-spec.yaml", Uuid::new_v4().simple()));
    let out_str = out_path.to_string_lossy().into_owned();
    let args = ["workflows", "spec", "forms2", "-o", &out_str];
    assert_success(&user.run("workflows-spec-file", &args).await);
    assert!(std::fs::read_to_string(&out_path)?.contains("forms2"));
    Ok(())
}

/// `workflows update` refuses a bad change before sending anything, reports the API's refusal,
/// does nothing for no change or a dry run, and stops when the workflow changed meanwhile
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn workflows_update_refusals() -> Result<(), Error> {
    // a user named after dione in a group named after pallas, and a second group
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "workflows-update-refusals",
        &generators::readable_name("dione", &suffix),
        &generators::readable_name("pallas", &suffix),
    )
    .await?;
    let other = extra_group(&user, &generators::readable_name("hygiea", &suffix)).await?;
    agent(&user, "guard", &[]).await?;
    let id = workflow_with(&user, "guarded", &["guard"]).await?;
    let before = read_workflow(&user, id).await?.1.modified;
    // nothing to update without a change flag
    let out = user
        .run(
            "workflows-update-nothing",
            &["workflows", "update", "guarded"],
        )
        .await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("Nothing to update"));
    // setting the current value changes nothing
    let args = [
        "workflows",
        "update",
        "guarded",
        "--set",
        "auto_instance=true",
    ];
    let out = user.run("workflows-update-no-change", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("No changes"), "{}", stdout(&out));
    // a dry run shows the change and writes nothing
    let args = [
        "workflows",
        "update",
        "guarded",
        "--set",
        "auto_instance=false",
        "--dry-run",
    ];
    let out = user.run("workflows-update-dry-run", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("auto_instance"), "{}", stdout(&out));
    assert!(read_workflow(&user, id).await?.1.auto_instance);
    // a read-only field cannot be set
    let args = ["workflows", "update", "guarded", "--set", "id=x"];
    let out = user.run("workflows-update-read-only", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("read-only"));
    // a document with an unknown key is refused
    let path = write_file("bad.yaml", "colour: red\n");
    let args = ["workflows", "update", "guarded", "--file", &path];
    let out = user.run("workflows-update-unknown-key", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("colour"));
    // an entry agent outside the workflow's agents is refused before any request
    let args = [
        "workflows",
        "update",
        "guarded",
        "--set",
        "entry_agent=stranger",
    ];
    let out = user.run("workflows-update-bad-entry", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("stranger"));
    // an agent that doesn't exist is refused before the update is sent
    let args = [
        "workflows",
        "update",
        "guarded",
        "--set",
        "agents=[guard, ghost]",
    ];
    let out = user.run("workflows-update-unknown-agent", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("ghost"));
    // a repeated agent and a blank name are refused
    let args = [
        "workflows",
        "update",
        "guarded",
        "--set",
        "agents=[guard, guard]",
    ];
    let out = user.run("workflows-update-repeated-agent", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("twice"));
    // the API's refusal is printed: the agent isn't in the group the workflow would join
    let args = ["workflows", "update", "guarded", "--add-group", &other];
    let out = user.run("workflows-update-api-refusal", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("400"));
    assert_eq!(
        read_workflow(&user, id).await?.0.groups,
        vec![user.group.clone()]
    );
    // none of the refused updates changed the workflow
    assert_eq!(read_workflow(&user, id).await?.1.modified, before);
    // a workflow changed between the read and the apply is refused, and --force applies anyway
    let hook_env = hook(&user, "workflows update guarded -d changed-remotely");
    let envs = [("THORCTL_EDIT_BEFORE_APPLY", hook_env.as_str())];
    let args = [
        "workflows",
        "update",
        "guarded",
        "--set",
        "auto_instance=false",
    ];
    let out = run_with(&user, "workflows-update-remote-change", &args, &envs, None).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("changed remotely"));
    assert!(read_workflow(&user, id).await?.1.auto_instance);
    let args = [
        "workflows",
        "update",
        "guarded",
        "--set",
        "auto_instance=false",
        "--force",
    ];
    let out = run_with(&user, "workflows-update-remote-force", &args, &envs, None).await;
    assert_success(&out);
    assert!(!read_workflow(&user, id).await?.1.auto_instance);
    Ok(())
}

/// `workflows edit` in an editor applies a change, shows the header and read-only fields, leaves
/// an unchanged buffer alone, reopens after a bad edit, saves a cancelled bad edit to a recovery
/// file, and reports the API's refusal with the recovery file
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn workflows_edit_interactive() -> Result<(), Error> {
    // a user named after eris in a group named after sylvia, and a second group
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "workflows-edit-interactive",
        &generators::readable_name("eris", &suffix),
        &generators::readable_name("sylvia", &suffix),
    )
    .await?;
    let other = extra_group(&user, &generators::readable_name("flora", &suffix)).await?;
    agent(&user, "editor", &[]).await?;
    let id = workflow_with(&user, "editable", &["editor"]).await?;
    // a change made in the editor is applied, and the buffer had a header and the read-only id
    let script = EditorScript::replace("auto_instance: true", "auto_instance: false");
    let editor = script.editor();
    let args = ["workflows", "edit", "editable", "--editor", &editor];
    let out = run_with(&user, "workflows-edit-change", &args, &EDITOR_ENV, None).await;
    assert_success(&out);
    assert!(stdout(&out).contains("updated"), "{}", stdout(&out));
    assert!(!read_workflow(&user, id).await?.1.auto_instance);
    let seen = script.seen(0).expect("the editor saw no buffer");
    assert!(seen.contains("# Editing workflow 'editable'"), "{seen}");
    assert!(
        seen.contains("*id*") && seen.contains("read-only"),
        "{seen}"
    );
    assert!(seen.contains("entry_agent: editor"), "{seen}");
    // an unchanged buffer changes nothing
    let script = EditorScript::noop();
    let editor = script.editor();
    let args = ["workflows", "edit", "editable", "--editor", &editor];
    let out = run_with(&user, "workflows-edit-noop", &args, &EDITOR_ENV, None).await;
    assert_success(&out);
    assert!(stdout(&out).contains("No changes"), "{}", stdout(&out));
    // an empty buffer cancels
    let script = EditorScript::write("");
    let editor = script.editor();
    let args = ["workflows", "edit", "editable", "--editor", &editor];
    let out = run_with(&user, "workflows-edit-cancel", &args, &EDITOR_ENV, None).await;
    assert_success(&out);
    assert!(stdout(&out).contains("Cancelled"), "{}", stdout(&out));
    // editing the read-only id is reported and ignored
    let script = EditorScript::replace(&id.to_string(), "00000000-0000-0000-0000-000000000000");
    let editor = script.editor();
    let args = ["workflows", "edit", "editable", "--editor", &editor];
    let out = run_with(&user, "workflows-edit-read-only", &args, &EDITOR_ENV, None).await;
    assert_success(&out);
    assert!(String::from_utf8_lossy(&out.stderr).contains("read-only"));
    assert!(stdout(&out).contains("No changes"), "{}", stdout(&out));
    // a bad edit reopens the editor with the user's text, and a fix succeeds
    let script = EditorScript::sequence(vec![
        Step::append("colour: red"),
        Step::replace("colour: red", "# removed"),
    ]);
    let editor = script.editor();
    let envs = [
        ("THORCTL_EDIT_ASSUME_TTY", "1"),
        ("THORCTL_EDIT_PROMPT", "reedit"),
    ];
    let args = ["workflows", "edit", "editable", "--editor", &editor];
    let out = run_with(&user, "workflows-edit-reedit", &args, &envs, None).await;
    assert_success(&out);
    assert_eq!(script.invocations(), 2);
    assert!(String::from_utf8_lossy(&out.stderr).contains("colour"));
    // an entry agent that isn't one of the agents reopens too, and is never sent
    let script = EditorScript::replace("entry_agent: editor", "entry_agent: stranger");
    let editor = script.editor();
    let args = ["workflows", "edit", "editable", "--editor", &editor];
    let out = run_with(&user, "workflows-edit-bad-entry", &args, &EDITOR_ENV, None).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(stderr.contains("stranger"), "{stderr}");
    if let Some(path) = recovery_path(&stderr) {
        std::fs::remove_file(path)?;
    }
    // cancelling after a bad edit fails and saves the text to a recovery file
    let script = EditorScript::append("colour: red");
    let editor = script.editor();
    let args = ["workflows", "edit", "editable", "--editor", &editor];
    let out = run_with(&user, "workflows-edit-bad-cancel", &args, &EDITOR_ENV, None).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let path = recovery_path(&stderr).expect("no recovery file named");
    assert!(std::fs::read_to_string(&path)?.contains("colour: red"));
    std::fs::remove_file(&path)?;
    // a change the API refuses is reported with its reason and the recovery file
    let script = EditorScript::write(&format!(
        "name: editable\ngroups: [{}, {other}]\nagents: [editor]\nentry_agent: editor\n\
         limits: {{}}\ndefault_models: []\nauto_instance: true\n",
        user.group
    ));
    let editor = script.editor();
    let args = ["workflows", "edit", "editable", "--editor", &editor];
    let out = run_with(
        &user,
        "workflows-edit-api-refusal",
        &args,
        &EDITOR_ENV,
        None,
    )
    .await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(stderr.contains("400"), "{stderr}");
    let path = recovery_path(&stderr).expect("no recovery file named");
    std::fs::remove_file(&path)?;
    // a dry run shows the changes and writes nothing
    let script = EditorScript::replace("auto_instance: false", "auto_instance: true");
    let editor = script.editor();
    let args = [
        "workflows",
        "edit",
        "editable",
        "--editor",
        &editor,
        "--dry-run",
    ];
    let out = run_with(&user, "workflows-edit-dry-run", &args, &EDITOR_ENV, None).await;
    assert_success(&out);
    assert!(stdout(&out).contains("auto_instance"), "{}", stdout(&out));
    assert!(!read_workflow(&user, id).await?.1.auto_instance);
    // declining the confirmation applies nothing and saves the edit
    let declined = [
        ("THORCTL_EDIT_ASSUME_TTY", "1"),
        ("THORCTL_EDIT_PROMPT", "cancel"),
    ];
    let editor = script.editor();
    let args = ["workflows", "edit", "editable", "--editor", &editor];
    let out = run_with(&user, "workflows-edit-declined", &args, &declined, None).await;
    assert_success(&out);
    assert!(stdout(&out).contains("Cancelled"), "{}", stdout(&out));
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let path = recovery_path(&stderr.replace("Your edits were saved to", "saved to"))
        .expect("no recovery file named");
    std::fs::remove_file(&path)?;
    assert!(!read_workflow(&user, id).await?.1.auto_instance);
    // a workflow that changed during the edit is refused unless forced
    let hook_env = hook(&user, "workflows update editable -d changed-remotely");
    let envs = [
        ("THORCTL_EDIT_ASSUME_TTY", "1"),
        ("THORCTL_EDIT_BEFORE_APPLY", hook_env.as_str()),
    ];
    let args = ["workflows", "edit", "editable", "--editor", &editor];
    let out = run_with(&user, "workflows-edit-remote-change", &args, &envs, None).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(stderr.contains("changed remotely"), "{stderr}");
    if let Some(path) = recovery_path(&stderr) {
        std::fs::remove_file(path)?;
    }
    let args = [
        "workflows",
        "edit",
        "editable",
        "--editor",
        &editor,
        "--force",
    ];
    let out = run_with(&user, "workflows-edit-remote-force", &args, &envs, None).await;
    assert_success(&out);
    assert!(read_workflow(&user, id).await?.1.auto_instance);
    Ok(())
}

/// An agent that misses one of the workflow's groups is refused, and `--extend-target-groups`
/// adds the group to the agent instead
#[tokio::test]
async fn workflows_update_extends_target_groups() -> Result<(), Error> {
    // a user named after selene in a group named after luna, and a second group
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "workflows-update-extend",
        &generators::readable_name("selene", &suffix),
        &generators::readable_name("luna", &suffix),
    )
    .await?;
    let other = extra_group(&user, &generators::readable_name("terra", &suffix)).await?;
    // a workflow in both groups and an agent in only one of them
    let args = [
        "workflows",
        "create",
        "wide",
        "-g",
        &user.group,
        "-g",
        &other,
    ];
    assert_success(&user.run("workflows-create-wide", &args).await);
    let id = find_id(&user, EntityKinds::WorkflowCatalog, "wide").await?;
    let narrow = agent(&user, "narrow", &[]).await?;
    // the agent is refused with the API's reason
    let args = ["workflows", "update", "wide", "--set", "agents=[narrow]"];
    let out = user.run("workflows-update-missing-group", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("isn't in groups"));
    assert_eq!(
        agent_names(&read_workflow(&user, id).await?.1),
        Vec::<String>::new()
    );
    // the flag widens the agent's groups and adds it
    let args = [
        "workflows",
        "update",
        "wide",
        "--set",
        "agents=[narrow]",
        "--extend-target-groups",
    ];
    assert_success(&user.run("workflows-update-extend", &args).await);
    assert_eq!(
        agent_names(&read_workflow(&user, id).await?.1),
        vec!["narrow"]
    );
    assert!(
        user.client
            .entities
            .get(narrow.id)
            .await?
            .groups
            .contains(&other)
    );
    Ok(())
}

/// `workflows create --spec` builds a workflow from a printed spec on stdin or in a file, flags
/// override the spec, and a bad spec fails before anything is created
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn workflows_create_from_spec() -> Result<(), Error> {
    // a user named after leda in a group named after maia
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "workflows-create-spec",
        &generators::readable_name("leda", &suffix),
        &generators::readable_name("maia", &suffix),
    )
    .await?;
    let first = agent(&user, "first", &[]).await?;
    let second = agent(&user, "second", &[]).await?;
    // a workflow with two agents, an entry agent and every setting changed
    let args = [
        "workflows",
        "create",
        "original",
        "-g",
        &user.group,
        "--agent",
        "first",
        "--agent",
        "second",
        "--entry",
        "second",
        "--default-model",
        "claude-sonnet",
        "--max-alive-sub-agents",
        "4",
        "--model-auth",
        "user",
        "--no-auto-instance",
    ];
    assert_success(&user.run("workflows-create-original", &args).await);
    let spec = stdout(
        &user
            .run(
                "workflows-spec-original",
                &["workflows", "spec", "original"],
            )
            .await,
    );
    // the spec on stdin creates a copy under another name, with the same settings and agents
    let spec = spec.replace("name: original", "name: copy");
    let args = ["workflows", "create", "--spec", "-"];
    let out = run_with(
        &user,
        "workflows-create-spec-stdin",
        &args,
        &[],
        Some(&spec),
    )
    .await;
    assert_success(&out);
    assert!(
        stdout(&out).contains("Created workflow copy"),
        "{}",
        stdout(&out)
    );
    let copy = find_id(&user, EntityKinds::WorkflowCatalog, "copy").await?;
    let (entity, meta) = read_workflow(&user, copy).await?;
    assert_eq!(entity.groups, vec![user.group.clone()]);
    assert_eq!(agent_names(&meta), vec!["first", "second"]);
    assert_eq!(meta.entry_agent, Some(second.id));
    assert_eq!(meta.limits.max_alive_sub_agents, Some(4));
    assert_eq!(meta.default_models[0].model_id, "claude-sonnet");
    assert!(!meta.auto_instance);
    assert_eq!(
        meta.model_auth.levels.iter().copied().collect::<Vec<_>>(),
        vec![ModelAuthLevel::User]
    );
    // the same spec from a file, with a name, a description and an entry agent from flags
    let path = write_file("spec.yaml", &spec);
    let args = [
        "workflows",
        "create",
        "second-copy",
        "--spec",
        &path,
        "-d",
        "made from a spec",
        "--entry",
        "first",
    ];
    assert_success(&user.run("workflows-create-spec-file", &args).await);
    let second_copy = find_id(&user, EntityKinds::WorkflowCatalog, "second-copy").await?;
    let (entity, meta) = read_workflow(&user, second_copy).await?;
    assert_eq!(entity.description.as_deref(), Some("made from a spec"));
    assert_eq!(meta.entry_agent, Some(first.id));
    // a flag agent joins the spec's agents, and agents can be given by ID
    let third = agent(&user, "third", &[]).await?;
    let third_id = third.id.to_string();
    let args = [
        "workflows",
        "create",
        "third-copy",
        "--spec",
        &path,
        "--agent",
        &third_id,
    ];
    assert_success(&user.run("workflows-create-spec-agent-id", &args).await);
    let third_copy = find_id(&user, EntityKinds::WorkflowCatalog, "third-copy").await?;
    assert_eq!(
        agent_names(&read_workflow(&user, third_copy).await?.1),
        vec!["first", "second", "third"]
    );
    // a spec with an unknown key fails before anything is created
    let bad = write_file("bad.yaml", "name: broken\ngroups: [x]\ncolour: red\n");
    let args = ["workflows", "create", "--spec", &bad];
    let out = user.run("workflows-create-spec-bad", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("colour"));
    // a spec that names an agent that doesn't exist fails the same way
    let missing = write_file(
        "missing.yaml",
        &format!(
            "name: ghost\ngroups: [{}]\nagents: [no-such-agent]\n",
            user.group
        ),
    );
    let args = ["workflows", "create", "--spec", &missing];
    let out = user.run("workflows-create-spec-missing-agent", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no-such-agent"));
    // an entry agent that isn't one of the agents is refused before any request
    let stray = write_file(
        "stray.yaml",
        &format!(
            "name: stray\ngroups: [{}]\nagents: [first]\nentry_agent: second\n",
            user.group
        ),
    );
    let args = ["workflows", "create", "--spec", &stray];
    let out = user.run("workflows-create-spec-bad-entry", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("entry"));
    let opts = EntityListOpts::default()
        .groups(vec![user.group.clone()])
        .kind(EntityKinds::WorkflowCatalog)
        .limit(100);
    let listed = user.client.entities.list(&opts).await?;
    assert!(
        !listed
            .data
            .iter()
            .any(|line| ["ghost", "broken", "stray"].contains(&line.name.as_str()))
    );
    // a spec file that doesn't exist is named in the error
    let args = ["workflows", "create", "--spec", "/no/such/spec.yaml"];
    let out = user.run("workflows-create-spec-no-file", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("/no/such/spec.yaml"));
    Ok(())
}

/// `workflows delete` does nothing for a dry run, needs `--skip-confirm` without a terminal, and
/// deletes the workflow (which has an instance) otherwise
#[tokio::test]
async fn workflows_delete() -> Result<(), Error> {
    // a user named after thalia in a group named after clio, and a workflow with an instance
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "workflows-delete",
        &generators::readable_name("thalia", &suffix),
        &generators::readable_name("clio", &suffix),
    )
    .await?;
    let (tool_root, tool) = agentic_tool(&user).await?;
    let kept = agent(&user, "kept", &[tool_root.id]).await?;
    let id = workflow_with(&user, "doomed", &["kept"]).await?;
    let workflow = user.client.entities.get(id).await?;
    let harness = harness_instance(&user).await?;
    let r1 = generators::gen_workflow_instance(&workflow, 1, None, &[(&kept, &[&tool, &harness])]);
    user.client
        .entities
        .create(generators::gen_workflow_instance_req(&r1))
        .await?;
    // a dry run names what would be deleted and keeps the workflow
    let args = ["workflows", "delete", "doomed", "--dry-run"];
    let out = user.run("workflows-delete-dry-run", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("doomed"), "{}", stdout(&out));
    assert!(user.client.entities.get(id).await.is_ok());
    // without a terminal the command asks for the flag instead of prompting
    let args = ["workflows", "delete", "doomed"];
    let out = user.run("workflows-delete-no-tty", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--skip-confirm"));
    assert!(user.client.entities.get(id).await.is_ok());
    // with the flag the workflow is deleted, and its agent is kept
    let args = ["workflows", "delete", "doomed", "--yes"];
    let out = user.run("workflows-delete", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("Deleted"), "{}", stdout(&out));
    assert!(user.client.entities.get(id).await.is_err());
    assert!(user.client.entities.get(kept.id).await.is_ok());
    // deleting it again says there is no such workflow
    let out = user.run("workflows-delete-again", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("doomed"));
    Ok(())
}

/// `instances request` asks for a new workflow or toolset instance, prints the revision to wait
/// for, can wait for the instance, times out with exit code 3, and refuses anything else
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn instances_request() -> Result<(), Error> {
    // a user named after callisto in a group named after tethys, with a workflow and a toolset
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "instances-request",
        &generators::readable_name("callisto", &suffix),
        &generators::readable_name("tethys", &suffix),
    )
    .await?;
    let (tool_root, tool) = agentic_tool(&user).await?;
    let triage = agent(&user, "triage", &[tool_root.id]).await?;
    let id = workflow_with(&user, "requested", &["triage"]).await?;
    let args = ["toolsets", "create", "kit", "-g", &user.group];
    assert_success(&user.run("toolsets-create-kit", &args).await);
    // a request is accepted and names the revision to wait for
    let args = ["instances", "request", "workflow", "requested"];
    let out = user.run("instances-request", &args).await;
    assert_success(&out);
    let text = stdout(&out);
    assert!(
        text.contains("Requested a new instance of workflow requested"),
        "{text}"
    );
    assert!(text.contains("instance-revision=1"), "{text}");
    // the JSON result is a document with the same revisions
    let args = [
        "instances",
        "request",
        "workflow",
        "requested",
        "-o",
        "json",
    ];
    let result = json(&user.run("instances-request-json", &args).await);
    assert_eq!(result["root_name"], "requested");
    assert_eq!(result["root"], Value::String(id.to_string()));
    assert_eq!(result["requested"], true);
    assert_eq!(result["next_revision"], 1);
    // a toolset is requested the same way, and a dry run says it requested nothing
    let args = ["instances", "request", "toolset", "kit"];
    let out = user.run("instances-request-toolset", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("toolset kit"), "{}", stdout(&out));
    let args = [
        "instances",
        "request",
        "workflow",
        "requested",
        "--dry-run",
        "-o",
        "json",
    ];
    let result = json(&user.run("instances-request-dry-run", &args).await);
    assert_eq!(result["requested"], false);
    // a name of another kind of entity, and a name that doesn't exist, are refused
    let args = ["instances", "request", "workflow", "triage"];
    let out = user.run("instances-request-not-a-workflow", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("triage"));
    let args = ["instances", "request", "toolset", "requested"];
    let out = user.run("instances-request-not-a-toolset", &args).await;
    assert!(!out.status.success());
    let args = ["instances", "request", "workflow", "no-such-workflow"];
    let out = user.run("instances-request-missing", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no-such-workflow"));
    // a wait that outlasts its timeout exits 3
    let args = [
        "instances",
        "request",
        "workflow",
        "requested",
        "--wait-for",
        "instance-revision=99",
        "--interval",
        "200ms",
        "--timeout",
        "2s",
    ];
    let out = user.run("instances-request-wait-timeout", &args).await;
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("Requested a new instance"));
    // a wait ends once the instance exists, and prints the instances
    let workflow = user.client.entities.get(id).await?;
    let harness = harness_instance(&user).await?;
    let r1 =
        generators::gen_workflow_instance(&workflow, 1, None, &[(&triage, &[&tool, &harness])]);
    let args = [
        "instances",
        "request",
        "workflow",
        "requested",
        "--wait-for",
        "instance-revision=1",
        "--interval",
        "500ms",
        "--timeout",
        "60s",
        "-o",
        "json",
    ];
    let waiting = user.spawn(&args);
    tokio::time::sleep(Duration::from_secs(3)).await;
    let r1_id = user
        .client
        .entities
        .create(generators::gen_workflow_instance_req(&r1))
        .await?
        .id;
    let out = tokio::time::timeout(Duration::from_secs(90), waiting.wait_with_output())
        .await
        .expect("instances request didn't return")?;
    user.record("instances-request-wait", &args, &out);
    assert_success(&out);
    let waited = json(&out);
    assert_eq!(waited["instance_revision"], Value::from(1));
    assert_eq!(waited["latest_instance"], Value::String(r1_id.to_string()));
    Ok(())
}

/// Create a ready workflow in the user's group whose agent calls one container tool
///
/// # Arguments
///
/// * `user` - The test user
/// * `name` - The workflow's name
async fn ready_workflow(user: &TestUser, name: &str) -> Result<runs::RunWorkflow, Error> {
    // an Image tool and a container tool that runs it
    let groups = [user.group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &user.admin).await?;
    let (_, boxed) = runs::container_tool(&groups, image.id, &user.admin).await?;
    runs::ready_workflow(name, &groups, &[(&boxed, ToolMechanism::Cli)], &user.admin).await
}

/// Get the ids of a workflow instance's runs in the user's group
///
/// # Arguments
///
/// * `user` - The test user
/// * `instance` - The workflow instance
async fn instance_runs(user: &TestUser, instance: Uuid) -> Result<Vec<Uuid>, Error> {
    // runs are tagged with their instance
    let opts = ReactionCursorOpts::default().group(&user.group);
    let cursor = user
        .client
        .reactions
        .list_tag_new(&format!("WorkflowInstance={instance}"), &opts)
        .await?;
    Ok(cursor
        .data
        .iter()
        .filter_map(|run| Uuid::parse_str(run).ok())
        .collect())
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

/// Wait for a workflow instance to have a run that isn't among some known runs
///
/// # Arguments
///
/// * `user` - The test user
/// * `instance` - The workflow instance
/// * `known` - The runs already seen
async fn new_run(user: &TestUser, instance: Uuid, known: &[Uuid]) -> Result<Uuid, Error> {
    // poll the instance's runs until one is new
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(run) = instance_runs(user, instance)
            .await?
            .into_iter()
            .find(|run| !known.contains(run))
        {
            return Ok(run);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "workflows run never created a run"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Complete a run the way its worker would: store a result with one file under the run's key and
/// proceed the run
///
/// # Arguments
///
/// * `user` - The test user
/// * `run` - The run's reaction
/// * `sha256` - The file the run ran on
async fn complete_run(user: &TestUser, run: Uuid, sha256: &str) -> Result<(), Error> {
    // the run stores its results under its key
    let reaction = user.client.reactions.get(&user.group, run).await?;
    let key = reaction.result_key().expect("a run has a result key");
    let dir = std::env::temp_dir().join(common::unique("workflow-run-result-"));
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("report.txt"), "triaged")?;
    let req = OutputRequest::new(
        sha256.to_owned(),
        key,
        "{\"answer\": \"benign\"}",
        OutputDisplayType::Json,
    )
    .file(thorium::models::OnDiskFile::new(dir.join("report.txt")).trim_prefix(&dir));
    user.client.files.create_result(req).await?;
    std::fs::remove_dir_all(&dir)?;
    // the run completes as its creator
    let (token, _) = Thorium::auth(
        &user.admin.host,
        &user.username,
        &user.password,
        &reqwest::Client::new(),
    )
    .await?;
    let auth = test_utilities::auth_header(&token);
    runs::handle_reaction(&user.client, &auth, &user.group, run, "proceed").await
}

/// `workflows run` takes a workflow name for its latest instance, `NAME#rN` for one revision,
/// and either id, tells which instance it picked, follows the run with `--wait` and `--output`,
/// and refuses a workflow with no instance or an unknown name
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn workflows_run_by_name() -> Result<(), Error> {
    // a user named after pan in a group named after ceres, a ready workflow and a second revision
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "workflows-run-by-name",
        &generators::readable_name("pan", &suffix),
        &generators::readable_name("ceres", &suffix),
    )
    .await?;
    let named = ready_workflow(&user, "Named").await?;
    let boxed = named.tools[0].clone();
    let r2 = runs::submit_instance(
        &named.workflow,
        2,
        Some(named.id()),
        &named.agent,
        &named.harness,
        &[(&boxed, ToolMechanism::Cli)],
        &user.admin,
    )
    .await?;
    let first = sample(&user).await?;
    // a bare name runs the latest instance and says which one it picked
    let args = ["workflows", "run", "Named", "--sample", &first];
    let out = user.run("workflows-run-by-name", &args).await;
    assert_success(&out);
    let started = Uuid::parse_str(stdout(&out).trim()).expect("thorctl didn't print a reaction id");
    assert!(String::from_utf8_lossy(&out.stderr).contains(&r2.id.to_string()));
    assert_eq!(instance_runs(&user, r2.id).await?, vec![started]);
    assert_eq!(instance_runs(&user, named.id()).await?, Vec::<Uuid>::new());
    // a revision name runs that revision
    let args = ["workflows", "run", "Named#r1", "--sample", &first];
    let out = user.run("workflows-run-by-revision", &args).await;
    assert_success(&out);
    let by_revision =
        Uuid::parse_str(stdout(&out).trim()).expect("thorctl didn't print a reaction id");
    assert_eq!(instance_runs(&user, named.id()).await?, vec![by_revision]);
    // the workflow's id runs its latest instance, and an instance's id runs that instance
    let workflow_id = named.workflow.id.to_string();
    let args = ["workflows", "run", &workflow_id, "--sample", &first];
    assert_success(&user.run("workflows-run-by-workflow-id", &args).await);
    assert_eq!(instance_runs(&user, r2.id).await?.len(), 2);
    let instance_id = named.id().to_string();
    let args = ["workflows", "run", &instance_id, "--sample", &first];
    assert_success(&user.run("workflows-run-by-instance-id", &args).await);
    assert_eq!(instance_runs(&user, named.id()).await?.len(), 2);
    // a name that names no workflow or instance is refused with the workflow lookup's reason
    let args = ["workflows", "run", "Nobody", "--sample", &first];
    let out = user.run("workflows-run-unknown", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("Nobody"));
    // a workflow with no instance says how to get one, and creates no run
    agent(&user, "idle", &[]).await?;
    workflow_with(&user, "Fresh", &["idle"]).await?;
    let args = ["workflows", "run", "Fresh", "--sample", &first];
    let out = user.run("workflows-run-no-instance", &args).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        stderr.contains("instances request workflow Fresh"),
        "{stderr}"
    );
    // a wait on a run by name follows it: it times out with exit 3 when nothing finishes it
    let args = [
        "workflows",
        "run",
        "Named",
        "--sample",
        &first,
        "--wait",
        "--timeout",
        "1s",
        "--interval",
        "200ms",
    ];
    let out = user.run("workflows-run-by-name-timeout", &args).await;
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("Timed out"));
    // a wait that ends with the run completed downloads its results and exits 0
    let known = instance_runs(&user, r2.id).await?;
    let dir = std::env::temp_dir().join(common::unique("workflow-run-by-name-output-"));
    let dir_arg = dir.to_string_lossy().into_owned();
    let args = [
        "workflows",
        "run",
        "Named",
        "--sample",
        &first,
        "--wait",
        "--output",
        &dir_arg,
        "--timeout",
        "60s",
        "--interval",
        "200ms",
    ];
    let waiting = user.spawn(&args);
    let run = new_run(&user, r2.id, &known).await?;
    complete_run(&user, run, &first).await?;
    let out = tokio::time::timeout(Duration::from_secs(90), waiting.wait_with_output())
        .await
        .expect("workflows run --wait didn't return")?;
    user.record("workflows-run-by-name-completed", &args, &out);
    assert_success(&out);
    assert!(stdout(&out).contains("Completed"), "{}", stdout(&out));
    let downloaded: Vec<_> = std::fs::read_dir(&dir)?.collect::<Result<_, _>>()?;
    assert_eq!(downloaded.len(), 1, "{downloaded:?}");
    assert_eq!(
        std::fs::read_to_string(downloaded[0].path().join("report.txt"))?,
        "triaged"
    );
    std::fs::remove_dir_all(&dir)?;
    Ok(())
}
