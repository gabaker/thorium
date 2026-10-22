//! Tests the thorctl workflows and instances commands against the test API

mod common;

use common::{TestUser, assert_success, json, stdout};
use serde_json::Value;
use std::time::Duration;
use thorium::Error;
use thorium::models::{
    AgentEntityRequest, Entity, EntityKinds, EntityListOpts, ToolSetEntityRequest,
    ToolSetExclusion, ToolSetExclusionReason, ToolSetInclusion, WorkflowInstanceStatus,
};
use thorium::test_utilities::generators;
use uuid::Uuid;

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
    // the agent is deleted, and its snapshot is still read by name and by id
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
