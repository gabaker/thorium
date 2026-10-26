//! Tests the thorctl agents commands against the test API

mod common;

use common::editor::{EditorScript, Step, recovery_path};
use common::{TestUser, assert_success, json, stdout};
use serde_json::Value;
use std::collections::BTreeSet;
use std::process::{Output, Stdio};
use thorium::Error;
use thorium::models::{
    AgentConfig, AssociationKind, Directionality, EntityKinds, EntityListOpts, EntityMetadata,
    ModelAuthLevel, ModelCapabilities, ModelMatcher, ModelRequirements,
};
use thorium::test_utilities::generators;
use tokio::io::AsyncWriteExt as _;
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
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        child.wait_with_output(),
    )
    .await
    .expect("thorctl timed out")
    .expect("failed to run thorctl");
    // record this run
    user.record(name, args, &output);
    output
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

/// `agents create` sends tools (latest and pinned), a sub-agent, a harness, and model auth, reads
/// its config from stdin, and refuses unknown names and versions before creating anything
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn agents_create_with_edges() -> Result<(), Error> {
    // a user named after ganymede in a group named after juno
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "agents-create-edges",
        &generators::readable_name("ganymede", &suffix),
        &generators::readable_name("juno", &suffix),
    )
    .await?;
    // a sub-agent, two tools, and a harness
    let sub_path = write_file(
        "sub.json",
        &serde_json::to_string(&generators::gen_agent_config("scout"))?,
    );
    let args = [
        "agents",
        "create",
        "-n",
        "scout",
        "-g",
        &user.group,
        "--config",
        &sub_path,
    ];
    assert_success(&user.run("agents-create-scout", &args).await);
    let scout = agent_id(&user, "scout").await?.expect("no scout");
    let spec = generators::gen_agentic_spec();
    let (latest, _) = generators::tool(&[&user.group], "1.0.0", &spec, &user.client).await?;
    let (pinned_tool, pinned) =
        generators::tool(&[&user.group], "1.0.0", &spec, &user.client).await?;
    let (harness, _) = generators::tool(
        &[&user.group],
        "1.0.0",
        &generators::gen_harness_spec(),
        &user.client,
    )
    .await?;
    // create a lead agent with all of them and its config on stdin
    let config = serde_json::to_string(&generators::gen_agent_config("captain"))?;
    let pinned_ref = format!("{}@1.0.0", pinned_tool.name);
    let args = [
        "agents",
        "create",
        "-n",
        "captain",
        "-g",
        &user.group,
        "--config",
        "-",
        "--tool",
        &latest.name,
        "--tool",
        &pinned_ref,
        "--sub-agent",
        "scout",
        "--harness",
        &harness.name,
        "--model-auth",
        "group",
        "--model-auth",
        "user",
    ];
    let out = run_with(&user, "agents-create-edges", &args, &[], Some(&config)).await;
    assert_success(&out);
    // every edge exists and the model auth levels are stored
    let captain = agent_id(&user, "captain").await?.expect("no captain");
    let mut calls = vec![latest.id, pinned.id];
    calls.sort();
    assert_eq!(
        out_edges(&user, captain, AssociationKind::Calls).await?,
        calls
    );
    assert_eq!(
        out_edges(&user, captain, AssociationKind::DelegatesTo).await?,
        vec![scout]
    );
    assert_eq!(
        out_edges(&user, captain, AssociationKind::RunsIn).await?,
        vec![harness.id]
    );
    let read = user.client.entities.get(captain).await?;
    let EntityMetadata::Agent(meta) = &read.metadata else {
        panic!("expected an agent");
    };
    let levels: BTreeSet<ModelAuthLevel> = [ModelAuthLevel::Group, ModelAuthLevel::User]
        .into_iter()
        .collect();
    assert_eq!(meta.model_auth.levels, levels);
    // a tool that doesn't exist fails before the agent is created
    let args = [
        "agents",
        "create",
        "-n",
        "ghost",
        "-g",
        &user.group,
        "--config",
        &sub_path,
        "--tool",
        "no-such-tool",
    ];
    let out = user.run("agents-create-unknown-tool", &args).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no-such-tool"), "{stderr}");
    assert!(agent_id(&user, "ghost").await?.is_none());
    // a version the tool doesn't have fails the same way
    let missing = format!("{}@9.9.9", latest.name);
    let args = [
        "agents",
        "create",
        "-n",
        "ghost",
        "-g",
        &user.group,
        "--config",
        &sub_path,
        "--tool",
        &missing,
    ];
    let out = user.run("agents-create-unknown-version", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("9.9.9"));
    assert!(agent_id(&user, "ghost").await?.is_none());
    Ok(())
}

/// The unlink commands remove one edge each, name a missing edge as an error, and
/// `clear-harness` stops the agent running in its harness
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn agents_unlink_edges() -> Result<(), Error> {
    // a user named after thebe in a group named after hera
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "agents-unlink",
        &generators::readable_name("thebe", &suffix),
        &generators::readable_name("hera", &suffix),
    )
    .await?;
    // an agent with a latest tool, a pinned tool, a sub-agent, and a harness
    let path = write_file(
        "agent.json",
        &serde_json::to_string(&generators::gen_agent_config("worker"))?,
    );
    let args = [
        "agents",
        "create",
        "-n",
        "scout",
        "-g",
        &user.group,
        "--config",
        &path,
    ];
    assert_success(&user.run("agents-create-scout", &args).await);
    let spec = generators::gen_agentic_spec();
    let (latest, _) = generators::tool(&[&user.group], "1.0.0", &spec, &user.client).await?;
    let (pinned_tool, pinned) =
        generators::tool(&[&user.group], "1.0.0", &spec, &user.client).await?;
    let (harness, _) = generators::tool(
        &[&user.group],
        "1.0.0",
        &generators::gen_harness_spec(),
        &user.client,
    )
    .await?;
    let pinned_ref = format!("{}@1.0.0", pinned_tool.name);
    let args = [
        "agents",
        "create",
        "-n",
        "boss",
        "-g",
        &user.group,
        "--config",
        &path,
        "--tool",
        &latest.name,
        "--tool",
        &pinned_ref,
        "--sub-agent",
        "scout",
        "--harness",
        &harness.name,
    ];
    assert_success(&user.run("agents-create-boss", &args).await);
    let boss = agent_id(&user, "boss").await?.expect("no boss");
    // unlinking the latest tool removes only that edge
    let args = ["agents", "unlink-tool", "boss", &latest.name];
    assert_success(&user.run("agents-unlink-tool", &args).await);
    assert_eq!(
        out_edges(&user, boss, AssociationKind::Calls).await?,
        vec![pinned.id]
    );
    // unlinking it again names the missing edge
    let out = user.run("agents-unlink-tool-again", &args).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(&latest.name), "{stderr}");
    // a pinned tool needs its pin, and the error says so
    let args = ["agents", "unlink-tool", "boss", &pinned_tool.name];
    let out = user.run("agents-unlink-tool-needs-pin", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--pin"));
    assert_eq!(
        out_edges(&user, boss, AssociationKind::Calls).await?,
        vec![pinned.id]
    );
    // the pin removes the pinned edge
    let pin = pinned.id.to_string();
    let args = [
        "agents",
        "unlink-tool",
        "boss",
        &pinned_tool.name,
        "--pin",
        &pin,
    ];
    assert_success(&user.run("agents-unlink-tool-pin", &args).await);
    assert_eq!(
        out_edges(&user, boss, AssociationKind::Calls).await?,
        Vec::<Uuid>::new()
    );
    // undelegate removes the delegation and fails when there is none
    let args = ["agents", "undelegate", "boss", "scout"];
    assert_success(&user.run("agents-undelegate", &args).await);
    assert_eq!(
        out_edges(&user, boss, AssociationKind::DelegatesTo).await?,
        Vec::<Uuid>::new()
    );
    let out = user.run("agents-undelegate-again", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("scout"));
    // an unlinked toolset that was never linked is refused by name
    let args = ["agents", "unlink-toolset", "boss", "no-such-toolset"];
    let out = user.run("agents-unlink-toolset-missing", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no-such-toolset"));
    // clear-harness removes the harness and fails when there is none
    let args = ["agents", "clear-harness", "boss"];
    assert_success(&user.run("agents-clear-harness", &args).await);
    assert_eq!(
        out_edges(&user, boss, AssociationKind::RunsIn).await?,
        Vec::<Uuid>::new()
    );
    let out = user.run("agents-clear-harness-again", &args).await;
    assert!(!out.status.success());
    // describe shows no edges any more
    let args = ["agents", "describe", "boss", "-o", "json"];
    let described = json(&user.run("agents-describe-unlinked", &args).await);
    let meta = &described["metadata"]["Agent"];
    assert_eq!(meta["latest_tools"].as_array().map(Vec::len), Some(0));
    assert_eq!(meta["pinned_tools"].as_array().map(Vec::len), Some(0));
    assert_eq!(meta["sub_agents"].as_array().map(Vec::len), Some(0));
    assert!(meta["harness"].is_null());
    Ok(())
}

/// The environment that lets thorctl open the editor script without a terminal; without a
/// terminal a failed edit is cancelled and saved, and the confirmation is accepted
const EDITOR_ENV: [(&str, &str); 1] = [("THORCTL_EDIT_ASSUME_TTY", "1")];

/// Create an agent with a generated config in the user's group
///
/// # Arguments
///
/// * `user` - The test user
/// * `name` - The agent's name, which is also its function name
async fn create_agent(user: &TestUser, name: &str) -> Result<Uuid, Error> {
    // write the config and create the agent through thorctl
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
    Ok(agent_id(user, name)
        .await?
        .expect("the agent wasn't created"))
}

/// Read an agent as the user's client sees it
///
/// # Arguments
///
/// * `user` - The test user
/// * `id` - The agent's id
async fn read_agent(
    user: &TestUser,
    id: Uuid,
) -> Result<(thorium::models::Entity, thorium::models::AgentEntity), Error> {
    // read the entity and take its agent metadata
    let entity = user.client.entities.get(id).await?;
    let EntityMetadata::Agent(meta) = entity.metadata.clone() else {
        panic!("expected an agent");
    };
    Ok((entity, meta))
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

/// A shell command that runs thorctl as the user, for the hook that changes an agent between
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

/// `agents update` changes fields from typed flags, `--set`, `--unset`, `--patch`, `--file` and
/// stdin, prints the result as JSON, and a spec round trips without changes
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn agents_update_forms() -> Result<(), Error> {
    // a user named after rhea in a group named after ceres, and a second group
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "agents-update-forms",
        &generators::readable_name("rhea", &suffix),
        &generators::readable_name("ceres", &suffix),
    )
    .await?;
    let other = extra_group(&user, &generators::readable_name("vesta", &suffix)).await?;
    let id = create_agent(&user, "worker").await?;
    // a rename and a description
    let args = [
        "agents",
        "update",
        "worker",
        "--rename",
        "worker2",
        "-d",
        "checks files",
    ];
    let out = user.run("agents-update-rename", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("updated"), "{}", stdout(&out));
    let (entity, _) = read_agent(&user, id).await?;
    assert_eq!(entity.name, "worker2");
    assert_eq!(entity.description.as_deref(), Some("checks files"));
    // a group is added and removed
    let args = ["agents", "update", "worker2", "--add-group", &other];
    assert_success(&user.run("agents-update-add-group", &args).await);
    let (entity, _) = read_agent(&user, id).await?;
    assert!(entity.groups.contains(&other) && entity.groups.contains(&user.group));
    let args = ["agents", "update", "worker2", "--remove-group", &user.group];
    assert_success(&user.run("agents-update-remove-group", &args).await);
    let (entity, _) = read_agent(&user, id).await?;
    assert_eq!(entity.groups, vec![other.clone()]);
    // a config value from --set, with the JSON result pinned
    let args = [
        "agents",
        "update",
        "worker2",
        "--set",
        "config.max_turns=5",
        "-o",
        "json",
    ];
    let result = json(&user.run("agents-update-set-json", &args).await);
    assert_eq!(result["kind"], "agent");
    assert_eq!(result["name"], "worker2");
    assert_eq!(result["status"], "updated");
    assert_eq!(result["changed"], true);
    assert_eq!(result["diff"][0]["path"], "config.max_turns");
    assert_eq!(result["diff"][0]["old"], 10);
    assert_eq!(result["diff"][0]["new"], 5);
    assert_eq!(result["resource"]["config"]["max_turns"], 5);
    let (_, meta) = read_agent(&user, id).await?;
    assert_eq!(meta.config.max_turns, Some(5));
    // model auth is set and cleared
    let args = [
        "agents",
        "update",
        "worker2",
        "--model-auth",
        "group",
        "--model-auth",
        "user",
    ];
    assert_success(&user.run("agents-update-model-auth", &args).await);
    let (_, meta) = read_agent(&user, id).await?;
    assert_eq!(meta.model_auth.levels.len(), 2);
    let args = ["agents", "update", "worker2", "--clear-model-auth"];
    assert_success(&user.run("agents-update-clear-model-auth", &args).await);
    let (_, meta) = read_agent(&user, id).await?;
    assert!(meta.model_auth.levels.is_empty());
    // a merge patch and an unset
    let args = [
        "agents",
        "update",
        "worker2",
        "--patch",
        r#"{"description": null}"#,
    ];
    assert_success(&user.run("agents-update-patch", &args).await);
    let (entity, _) = read_agent(&user, id).await?;
    assert_eq!(entity.description, None);
    let args = ["agents", "update", "worker2", "--set", "description=again"];
    assert_success(&user.run("agents-update-describe", &args).await);
    let args = ["agents", "update", "worker2", "--unset", "description"];
    assert_success(&user.run("agents-update-unset", &args).await);
    let (entity, _) = read_agent(&user, id).await?;
    assert_eq!(entity.description, None);
    // a config file replaces the config
    let config = write_file("config.yaml", "max_turns: 3\nfunction_name: worker_fn\n");
    let args = ["agents", "update", "worker2", "--config", &config];
    assert_success(&user.run("agents-update-config-file", &args).await);
    let (_, meta) = read_agent(&user, id).await?;
    assert_eq!(meta.config.max_turns, Some(3));
    assert_eq!(meta.config.function_name.as_deref(), Some("worker_fn"));
    assert_eq!(meta.config.system_prompt, None);
    // the spec round trips: unchanged, then changed through stdin and through a file
    let spec = stdout(
        &user
            .run("agents-spec", &["agents", "spec", "worker2"])
            .await,
    );
    assert!(spec.contains("worker2") && spec.contains("*id*"), "{spec}");
    let args = ["agents", "update", "worker2", "--file", "-"];
    let out = run_with(&user, "agents-update-spec-same", &args, &[], Some(&spec)).await;
    assert_success(&out);
    assert!(stdout(&out).contains("No changes"), "{}", stdout(&out));
    let changed = spec.replace("max_turns: 3", "max_turns: 4");
    let out = run_with(
        &user,
        "agents-update-spec-stdin",
        &args,
        &[],
        Some(&changed),
    )
    .await;
    assert_success(&out);
    let (_, meta) = read_agent(&user, id).await?;
    assert_eq!(meta.config.max_turns, Some(4));
    let path = write_file("spec.yaml", &spec.replace("max_turns: 3", "max_turns: 6"));
    let args = ["agents", "update", "worker2", "--file", &path];
    assert_success(&user.run("agents-update-spec-file", &args).await);
    let (_, meta) = read_agent(&user, id).await?;
    assert_eq!(meta.config.max_turns, Some(6));
    // spec as JSON is a document, and -o writes a file
    let args = ["agents", "spec", "worker2", "--format", "json"];
    let doc = json(&user.run("agents-spec-json", &args).await);
    assert_eq!(doc["name"], "worker2");
    assert_eq!(doc["config"]["max_turns"], 6);
    let out_path = std::env::temp_dir().join(format!("{}-spec.yaml", Uuid::new_v4().simple()));
    let out_str = out_path.to_string_lossy().into_owned();
    assert_success(
        &user
            .run(
                "agents-spec-file",
                &["agents", "spec", "worker2", "-o", &out_str],
            )
            .await,
    );
    assert!(std::fs::read_to_string(&out_path)?.contains("worker2"));
    Ok(())
}

/// `agents update` refuses a bad change before sending anything, reports the API's refusal,
/// does nothing for no change or a dry run, and stops when the agent changed meanwhile
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn agents_update_refusals() -> Result<(), Error> {
    // a user named after dione in a group named after pallas
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "agents-update-refusals",
        &generators::readable_name("dione", &suffix),
        &generators::readable_name("pallas", &suffix),
    )
    .await?;
    let id = create_agent(&user, "guard").await?;
    // nothing to update without a change flag
    let out = user
        .run("agents-update-nothing", &["agents", "update", "guard"])
        .await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("Nothing to update"));
    // setting the current value changes nothing
    let args = ["agents", "update", "guard", "--set", "config.max_turns=10"];
    let out = user.run("agents-update-no-change", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("No changes"), "{}", stdout(&out));
    // a dry run shows the change and writes nothing
    let args = [
        "agents",
        "update",
        "guard",
        "--set",
        "config.max_turns=11",
        "--dry-run",
    ];
    let out = user.run("agents-update-dry-run", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("max_turns"), "{}", stdout(&out));
    assert_eq!(read_agent(&user, id).await?.1.config.max_turns, Some(10));
    // a read-only field cannot be set
    let args = ["agents", "update", "guard", "--set", "id=x"];
    let out = user.run("agents-update-read-only", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("read-only"));
    // a document with an unknown key is refused and nothing changes
    let path = write_file("bad.yaml", "colour: red\n");
    let args = ["agents", "update", "guard", "--file", &path];
    let out = user.run("agents-update-unknown-key", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("colour"));
    // a config setting that doesn't exist is refused and nothing changes
    let args = ["agents", "update", "guard", "--set", "config.max_turn=3"];
    let out = user.run("agents-update-unknown-setting", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("max_turn"));
    // a tool that doesn't exist is refused before the update is sent
    let args = [
        "agents",
        "update",
        "guard",
        "--set",
        "tools=[{tool: no-such-tool}]",
    ];
    let out = user.run("agents-update-unknown-tool", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no-such-tool"));
    // the API's refusal is printed: a group that doesn't exist
    let args = [
        "agents",
        "update",
        "guard",
        "--add-group",
        "no-such-group-anywhere",
    ];
    let out = user.run("agents-update-api-refusal", &args).await;
    assert!(!out.status.success());
    assert!(!String::from_utf8_lossy(&out.stderr).is_empty());
    let (entity, meta) = read_agent(&user, id).await?;
    assert_eq!(entity.groups, vec![user.group.clone()]);
    assert_eq!(meta.config.max_turns, Some(10));
    // an agent changed between the read and the apply is refused, and --force applies anyway
    let hook_env = hook(&user, "agents update guard -d changed-remotely");
    let envs = [("THORCTL_EDIT_BEFORE_APPLY", hook_env.as_str())];
    let args = ["agents", "update", "guard", "--set", "config.max_turns=7"];
    let out = run_with(&user, "agents-update-remote-change", &args, &envs, None).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("changed remotely"));
    assert_eq!(read_agent(&user, id).await?.1.config.max_turns, Some(10));
    let args = [
        "agents",
        "update",
        "guard",
        "--set",
        "config.max_turns=7",
        "--force",
    ];
    let out = run_with(&user, "agents-update-remote-force", &args, &envs, None).await;
    assert_success(&out);
    assert_eq!(read_agent(&user, id).await?.1.config.max_turns, Some(7));
    Ok(())
}

/// `agents update` and `edit` change tools, toolsets, sub-agents and the harness by name
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn agents_update_edges() -> Result<(), Error> {
    // a user named after metis in a group named after nyx
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "agents-update-edges",
        &generators::readable_name("metis", &suffix),
        &generators::readable_name("nyx", &suffix),
    )
    .await?;
    let id = create_agent(&user, "chief").await?;
    let scout = create_agent(&user, "scout").await?;
    let spec = generators::gen_agentic_spec();
    let (first, _) = generators::tool(&[&user.group], "1.0.0", &spec, &user.client).await?;
    let (second, second_r1) =
        generators::tool(&[&user.group], "1.0.0", &spec, &user.client).await?;
    let (harness, _) = generators::tool(
        &[&user.group],
        "1.0.0",
        &generators::gen_harness_spec(),
        &user.client,
    )
    .await?;
    // set tools (one followed, one pinned), a sub-agent and a harness in one update
    let tools = format!(
        "tools=[{{tool: {}}}, {{tool: {}, version: \"1.0.0\"}}]",
        first.name, second.name
    );
    let harness_set = format!("harness={{tool: {}}}", harness.name);
    let args = [
        "agents",
        "update",
        "chief",
        "--set",
        &tools,
        "--set",
        "sub_agents=[scout]",
        "--set",
        &harness_set,
    ];
    let out = user.run("agents-update-edges", &args).await;
    assert_success(&out);
    let mut calls = vec![first.id, second_r1.id];
    calls.sort();
    assert_eq!(out_edges(&user, id, AssociationKind::Calls).await?, calls);
    assert_eq!(
        out_edges(&user, id, AssociationKind::DelegatesTo).await?,
        vec![scout]
    );
    assert_eq!(
        out_edges(&user, id, AssociationKind::RunsIn).await?,
        vec![harness.id]
    );
    // the spec lists them by name and updating with it changes nothing
    let spec_text = stdout(
        &user
            .run("agents-spec-edges", &["agents", "spec", "chief"])
            .await,
    );
    for expected in [
        first.name.as_str(),
        second.name.as_str(),
        "scout",
        harness.name.as_str(),
        "1.0.0",
    ] {
        assert!(
            spec_text.contains(expected),
            "spec lacks {expected}:\n{spec_text}"
        );
    }
    let args = ["agents", "update", "chief", "--file", "-"];
    let out = run_with(
        &user,
        "agents-update-edges-same",
        &args,
        &[],
        Some(&spec_text),
    )
    .await;
    assert_success(&out);
    assert!(stdout(&out).contains("No changes"), "{}", stdout(&out));
    // dropping the pinned tool, the sub-agent and the harness removes those edges only
    let args = [
        "agents",
        "update",
        "chief",
        "--set",
        &format!("tools=[{{tool: {}}}]", first.name),
        "--set",
        "sub_agents=[]",
        "--unset",
        "harness",
    ];
    assert_success(&user.run("agents-update-edges-drop", &args).await);
    assert_eq!(
        out_edges(&user, id, AssociationKind::Calls).await?,
        vec![first.id]
    );
    assert_eq!(
        out_edges(&user, id, AssociationKind::DelegatesTo).await?,
        Vec::<Uuid>::new()
    );
    assert_eq!(
        out_edges(&user, id, AssociationKind::RunsIn).await?,
        Vec::<Uuid>::new()
    );
    // edit with a flag source never opens an editor
    let args = [
        "agents",
        "edit",
        "chief",
        "--set",
        "description=edited by flag",
    ];
    assert_success(&user.run("agents-edit-flags", &args).await);
    let (entity, _) = read_agent(&user, id).await?;
    assert_eq!(entity.description.as_deref(), Some("edited by flag"));
    Ok(())
}

/// `agents edit` in an editor applies a change, shows the header and read-only fields, leaves an
/// unchanged buffer alone, reopens after a bad edit, saves a cancelled bad edit to a recovery
/// file, and reports the API's refusal with the recovery file
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn agents_edit_interactive() -> Result<(), Error> {
    // a user named after eris in a group named after sylvia
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "agents-edit-interactive",
        &generators::readable_name("eris", &suffix),
        &generators::readable_name("sylvia", &suffix),
    )
    .await?;
    let id = create_agent(&user, "editable").await?;
    // a change made in the editor is applied, and the buffer had a header and the read-only id
    let script = EditorScript::replace("max_turns: 10", "max_turns: 9");
    let editor = script.editor();
    let args = ["agents", "edit", "editable", "--editor", &editor];
    let out = run_with(&user, "agents-edit-change", &args, &EDITOR_ENV, None).await;
    assert_success(&out);
    assert!(stdout(&out).contains("updated"), "{}", stdout(&out));
    assert_eq!(read_agent(&user, id).await?.1.config.max_turns, Some(9));
    let seen = script.seen(0).expect("the editor saw no buffer");
    assert!(seen.contains("# Editing agent 'editable'"), "{seen}");
    assert!(
        seen.contains("*id*") && seen.contains("read-only"),
        "{seen}"
    );
    // an unchanged buffer changes nothing
    let script = EditorScript::noop();
    let editor = script.editor();
    let args = ["agents", "edit", "editable", "--editor", &editor];
    let out = run_with(&user, "agents-edit-noop", &args, &EDITOR_ENV, None).await;
    assert_success(&out);
    assert!(stdout(&out).contains("No changes"), "{}", stdout(&out));
    // an empty buffer cancels
    let script = EditorScript::write("");
    let editor = script.editor();
    let args = ["agents", "edit", "editable", "--editor", &editor];
    let out = run_with(&user, "agents-edit-cancel", &args, &EDITOR_ENV, None).await;
    assert_success(&out);
    assert!(stdout(&out).contains("Cancelled"), "{}", stdout(&out));
    // editing the read-only id is reported and ignored
    let script = EditorScript::replace(&id.to_string(), "00000000-0000-0000-0000-000000000000");
    let editor = script.editor();
    let args = ["agents", "edit", "editable", "--editor", &editor];
    let out = run_with(&user, "agents-edit-read-only", &args, &EDITOR_ENV, None).await;
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
    let args = ["agents", "edit", "editable", "--editor", &editor];
    let out = run_with(&user, "agents-edit-reedit", &args, &envs, None).await;
    assert_success(&out);
    assert_eq!(script.invocations(), 2);
    assert!(String::from_utf8_lossy(&out.stderr).contains("colour"));
    // cancelling after a bad edit fails and saves the text to a recovery file
    let script = EditorScript::append("colour: red");
    let editor = script.editor();
    let args = ["agents", "edit", "editable", "--editor", &editor];
    let out = run_with(&user, "agents-edit-bad-cancel", &args, &EDITOR_ENV, None).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let path = recovery_path(&stderr).expect("no recovery file named");
    assert!(std::fs::read_to_string(&path)?.contains("colour: red"));
    std::fs::remove_file(&path)?;
    // a name the API refuses is reported with its reason and the recovery file
    let script = EditorScript::replace(&user.group, "no-such-group-anywhere");
    let editor = script.editor();
    let args = ["agents", "edit", "editable", "--editor", &editor];
    let out = run_with(&user, "agents-edit-api-refusal", &args, &EDITOR_ENV, None).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(stderr.contains("Code: 401"), "{stderr}");
    let path = recovery_path(&stderr).expect("no recovery file named");
    std::fs::remove_file(&path)?;
    assert_eq!(
        read_agent(&user, id).await?.0.groups,
        vec![user.group.clone()]
    );
    // a dry run shows the changes and writes nothing
    let script = EditorScript::replace("max_turns: 9", "max_turns: 8");
    let editor = script.editor();
    let args = [
        "agents",
        "edit",
        "editable",
        "--editor",
        &editor,
        "--dry-run",
    ];
    let out = run_with(&user, "agents-edit-dry-run", &args, &EDITOR_ENV, None).await;
    assert_success(&out);
    assert!(stdout(&out).contains("max_turns"), "{}", stdout(&out));
    assert_eq!(read_agent(&user, id).await?.1.config.max_turns, Some(9));
    // declining the confirmation applies nothing and saves the edit
    let declined = [
        ("THORCTL_EDIT_ASSUME_TTY", "1"),
        ("THORCTL_EDIT_PROMPT", "cancel"),
    ];
    let script = EditorScript::replace("max_turns: 9", "max_turns: 8");
    let editor = script.editor();
    let args = ["agents", "edit", "editable", "--editor", &editor];
    let out = run_with(&user, "agents-edit-declined", &args, &declined, None).await;
    assert_success(&out);
    assert!(stdout(&out).contains("Cancelled"), "{}", stdout(&out));
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let path = recovery_path(&stderr.replace("Your edits were saved to", "saved to"))
        .expect("no recovery file named");
    assert!(std::fs::read_to_string(&path)?.contains("max_turns: 8"));
    std::fs::remove_file(&path)?;
    assert_eq!(read_agent(&user, id).await?.1.config.max_turns, Some(9));
    // an agent that changed during the edit is refused unless forced
    let hook_env = hook(&user, "agents update editable -d changed-remotely");
    let envs = [
        ("THORCTL_EDIT_ASSUME_TTY", "1"),
        ("THORCTL_EDIT_BEFORE_APPLY", hook_env.as_str()),
    ];
    let script = EditorScript::replace("max_turns: 9", "max_turns: 8");
    let editor = script.editor();
    let args = ["agents", "edit", "editable", "--editor", &editor];
    let out = run_with(&user, "agents-edit-remote-change", &args, &envs, None).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(stderr.contains("changed remotely"), "{stderr}");
    if let Some(path) = recovery_path(&stderr) {
        std::fs::remove_file(path)?;
    }
    let args = ["agents", "edit", "editable", "--editor", &editor, "--force"];
    let out = run_with(&user, "agents-edit-remote-force", &args, &envs, None).await;
    assert_success(&out);
    assert_eq!(read_agent(&user, id).await?.1.config.max_turns, Some(8));
    Ok(())
}

/// `agents delete` does nothing for a dry run, needs `--skip-confirm` without a terminal, and
/// deletes the agent and its edges otherwise
#[tokio::test]
async fn agents_delete() -> Result<(), Error> {
    // a user named after hygiea in a group named after flora
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "agents-delete",
        &generators::readable_name("hygiea", &suffix),
        &generators::readable_name("flora", &suffix),
    )
    .await?;
    let id = create_agent(&user, "doomed").await?;
    // a dry run names what would be deleted and keeps the agent
    let args = ["agents", "delete", "doomed", "--dry-run"];
    let out = user.run("agents-delete-dry-run", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("doomed"), "{}", stdout(&out));
    assert!(agent_id(&user, "doomed").await?.is_some());
    // without a terminal the command asks for the flag instead of prompting
    let out = user
        .run("agents-delete-no-tty", &["agents", "delete", "doomed"])
        .await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--skip-confirm"));
    assert!(agent_id(&user, "doomed").await?.is_some());
    // with the flag the agent is deleted
    let out = user
        .run("agents-delete", &["agents", "delete", "doomed", "--yes"])
        .await;
    assert_success(&out);
    assert!(stdout(&out).contains("Deleted"), "{}", stdout(&out));
    assert!(agent_id(&user, "doomed").await?.is_none());
    assert!(user.client.entities.get(id).await.is_err());
    // deleting it again says there is no such agent
    let out = user
        .run(
            "agents-delete-again",
            &["agents", "delete", "doomed", "--yes"],
        )
        .await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("doomed"));
    Ok(())
}

/// An edge to a tool that misses one of the agent's groups is refused, and
/// `--extend-target-groups` adds the group to the tool instead
#[tokio::test]
async fn agents_update_extends_target_groups() -> Result<(), Error> {
    // a user named after selene in a group named after luna, and a second group
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "agents-update-extend",
        &generators::readable_name("selene", &suffix),
        &generators::readable_name("luna", &suffix),
    )
    .await?;
    let other = extra_group(&user, &generators::readable_name("terra", &suffix)).await?;
    // an agent in both groups and a tool in only one of them
    let path = write_file(
        "wide.json",
        &serde_json::to_string(&generators::gen_agent_config("wide"))?,
    );
    let args = [
        "agents",
        "create",
        "-n",
        "wide",
        "-g",
        &user.group,
        "-g",
        &other,
        "--config",
        &path,
    ];
    assert_success(&user.run("agents-create-wide", &args).await);
    let id = agent_id(&user, "wide").await?.expect("no wide agent");
    let (tool, _) = generators::tool(
        &[&user.group],
        "1.0.0",
        &generators::gen_agentic_spec(),
        &user.client,
    )
    .await?;
    // the edge is refused with the API's reason
    let tools = format!("tools=[{{tool: {}}}]", tool.name);
    let args = ["agents", "update", "wide", "--set", &tools];
    let out = user.run("agents-update-missing-group", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("isn't in groups"));
    assert_eq!(
        out_edges(&user, id, AssociationKind::Calls).await?,
        Vec::<Uuid>::new()
    );
    // the flag widens the tool's groups and adds the edge
    let args = [
        "agents",
        "update",
        "wide",
        "--set",
        &tools,
        "--extend-target-groups",
    ];
    assert_success(&user.run("agents-update-extend", &args).await);
    assert_eq!(
        out_edges(&user, id, AssociationKind::Calls).await?,
        vec![tool.id]
    );
    assert!(
        user.client
            .entities
            .get(tool.id)
            .await?
            .groups
            .contains(&other)
    );
    Ok(())
}
