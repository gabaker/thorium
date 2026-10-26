//! Tests `thorctl toolbox export`, `diff`, and `remove` against the test API

mod common;

use common::{TestUser, assert_success, json, stdout, unique};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Output;
use thorium::Error;
use thorium::models::{
    AgentEntityRequest, AgenticTool, AgenticToolBacking, ContainerToolBacking, CredentialKind,
    Entity, EntityKinds, GroupRequest, GroupUsersRequest, NetworkPolicyRequest, PipelineRequest,
    ToolSetEntityRequest, ToolSpec, WorkflowCatalogEntityRequest,
};
use thorium::test_utilities::generators;
use uuid::Uuid;

/// The tools, toolset, agent, workflow, and pipeline a test builds in its group
struct World {
    /// The tool every other tool depends on, which has two versions
    base: Entity,
    /// `base`'s first version
    base_v1: Entity,
    /// The tool that depends on `base`
    dependent: Entity,
    /// The agentic tool that pins `base`'s first version
    caller: Entity,
    /// The name of the pipeline that follows `base` and pins its first version
    pipeline: String,
    /// The toolset that includes `caller`
    toolset: Entity,
    /// The agent that uses `caller` and the toolset
    agent: Entity,
    /// The workflow that runs the agent
    workflow: Entity,
}

/// Build a spec of a different image than another, so it publishes as a new version
///
/// # Arguments
///
/// * `image` - The container image
fn image_spec(image: &str) -> ToolSpec {
    // start from a valid generated spec
    let mut spec = serde_json::to_value(generators::gen_image_spec()).unwrap();
    spec["Image"]["environment"]["K8s"]["image"] = json!(image);
    serde_json::from_value(spec).unwrap()
}

/// Publish a new version of a tool
///
/// # Arguments
///
/// * `user` - The user to publish as
/// * `tool` - The tool's id
/// * `label` - The new version's label
/// * `spec` - The new version's spec
async fn publish(user: &TestUser, tool: Uuid, label: &str, spec: &ToolSpec) -> Result<(), Error> {
    // publish the spec through a tool update
    user.client
        .entities
        .update(tool, generators::gen_tool_publish(label, spec))
        .await?;
    Ok(())
}

/// Build the tools, toolset, agent, workflow, and pipeline of a test
///
/// # Arguments
///
/// * `user` - The user to build everything as
async fn world(user: &TestUser) -> Result<World, Error> {
    let group = user.group.as_str();
    // `base` has two versions, and `dependent` reads its results
    let (base, base_v1) = generators::tool(
        &[group],
        "1.0.0",
        &image_spec("registry.local/base:1"),
        &user.client,
    )
    .await?;
    publish(user, base.id, "2.0.0", &image_spec("registry.local/base:2")).await?;
    let mut dependent_spec = serde_json::to_value(image_spec("registry.local/dep:1")).unwrap();
    dependent_spec["Image"]["data"]["dependencies"]["results"]["tools"] = json!([base.id]);
    let (dependent, _) = generators::tool(
        &[group],
        "1.0.0",
        &serde_json::from_value(dependent_spec).unwrap(),
        &user.client,
    )
    .await?;
    // `caller` is an agentic tool whose container pins `base`'s first version
    let caller_spec = ToolSpec::Agentic(AgenticTool {
        function_name: format!("call_{}", unique("")[..8].to_owned()),
        model_description: "Calls the base tool".to_owned(),
        input_schema: [
            ("type".to_owned(), json!("object")),
            ("properties".to_owned(), json!({})),
        ]
        .into_iter()
        .collect(),
        backing: AgenticToolBacking::Container(ContainerToolBacking {
            tool_instance: base_v1.id,
            ..ContainerToolBacking::default()
        }),
        timeout: Some(60),
        ..AgenticTool::default()
    });
    let (caller, _) = generators::tool(&[group], "1.0.0", &caller_spec, &user.client).await?;
    // a pipeline that follows `base`'s latest for one stage and pins its first version for another
    let pipeline = unique("pipe")[..20].to_owned();
    let request = PipelineRequest::new(group, &pipeline, json!([["follow"], ["pin"]]))
        .stage_tool("follow", base.id)
        .stage_tool("pin", base_v1.id);
    user.client.pipelines.create(&request).await?;
    // a toolset over `caller`, an agent using both, and a workflow running the agent
    let toolset_req = generators::gen_toolset(
        &unique("kit")[..16],
        &[group],
        ToolSetEntityRequest::default().tool(caller.id),
    );
    let toolset = user.client.entities.create(toolset_req).await?;
    let agent_req = generators::gen_agent(
        &unique("bot")[..16],
        &[group],
        AgentEntityRequest::new(generators::gen_agent_config("bot"))
            .tool(caller.id)
            .tool_set(toolset.id),
    );
    let agent = user.client.entities.create(agent_req).await?;
    let workflow_req = generators::gen_workflow(
        &unique("flow")[..16],
        &[group],
        WorkflowCatalogEntityRequest::default()
            .agent(agent.id)
            .entry_agent(agent.id),
    );
    let workflow = user.client.entities.create(workflow_req).await?;
    Ok(World {
        base,
        base_v1,
        dependent,
        caller,
        pipeline,
        toolset: user.client.entities.get(toolset.id).await?,
        agent: user.client.entities.get(agent.id).await?,
        workflow: user.client.entities.get(workflow.id).await?,
    })
}

/// Make an empty directory to export into
fn export_dir() -> PathBuf {
    // a unique directory in the temp dir
    std::env::temp_dir().join(unique("thorctl-export-"))
}

/// Export a group's tools, pipelines, toolsets, agents, and workflows
///
/// # Arguments
///
/// * `user` - The user to export as
/// * `name` - A label for the run, which names what it checks
/// * `dir` - The directory to export into
/// * `extra` - More flags
async fn export(user: &TestUser, name: &str, dir: &Path, extra: &[&str]) -> Output {
    // export the whole group with every kind
    let dir = dir.to_string_lossy().into_owned();
    let mut args = vec![
        "toolbox",
        "export",
        "-g",
        &user.group,
        "--all-kinds",
        "-o",
        &dir,
    ];
    args.extend_from_slice(extra);
    user.run(name, &args).await
}

/// Read an exported toolbox's `toolbox.json`
///
/// # Arguments
///
/// * `dir` - The exported toolbox
fn read_toolbox(dir: &Path) -> Value {
    // parse the assembled manifest
    let text = std::fs::read_to_string(dir.join("toolbox.json")).expect("toolbox.json");
    serde_json::from_str(&text).expect("toolbox.json is JSON")
}

/// Get the spec of a version of an exported tool
///
/// # Arguments
///
/// * `toolbox` - The exported manifest
/// * `tool` - The tool's name
/// * `label` - The version's label
fn spec_of<'a>(toolbox: &'a Value, tool: &str, label: &str) -> &'a Value {
    // the spec is inline in the manifest
    &toolbox["tools"][tool]["versions"][label]["spec"]
}

/// Export a group, then validate and diff what was written
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn toolbox_export_validates_and_matches_thorium() -> Result<(), Error> {
    // a group with tools that refer to each other, and the items built on them
    let user = TestUser::new("toolbox-export").await?;
    let world = Box::pin(world(&user)).await?;
    let dir = export_dir();
    let output = export(&user, "toolbox-export-all", &dir, &["--format", "json"]).await;
    assert_success(&output);
    // the report counts what was exported and found nothing wrong with it
    let report = json(&output);
    assert_eq!(report["counts"]["tools"], 3, "{report}");
    assert_eq!(report["counts"]["tool_versions"], 4);
    assert_eq!(report["counts"]["toolsets"], 1);
    assert_eq!(report["counts"]["agents"], 1);
    assert_eq!(report["counts"]["workflows"], 1);
    assert_eq!(report["counts"]["pipelines"], 1);
    assert_eq!(report["validation_errors"], 0);
    // only v2 sections are written, and nothing names an id
    let toolbox = read_toolbox(&dir);
    assert_eq!(toolbox["format_version"], 2);
    assert!(toolbox.get("images").is_none());
    let text = toolbox.to_string();
    for id in [
        world.base.id,
        world.base_v1.id,
        world.dependent.id,
        world.caller.id,
    ] {
        assert!(!text.contains(&id.to_string()), "{id} is in the export");
    }
    // dependencies and pins are written as references
    assert_eq!(
        spec_of(&toolbox, &world.dependent.name, "1.0.0")["Image"]["data"]["dependencies"]["results"]
            ["tools"],
        json!([format!("ref:tool/{}", world.base.name)])
    );
    assert_eq!(
        spec_of(&toolbox, &world.caller.name, "1.0.0")["Agentic"]["backing"]["Container"]["tool_instance"],
        json!(format!("ref:tool/{}@1.0.0", world.base.name))
    );
    // the pipeline follows one tool and pins one version
    let stages = &toolbox["pipelines"][&world.pipeline]["versions"]["latest"]["stage_tools"];
    assert_eq!(stages["follow"], json!({"tool": world.base.name}));
    assert_eq!(
        stages["pin"],
        json!({"tool": world.base.name, "version": "1.0.0"})
    );
    // the directory layout holds a manifest and a JSON spec per version
    let version_dir = dir.join(format!("tools/{}/{}/2.0.0", user.group, world.base.name));
    assert!(version_dir.join("manifest.toml").is_file());
    assert!(
        version_dir
            .join(format!("{}.json", world.base.name))
            .is_file()
    );
    // the toolbox validates, as a file and as a directory
    let file = dir.join("toolbox.json").to_string_lossy().into_owned();
    assert_success(
        &user
            .run("toolbox-export-validate", &["toolbox", "validate", &file])
            .await,
    );
    let directory = dir.to_string_lossy().into_owned();
    assert_success(
        &user
            .run(
                "toolbox-export-validate-dir",
                &["toolbox", "validate", &directory],
            )
            .await,
    );
    // an export of the same group is what an import of it would leave, so a diff finds nothing
    let args = ["toolbox", "diff", &file, "--exit-code", "--format", "json"];
    let output = user.run("toolbox-export-diff", &args).await;
    assert_success(&output);
    let diff = json(&output);
    assert_eq!(diff["changed"], false, "{diff}");
    assert_eq!(diff["versions"]["unchanged"], 4);
    assert_eq!(diff["versions"]["label_conflict"], 0);
    // hashes the client computed equal the ones the API gave the instances
    assert_eq!(diff["summary"]["unchanged"], 7, "{diff}");
    Ok(())
}

/// Exporting a group takes each network policy its tools name from the group itself, even when
/// another group holds a distinct policy of the same name, which a lookup by name alone refuses
#[tokio::test]
async fn toolbox_export_reads_network_policies_by_group() -> Result<(), Error> {
    // a tool that names a policy, in a group that shares the policy's name with another group
    let user = TestUser::new("toolbox-export-policies").await?;
    let other = unique("policyctl");
    let owners = GroupUsersRequest::default()
        .direct("thorium")
        .direct(&user.username);
    user.admin
        .groups
        .create(&GroupRequest::new(&other).owners(owners))
        .await?;
    let policy = unique("policy");
    user.admin
        .network_policies
        .create(NetworkPolicyRequest::new(&policy, [user.group.as_str()]))
        .await?;
    let mut forced = NetworkPolicyRequest::new(&policy, [other.as_str()]);
    forced.forced_policy = true;
    user.admin.network_policies.create(forced).await?;
    let mut spec = serde_json::to_value(image_spec("registry.local/p:1")).unwrap();
    spec["Image"]["environment"]["K8s"]["network_policies"] = json!([policy.clone()]);
    generators::tool(
        &[user.group.as_str()],
        "1.0.0",
        &serde_json::from_value(spec).unwrap(),
        &user.client,
    )
    .await?;
    // the export finds the group's policy, not the other group's, and warns of nothing unread
    let dir = export_dir();
    let output = export(
        &user,
        "toolbox-export-policies",
        &dir,
        &["--format", "json"],
    )
    .await;
    assert_success(&output);
    let report = json(&output);
    assert!(
        report["warnings"]
            .as_array()
            .is_none_or(|notes| notes.iter().all(|note| !note
                .as_str()
                .unwrap_or_default()
                .contains("could not be read"))),
        "{report}"
    );
    let toolbox = read_toolbox(&dir);
    let policies = toolbox["network_policies"].as_array().expect("policies");
    assert_eq!(policies.len(), 1, "{toolbox}");
    assert_eq!(policies[0]["name"], json!(policy));
    assert_eq!(policies[0]["forced_policy"], json!(false));
    Ok(())
}

/// Model endpoints, profiles, and credentials and their secrets never appear in an export
#[tokio::test]
async fn toolbox_export_never_reads_model_configuration() -> Result<(), Error> {
    // a group with a tool and a model credential holding a secret
    let user = TestUser::new("toolbox-export-secrets").await?;
    let (tool, _) = generators::tool(
        &[&user.group],
        "1.0.0",
        &generators::gen_image_spec(),
        &user.client,
    )
    .await?;
    let secret = unique("s3cr3t");
    let credential = generators::model_credential(
        &[&user.group],
        CredentialKind::ApiKey,
        Some(&secret),
        &user.client,
    )
    .await?;
    let dir = export_dir();
    let output = export(&user, "toolbox-export-secrets-run", &dir, &[]).await;
    assert_success(&output);
    // nothing the export printed or wrote names the credential or holds its secret
    let mut everything = stdout(&output) + &String::from_utf8_lossy(&output.stderr);
    for entry in walk(&dir) {
        everything += &std::fs::read_to_string(entry).unwrap_or_default();
    }
    assert!(!everything.contains(&secret));
    assert!(!everything.contains(&credential.name));
    assert!(!everything.contains("ModelCredential"));
    assert!(everything.contains(&tool.name));
    // the manifest holds only the sections of the kinds a toolbox carries
    let toolbox = read_toolbox(&dir);
    for key in toolbox.as_object().unwrap().keys() {
        assert!(
            [
                "format_version",
                "name",
                "groups",
                "tools",
                "network_policies"
            ]
            .contains(&key.as_str()),
            "unexpected section {key}"
        );
    }
    Ok(())
}

/// List every file under a directory
///
/// # Arguments
///
/// * `dir` - The directory to walk
fn walk(dir: &Path) -> Vec<PathBuf> {
    // collect files recursively
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).expect("a directory").flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else {
            files.push(path);
        }
    }
    files
}

/// Deprecated versions are skipped unless asked for, unless something pins them
#[tokio::test]
async fn toolbox_export_filters_versions() -> Result<(), Error> {
    // `caller` pins `base`'s first version, which is then deprecated along with nothing else
    let user = TestUser::new("toolbox-export-versions").await?;
    let world = Box::pin(world(&user)).await?;
    user.client
        .entities
        .update(world.base_v1.id, generators::gen_tool_deprecate(true))
        .await?;
    let versions = |toolbox: &Value| -> Vec<String> {
        toolbox["tools"][&world.base.name]["versions"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    };
    // the pinned deprecated version is exported so the reference resolves
    let dir = export_dir();
    assert_success(&export(&user, "toolbox-export-deprecated", &dir, &[]).await);
    assert_eq!(versions(&read_toolbox(&dir)), ["1.0.0", "2.0.0"]);
    // with nothing pinning it, deprecated versions are left out
    let only = export_dir();
    let output = user
        .run(
            "toolbox-export-deprecated-skipped",
            &[
                "toolbox",
                "export",
                "-t",
                &format!("{}/{}", user.group, world.base.name),
                "-o",
                &only.to_string_lossy(),
            ],
        )
        .await;
    assert_success(&output);
    assert_eq!(versions(&read_toolbox(&only)), ["2.0.0"]);
    // --include-deprecated brings it back, and --latest-only keeps the latest
    let all = export_dir();
    let args = [
        "toolbox",
        "export",
        "-t",
        &format!("{}/{}", user.group, world.base.name),
        "--include-deprecated",
        "-o",
        &all.to_string_lossy(),
    ];
    assert_success(&user.run("toolbox-export-include-deprecated", &args).await);
    assert_eq!(versions(&read_toolbox(&all)), ["1.0.0", "2.0.0"]);
    let latest = export_dir();
    let args = [
        "toolbox",
        "export",
        "-i",
        &format!("{}/{}", user.group, world.base.name),
        "--latest-only",
        "-o",
        &latest.to_string_lossy(),
    ];
    assert_success(&user.run("toolbox-export-latest-only", &args).await);
    assert_eq!(versions(&read_toolbox(&latest)), ["2.0.0"]);
    Ok(())
}

/// Exporting again into the same directory leaves it alone, stops at a difference, and replaces
/// or leaves it when asked
#[tokio::test]
async fn toolbox_export_reconciles_with_the_output() -> Result<(), Error> {
    // export a tool, then export it again
    let user = TestUser::new("toolbox-export-reconcile").await?;
    let (tool, _) = generators::tool(
        &[&user.group],
        "1.0.0",
        &image_spec("registry.local/one:1"),
        &user.client,
    )
    .await?;
    let dir = export_dir();
    let selector = format!("{}/{}", user.group, tool.name);
    let dir_text = dir.to_string_lossy().into_owned();
    let args = ["toolbox", "export", "-t", &selector, "-o", &dir_text];
    assert_success(&user.run("toolbox-reconcile-first", &args).await);
    let first = std::fs::read_to_string(dir.join("toolbox.json")).unwrap();
    // an identical export changes nothing
    assert_success(&user.run("toolbox-reconcile-again", &args).await);
    assert_eq!(
        std::fs::read_to_string(dir.join("toolbox.json")).unwrap(),
        first
    );
    // after a new version the same export stops and names the tool
    publish(&user, tool.id, "2.0.0", &image_spec("registry.local/one:2")).await?;
    let output = user.run("toolbox-reconcile-conflict", &args).await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains(&format!("tool {}", tool.name)));
    assert_eq!(
        std::fs::read_to_string(dir.join("toolbox.json")).unwrap(),
        first
    );
    // --skip-conflicts leaves it as it was
    let skip = [
        "toolbox",
        "export",
        "-t",
        &selector,
        "-o",
        &dir_text,
        "--skip-conflicts",
    ];
    assert_success(&user.run("toolbox-reconcile-skip", &skip).await);
    assert_eq!(
        std::fs::read_to_string(dir.join("toolbox.json")).unwrap(),
        first
    );
    // --overwrite replaces it
    let overwrite = [
        "toolbox",
        "export",
        "-t",
        &selector,
        "-o",
        &dir_text,
        "--overwrite",
    ];
    assert_success(&user.run("toolbox-reconcile-overwrite", &overwrite).await);
    let toolbox = read_toolbox(&dir);
    assert!(toolbox["tools"][&tool.name]["versions"]["2.0.0"].is_object());
    Ok(())
}

/// A diff reports each change one line at a time and exits 1 only when asked and something changes
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn toolbox_diff_reports_changes() -> Result<(), Error> {
    // an exported toolbox that matches Thorium
    let user = TestUser::new("toolbox-diff").await?;
    let world = Box::pin(world(&user)).await?;
    let dir = export_dir();
    assert_success(&export(&user, "toolbox-diff-export", &dir, &[]).await);
    let file = dir.join("toolbox.json");
    let path = file.to_string_lossy().into_owned();
    // a version published in Thorium and not in the toolbox is not something an import changes
    publish(
        &user,
        world.base.id,
        "3.0.0",
        &image_spec("registry.local/base:3"),
    )
    .await?;
    let output = user
        .run(
            "toolbox-diff-extra",
            &["toolbox", "diff", &path, "--exit-code"],
        )
        .await;
    assert_success(&output);
    // edit the toolbox: change a version's spec, add a version, and add a tool
    let mut toolbox = read_toolbox(&dir);
    let base = &world.base.name;
    toolbox["tools"][base]["versions"]["2.0.0"]["spec"]["Image"]["environment"]["K8s"]["image"] =
        json!("registry.local/base:changed");
    let mut added = toolbox["tools"][base]["versions"]["2.0.0"].clone();
    added["spec"]["Image"]["environment"]["K8s"]["image"] = json!("registry.local/base:4");
    toolbox["tools"][base]["versions"]["4.0.0"] = added;
    let mut fresh = toolbox["tools"][base].clone();
    fresh["versions"] = json!({"1.0.0": toolbox["tools"][base]["versions"]["4.0.0"]});
    fresh["latest"] = Value::Null;
    toolbox["tools"][unique("fresh")[..14].to_owned()] = fresh;
    let edited = dir.join("edited.json");
    std::fs::write(&edited, serde_json::to_string_pretty(&toolbox).unwrap()).unwrap();
    let edited_path = edited.to_string_lossy().into_owned();
    // without --exit-code the diff succeeds and lists the changes
    let output = user
        .run("toolbox-diff-changed", &["toolbox", "diff", &edited_path])
        .await;
    assert_success(&output);
    let text = stdout(&output);
    assert!(
        text.contains(&format!("~ {base}@2.0.0 (label conflict")),
        "{text}"
    );
    assert!(
        text.contains(&format!("+ {base}@4.0.0 (new version)")),
        "{text}"
    );
    assert!(text.contains("1.0.0 (new)"), "{text}");
    assert!(!text.contains(&format!("{base}@1.0.0")), "{text}");
    // with --exit-code it exits 1
    let output = user
        .run(
            "toolbox-diff-exit",
            &["toolbox", "diff", &edited_path, "--exit-code"],
        )
        .await;
    assert_eq!(output.status.code(), Some(1));
    // the JSON document has the plan and the counts
    let args = ["toolbox", "diff", &edited_path, "--format", "json"];
    let output = user.run("toolbox-diff-json", &args).await;
    assert_success(&output);
    let diff = json(&output);
    assert_eq!(diff["changed"], true);
    assert_eq!(diff["versions"]["label_conflict"], 1);
    assert_eq!(diff["versions"]["new_version"], 1);
    assert_eq!(diff["versions"]["new"], 1);
    // compared with another group of the user's, everything is new
    let other = unique("otherctl");
    let owners = GroupUsersRequest::default()
        .direct("thorium")
        .direct(&user.username);
    user.admin
        .groups
        .create(&GroupRequest::new(&other).owners(owners))
        .await?;
    let args = [
        "toolbox",
        "diff",
        &path,
        "--group-override",
        &other,
        "--format",
        "json",
    ];
    let output = user.run("toolbox-diff-other-group", &args).await;
    assert_success(&output);
    let diff = json(&output);
    assert_eq!(diff["versions"]["new"], 4, "{diff}");
    Ok(())
}

/// A removal lists its plan, needs confirmation, leaves what something outside the toolbox uses,
/// and deletes the rest in dependency order
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn toolbox_remove_follows_dependencies() -> Result<(), Error> {
    // an exported toolbox, and a pipeline outside it that runs `base`
    let user = TestUser::new("toolbox-remove").await?;
    let world = Box::pin(world(&user)).await?;
    let dir = export_dir();
    assert_success(&export(&user, "toolbox-remove-export", &dir, &[]).await);
    let path = dir.join("toolbox.json").to_string_lossy().into_owned();
    let outside = unique("outside")[..20].to_owned();
    let request = PipelineRequest::new(&user.group, &outside, json!([["only"]]))
        .stage_tool("only", world.base.id);
    user.client.pipelines.create(&request).await?;
    let exists = |id: Uuid| {
        let client = user.client.clone();
        async move { client.entities.get(id).await.is_ok() }
    };
    // a dry run lists the plan and deletes nothing
    let output = user
        .run(
            "toolbox-remove-dry-run",
            &["toolbox", "remove", &path, "--dry-run"],
        )
        .await;
    assert_success(&output);
    let text = stdout(&output);
    assert!(
        text.contains(&format!("pipeline {}/{}", user.group, world.pipeline)),
        "{text}"
    );
    assert!(text.contains("Left in place"), "{text}");
    assert!(exists(world.caller.id).await);
    // without a terminal or --skip-confirm it refuses
    let output = user
        .run("toolbox-remove-no-confirm", &["toolbox", "remove", &path])
        .await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--skip-confirm"));
    assert!(exists(world.caller.id).await);
    // removing leaves `base`, which the outside pipeline runs, and exits non-zero
    let output = user
        .run("toolbox-remove-run", &["toolbox", "remove", &path, "--yes"])
        .await;
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(stderr.contains("not fully removed"), "{stderr}");
    assert!(
        user.client
            .pipelines
            .get(&user.group, &world.pipeline)
            .await
            .is_err()
    );
    for gone in [
        &world.workflow,
        &world.agent,
        &world.toolset,
        &world.caller,
        &world.dependent,
    ] {
        assert!(!exists(gone.id).await, "{} was not removed", gone.name);
    }
    assert!(exists(world.base.id).await);
    assert!(exists(world.base_v1.id).await);
    // once the outside pipeline is gone a second removal finishes the job
    user.client.pipelines.delete(&user.group, &outside).await?;
    let output = user
        .run(
            "toolbox-remove-finish",
            &["toolbox", "remove", &path, "--skip-confirm"],
        )
        .await;
    assert_success(&output);
    assert!(!exists(world.base.id).await);
    // and a third finds nothing to do
    let output = user
        .run(
            "toolbox-remove-nothing",
            &["toolbox", "remove", &path, "--skip-confirm"],
        )
        .await;
    assert_success(&output);
    assert!(stdout(&output).contains("Nothing to remove"));
    // nothing of the toolbox's is left in the group
    let remaining = user
        .client
        .entities
        .list(
            &thorium::models::EntityListOpts::default()
                .kinds([EntityKinds::Tool])
                .groups(vec![user.group.clone()]),
        )
        .await?;
    assert_eq!(remaining.data.len(), 0);
    Ok(())
}

/// A legacy toolbox works for diff and remove, read as the tools its images convert to
#[tokio::test]
async fn toolbox_diff_and_remove_accept_legacy_toolboxes() -> Result<(), Error> {
    // the small legacy toolbox, compared in the user's group
    let user = TestUser::new("toolbox-legacy").await?;
    let legacy = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/toolbox_v2/small.legacy.json"
    );
    let args = [
        "toolbox",
        "diff",
        legacy,
        "--group-override",
        &user.group,
        "--exit-code",
    ];
    let output = user.run("toolbox-legacy-diff", &args).await;
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stdout(&output).contains("clamav@latest (new)"),
        "{}",
        stdout(&output)
    );
    // nothing from it exists, so a removal has nothing to do
    let args = [
        "toolbox",
        "remove",
        legacy,
        "--group-override",
        &user.group,
        "--dry-run",
    ];
    let output = user.run("toolbox-legacy-remove", &args).await;
    assert_success(&output);
    assert!(
        stdout(&output).contains("Nothing to remove"),
        "{}",
        stdout(&output)
    );
    Ok(())
}
