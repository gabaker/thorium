//! Tests the thorctl agents commands against the test API

mod common;

use common::{TestUser, assert_success, json, stdout};
use serde_json::Value;
use thorium::Error;
use thorium::models::{
    AgentConfig, AssociationKind, Directionality, EntityKinds, EntityListOpts, EntityMetadata,
    ModelCapabilities, ModelMatcher, ModelRequirements,
};
use thorium::test_utilities::generators;
use uuid::Uuid;

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

/// Get the ids of an agent's outgoing edges of one kind
///
/// # Arguments
///
/// * `user` - The test user whose admin client to read with
/// * `agent` - The agent to read the edges of
/// * `kind` - The kind of edge
async fn out_edges(
    user: &TestUser,
    agent: Uuid,
    kind: AssociationKind,
) -> Result<Vec<Uuid>, Error> {
    // list the agent's associations and keep its outgoing edges of this kind
    let mut ids = user
        .admin
        .entities
        .associations(agent)
        .await?
        .into_iter()
        .filter(|assoc| assoc.kind == kind && assoc.direction == Directionality::To)
        .filter_map(|assoc| match assoc.other {
            thorium::models::AssociationTarget::Entity { id, .. } => Some(id),
            _ => None,
        })
        .collect::<Vec<Uuid>>();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// Find the id of an agent by name in the user's group
///
/// # Arguments
///
/// * `user` - The test user
/// * `name` - The agent's name
async fn agent_id(user: &TestUser, name: &str) -> Result<Option<Uuid>, Error> {
    // list the agents in the user's group and find the name
    let opts = EntityListOpts::default()
        .groups(vec![user.group.clone()])
        .kind(EntityKinds::Agent)
        .limit(100);
    let listed = user.client.entities.list(&opts).await?;
    Ok(listed
        .data
        .iter()
        .find(|line| line.name == name)
        .map(|line| line.id))
}

/// A config with accepted models and model requirements
fn lead_config() -> AgentConfig {
    // a lead agent that accepts two models that can call tools
    AgentConfig {
        system_prompt: Some("You lead a malware triage".into()),
        models: vec![
            ModelMatcher {
                model_id: "claude-opus".into(),
                ..ModelMatcher::default()
            },
            ModelMatcher {
                model_id: "gpt-5".into(),
                ..ModelMatcher::default()
            },
        ],
        model_requirements: ModelRequirements {
            min_context_window: Some(100_000),
            capabilities: ModelCapabilities {
                tool_calling: true,
                ..ModelCapabilities::default()
            },
            ..ModelRequirements::default()
        },
        function_name: Some("triage_lead".into()),
        model_description: Some("Leads a triage".into()),
        ..AgentConfig::default()
    }
}

/// `agents set-harness` runs an agent in a harness at its latest version or pinned to one, and
/// on an agent that already has a harness prints the API's refusal and keeps the harness
#[tokio::test]
async fn agents_set_harness() -> Result<(), Error> {
    // a user named after callisto in a group named after vesta, with two agents and a harness
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "agents-set-harness",
        &generators::readable_name("callisto", &suffix),
        &generators::readable_name("vesta", &suffix),
    )
    .await?;
    for name in ["follower", "pinner"] {
        let path = write_file(
            &format!("{name}.json"),
            &serde_json::to_string(&generators::gen_agent_config(name))?,
        );
        let args = [
            "agents",
            "create",
            "-n",
            name,
            "-g",
            &user.group,
            "--config",
            &path,
        ];
        assert_success(&user.run(&format!("agents-create-{name}"), &args).await);
    }
    let follower = agent_id(&user, "follower").await?.expect("no follower");
    let pinner = agent_id(&user, "pinner").await?.expect("no pinner");
    let (runtime, runtime_r1) = generators::tool(
        &[&user.group],
        "1.0.0",
        &generators::gen_harness_spec(),
        &user.client,
    )
    .await?;
    // set-harness without a pin follows the harness at latest
    let args = ["agents", "set-harness", "follower", &runtime.name];
    let out = user.run("agents-set-harness", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("latest"), "{}", stdout(&out));
    assert_eq!(
        out_edges(&user, follower, AssociationKind::RunsIn).await?,
        vec![runtime.id]
    );
    // set-harness with a pin runs the agent in that instance
    let pin = runtime_r1.id.to_string();
    let args = [
        "agents",
        "set-harness",
        "pinner",
        &runtime.name,
        "--pin",
        &pin,
    ];
    let out = user.run("agents-set-harness-pin", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains(&runtime_r1.name), "{}", stdout(&out));
    assert_eq!(
        out_edges(&user, pinner, AssociationKind::RunsIn).await?,
        vec![runtime_r1.id]
    );
    // a second harness prints the API's refusal naming the current one and keeps it
    let (other, _) = generators::tool(
        &[&user.group],
        "1.0.0",
        &generators::gen_harness_spec(),
        &user.client,
    )
    .await?;
    let args = ["agents", "set-harness", "follower", &other.name];
    let out = user.run("agents-set-harness-second", &args).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("400") && stderr.contains(&runtime.name),
        "{stderr}"
    );
    assert_eq!(
        out_edges(&user, follower, AssociationKind::RunsIn).await?,
        vec![runtime.id]
    );
    // a pin that isn't a version of the named harness fails before any edge is written
    let args = [
        "agents",
        "set-harness",
        "follower",
        &other.name,
        "--pin",
        &pin,
    ];
    let out = user.run("agents-set-harness-wrong-pin", &args).await;
    assert!(!out.status.success());
    assert_eq!(
        out_edges(&user, follower, AssociationKind::RunsIn).await?,
        vec![runtime.id]
    );
    // describe names the harness
    let out = user
        .run("agents-describe-harness", &["agents", "describe", "pinner"])
        .await;
    assert_success(&out);
    assert!(stdout(&out).contains(&runtime_r1.name), "{}", stdout(&out));
    Ok(())
}

/// `agents create` reads a config file, `describe` shows the agent, and `link-tool` and
/// `delegate` create the right edges
#[tokio::test]
// one scenario walks every case in turn against shared fixtures
#[allow(clippy::too_many_lines)]
async fn agents_create_link_and_delegate() -> Result<(), Error> {
    // a user named after io in a group named after minerva
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "agents-create-link-delegate",
        &generators::readable_name("io", &suffix),
        &generators::readable_name("minerva", &suffix),
    )
    .await?;
    // a config with models and requirements written as YAML
    let config = lead_config();
    let yaml = serde_norway::to_string(&serde_json::to_value(&config)?).unwrap();
    let path = write_file("lead.yaml", &yaml);
    let args = [
        "agents",
        "create",
        "-n",
        "triage-lead",
        "-g",
        &user.group,
        "--config",
        &path,
    ];
    assert_success(&user.run("agents-create", &args).await);
    // the agent's config equals the file's
    let lead = agent_id(&user, "triage-lead")
        .await?
        .expect("the agent wasn't created");
    let read = user.client.entities.get(lead).await?;
    let EntityMetadata::Agent(meta) = &read.metadata else {
        panic!("expected an agent");
    };
    assert_eq!(meta.config, config);
    // a sub-agent from a JSON config
    let sub_config = generators::gen_agent_config("researcher");
    let sub_path = write_file("sub.json", &serde_json::to_string(&sub_config)?);
    let args = [
        "agents",
        "create",
        "-n",
        "researcher",
        "-g",
        &user.group,
        "--config",
        &sub_path,
    ];
    assert_success(&user.run("agents-create-json", &args).await);
    let sub = agent_id(&user, "researcher")
        .await?
        .expect("the sub-agent wasn't created");
    // link one tool at latest and pin another tool's instance
    let (tool, _) = generators::tool(
        &[&user.group],
        "1.0.0",
        &generators::gen_agentic_spec(),
        &user.client,
    )
    .await?;
    let (pinned_tool, pinned) = generators::tool(
        &[&user.group],
        "1.0.0",
        &generators::gen_agentic_spec(),
        &user.client,
    )
    .await?;
    let tool_id = tool.id.to_string();
    let args = ["agents", "link-tool", "triage-lead", &tool_id];
    assert_success(&user.run("agents-link-tool", &args).await);
    let pinned_tool_id = pinned_tool.id.to_string();
    let pinned_id = pinned.id.to_string();
    let args = [
        "agents",
        "link-tool",
        "triage-lead",
        &pinned_tool_id,
        "--pin",
        &pinned_id,
    ];
    assert_success(&user.run("agents-link-tool-pin", &args).await);
    let mut expected = vec![tool.id, pinned.id];
    expected.sort();
    assert_eq!(
        out_edges(&user, lead, AssociationKind::Calls).await?,
        expected
    );
    // delegate to the sub-agent
    let args = ["agents", "delegate", "triage-lead", "researcher"];
    assert_success(&user.run("agents-delegate", &args).await);
    assert_eq!(
        out_edges(&user, lead, AssociationKind::DelegatesTo).await?,
        vec![sub]
    );
    // describe shows the models, tools, toolsets, sub-agents, and harness
    let out = user
        .run("agents-describe", &["agents", "describe", "triage-lead"])
        .await;
    assert_success(&out);
    let text = stdout(&out);
    for expected in [
        "claude-opus",
        "gpt-5",
        tool.name.as_str(),
        pinned.name.as_str(),
        "researcher",
        "Toolsets",
        "Harness",
    ] {
        assert!(
            text.contains(expected),
            "describe is missing {expected}:\n{text}"
        );
    }
    // describe as JSON prints the agent the client read
    let args = ["agents", "describe", "triage-lead", "-o", "json"];
    let described = json(&user.run("agents-describe-json", &args).await);
    assert_eq!(described["id"], Value::String(lead.to_string()));
    assert_eq!(
        described["metadata"]["Agent"]["sub_agents"][0]["id"],
        Value::String(sub.to_string())
    );
    // get lists both agents
    let args = ["agents", "get", "-g", &user.group, "-o", "json"];
    let listed = json(&user.run("agents-get-json", &args).await);
    assert_eq!(listed.as_array().map(Vec::len), Some(2));
    Ok(())
}

/// Delegating to something that isn't an agent prints the API's 400, an unreadable config fails
/// before any request, and the help text says AI agents
#[tokio::test]
async fn agents_refusals_and_help() -> Result<(), Error> {
    // a user and an agent in their group
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "agents-refusals",
        &generators::readable_name("europa", &suffix),
        &generators::readable_name("mars", &suffix),
    )
    .await?;
    let path = write_file(
        "agent.json",
        &serde_json::to_string(&generators::gen_agent_config("lonely"))?,
    );
    let args = [
        "agents",
        "create",
        "-n",
        "lonely",
        "-g",
        &user.group,
        "--config",
        &path,
    ];
    assert_success(&user.run("agents-create-lonely", &args).await);
    // delegating to a tool prints the API's 400 and fails
    let (tool, _) = generators::tool(
        &[&user.group],
        "1.0.0",
        &generators::gen_agentic_spec(),
        &user.client,
    )
    .await?;
    let tool_id = tool.id.to_string();
    let out = user
        .run(
            "agents-delegate-tool",
            &["agents", "delegate", "lonely", &tool_id],
        )
        .await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("400"));
    // a config that isn't YAML or JSON fails before any request, naming the file
    let bad = write_file("bad.yaml", "models: [unclosed\n  - {");
    let args = [
        "agents",
        "create",
        "-n",
        "broken",
        "-g",
        "no-such-group",
        "--config",
        &bad,
    ];
    let out = user.run("agents-create-bad-config", &args).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("bad.yaml"), "{stderr}");
    assert!(!stderr.contains("401"), "{stderr}");
    // the help text says AI agents
    let out = user.run("agents-help", &["agents", "--help"]).await;
    assert_success(&out);
    assert!(stdout(&out).contains("AI agents"), "{}", stdout(&out));
    Ok(())
}
