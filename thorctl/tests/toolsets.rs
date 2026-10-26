//! Tests the thorctl toolsets commands and `agents link-toolset` against the test API

mod common;

use common::editor::{EditorScript, Step, recovery_path};
use common::{TestUser, assert_success, json, stdout};
use serde_json::Value;
use std::process::{Output, Stdio};
use thorium::Error;
use thorium::models::{
    AgentEntityRequest, AssociationKind, AssociationTarget, Directionality, Entity, EntityKinds,
    EntityListOpts, EntityMetadata, EntityMetadataUpdate, EntityUpdate, ToolSetEntity,
    ToolSetSelector,
};
use thorium::test_utilities::generators;
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

/// A shell command that runs thorctl as the user, for the hook that changes a toolset between
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

/// Read a toolset as the user's client sees it
///
/// # Arguments
///
/// * `user` - The test user
/// * `id` - The toolset's id
async fn read_toolset(user: &TestUser, id: Uuid) -> Result<(Entity, ToolSetEntity), Error> {
    // read the entity and take its toolset metadata
    let entity = user.client.entities.get(id).await?;
    let EntityMetadata::ToolSet(meta) = entity.metadata.clone() else {
        panic!("expected a toolset");
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

/// Get the sorted ids an entity's outgoing edges of one kind point to
///
/// # Arguments
///
/// * `user` - The test user whose admin client to read with
/// * `id` - The entity to read the edges of
/// * `kind` - The kind of edge
async fn out_edges(user: &TestUser, id: Uuid, kind: AssociationKind) -> Result<Vec<Uuid>, Error> {
    // list the entity's associations and keep its outgoing edges of this kind
    let mut ids = user
        .admin
        .entities
        .associations(id)
        .await?
        .into_iter()
        .filter(|assoc| assoc.kind == kind && assoc.direction == Directionality::To)
        .filter_map(|assoc| match assoc.other {
            AssociationTarget::Entity { id, .. } => Some(id),
            _ => None,
        })
        .collect::<Vec<Uuid>>();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// Find the id of a toolset by name in the user's group
///
/// # Arguments
///
/// * `user` - The test user
/// * `name` - The toolset's name
async fn toolset_id(user: &TestUser, name: &str) -> Result<Uuid, Error> {
    // list the toolsets in the user's group and find the name
    let opts = EntityListOpts::default()
        .groups(vec![user.group.clone()])
        .kind(EntityKinds::ToolSet)
        .limit(100);
    let listed = user.client.entities.list(&opts).await?;
    Ok(listed
        .data
        .iter()
        .find(|line| line.name == name)
        .map(|line| line.id)
        .expect("the toolset wasn't created"))
}

/// Sort a list of ids
///
/// # Arguments
///
/// * `ids` - The ids to sort
fn sorted(ids: &[Uuid]) -> Vec<Uuid> {
    // copy and sort the ids
    let mut ids = ids.to_vec();
    ids.sort();
    ids
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

/// `toolsets create` with tools, a pin, a selector, and the read-only flag creates the expected
/// edges and fields, `describe` shows them, and `agents link-toolset` links with and without a
/// pin
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn toolsets_create_and_link() -> Result<(), Error> {
    // a user named after io in a group named after minerva
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "toolsets-create-link",
        &generators::readable_name("io", &suffix),
        &generators::readable_name("minerva", &suffix),
    )
    .await?;
    // two tools, one included at latest and one pinned
    let (latest, _) = agentic_tool(&user).await?;
    let (pinned_tool, pinned) = agentic_tool(&user).await?;
    let pinned_id = pinned.id.to_string();
    let args = [
        "toolsets",
        "create",
        "ghidra-read-only",
        "-g",
        &user.group,
        "--tool",
        &latest.name,
        "--pin",
        &pinned_id,
        "--selector",
        "toolset=ghidra",
        "--read-only",
    ];
    assert_success(&user.run("toolsets-create", &args).await);
    // the toolset has both includes and its settings
    let id = toolset_id(&user, "ghidra-read-only").await?;
    assert_eq!(
        out_edges(&user, id, AssociationKind::Includes).await?,
        sorted(&[latest.id, pinned.id])
    );
    let args = ["toolsets", "describe", "ghidra-read-only", "-o", "json"];
    let described = json(&user.run("toolsets-describe-json", &args).await);
    let meta = &described["metadata"]["ToolSet"];
    assert_eq!(meta["read_only"], Value::Bool(true));
    assert_eq!(meta["selector"]["key"], Value::String("toolset".into()));
    assert_eq!(meta["selector"]["value"], Value::String("ghidra".into()));
    assert_eq!(meta["tools"].as_array().map(Vec::len), Some(2));
    // describe shows the settings and tools as text
    let out = user
        .run(
            "toolsets-describe",
            &["toolsets", "describe", "ghidra-read-only"],
        )
        .await;
    assert_success(&out);
    let text = stdout(&out);
    for expected in [
        "toolset=ghidra",
        latest.name.as_str(),
        pinned.name.as_str(),
        "pinned",
        "Read only",
    ] {
        assert!(
            text.contains(expected),
            "describe is missing {expected}:\n{text}"
        );
    }
    assert!(!text.contains(&pinned_tool.id.to_string()), "{text}");
    // get lists the toolset
    let args = ["toolsets", "get", "-g", &user.group, "-o", "json"];
    let listed = json(&user.run("toolsets-get-json", &args).await);
    assert_eq!(listed.as_array().map(Vec::len), Some(1));
    // a ready instance to pin, submitted through the API
    let tool_set = user.client.entities.get(id).await?;
    let instance = generators::gen_toolset_instance(&tool_set, 1, None, &[&pinned]);
    let instance_id = user
        .client
        .entities
        .create(generators::gen_toolset_instance_req(&instance))
        .await?
        .id;
    // a second toolset to follow at latest
    let args = ["toolsets", "create", "radare", "-g", &user.group];
    assert_success(&user.run("toolsets-create-radare", &args).await);
    let radare = toolset_id(&user, "radare").await?;
    // an agent that links one toolset at latest and pins the other's instance
    let config = serde_json::to_string(&generators::gen_agent_config("reverser"))?;
    let path = std::env::temp_dir().join(format!("{}-agent.json", Uuid::new_v4().simple()));
    std::fs::write(&path, config)?;
    let path = path.to_string_lossy().into_owned();
    let args = [
        "agents",
        "create",
        "-n",
        "reverser",
        "-g",
        &user.group,
        "--config",
        &path,
    ];
    assert_success(&user.run("agents-create-reverser", &args).await);
    let args = ["agents", "link-toolset", "reverser", "radare"];
    assert_success(&user.run("agents-link-toolset", &args).await);
    let instance_arg = instance_id.to_string();
    let args = [
        "agents",
        "link-toolset",
        "reverser",
        "ghidra-read-only",
        "--pin",
        &instance_arg,
    ];
    assert_success(&user.run("agents-link-toolset-pin", &args).await);
    // the agent calls the toolset and the pinned instance
    let opts = EntityListOpts::default()
        .groups(vec![user.group.clone()])
        .kind(EntityKinds::Agent)
        .limit(10);
    let agent = user.client.entities.list(&opts).await?.data[0].id;
    assert_eq!(
        out_edges(&user, agent, AssociationKind::Calls).await?,
        sorted(&[radare, instance_id])
    );
    // pinning an instance of another toolset is refused before any request
    let args = [
        "agents",
        "link-toolset",
        "reverser",
        "radare",
        "--pin",
        &instance_arg,
    ];
    let out = user.run("agents-link-toolset-wrong-pin", &args).await;
    assert!(!out.status.success());
    Ok(())
}

/// The toolset edit commands each change exactly the named edge or field, a non-agentic include
/// prints the API's 400, and a selector without `=` fails before any request
#[tokio::test]
async fn toolsets_edits() -> Result<(), Error> {
    // a user named after europa in a group named after mars, and an empty toolset
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "toolsets-edits",
        &generators::readable_name("europa", &suffix),
        &generators::readable_name("mars", &suffix),
    )
    .await?;
    let args = ["toolsets", "create", "editable", "-g", &user.group];
    assert_success(&user.run("toolsets-create-editable", &args).await);
    let id = toolset_id(&user, "editable").await?;
    let read = || async {
        let args = ["toolsets", "describe", "editable", "-o", "json"];
        json(&user.run("toolsets-describe-json", &args).await)["metadata"]["ToolSet"].clone()
    };
    // include one tool at latest and another pinned
    let (latest, _) = agentic_tool(&user).await?;
    let (pinned_tool, pinned) = agentic_tool(&user).await?;
    let args = ["toolsets", "include", "editable", &latest.name];
    assert_success(&user.run("toolsets-include", &args).await);
    assert_eq!(
        out_edges(&user, id, AssociationKind::Includes).await?,
        vec![latest.id]
    );
    let pinned_id = pinned.id.to_string();
    let pinned_tool_id = pinned_tool.id.to_string();
    let args = [
        "toolsets",
        "include",
        "editable",
        &pinned_tool_id,
        "--pin",
        &pinned_id,
    ];
    assert_success(&user.run("toolsets-include-pin", &args).await);
    assert_eq!(
        out_edges(&user, id, AssociationKind::Includes).await?,
        sorted(&[latest.id, pinned.id])
    );
    // remove the latest edge only
    let args = ["toolsets", "remove-tool", "editable", &latest.name];
    assert_success(&user.run("toolsets-remove-tool", &args).await);
    assert_eq!(
        out_edges(&user, id, AssociationKind::Includes).await?,
        vec![pinned.id]
    );
    // set, replace, and clear the selector
    let args = ["toolsets", "selector", "editable", "toolset=ghidra"];
    assert_success(&user.run("toolsets-selector", &args).await);
    let meta = read().await;
    assert_eq!(meta["selector"]["value"], Value::String("ghidra".into()));
    assert_eq!(meta["read_only"], Value::Bool(false));
    let args = ["toolsets", "selector", "editable", "suite=radare"];
    assert_success(&user.run("toolsets-selector-replace", &args).await);
    let stored = user.client.entities.get(id).await?;
    let thorium::models::EntityMetadata::ToolSet(stored) = &stored.metadata else {
        panic!("expected a toolset");
    };
    assert_eq!(
        stored.selector,
        Some(ToolSetSelector::new("suite", "radare"))
    );
    let args = ["toolsets", "selector", "editable", "--clear"];
    assert_success(&user.run("toolsets-selector-clear", &args).await);
    assert!(read().await.get("selector").is_none());
    // turn read-only on and off
    let args = ["toolsets", "read-only", "editable", "on"];
    assert_success(&user.run("toolsets-read-only-on", &args).await);
    assert_eq!(read().await["read_only"], Value::Bool(true));
    let args = ["toolsets", "read-only", "editable", "off"];
    assert_success(&user.run("toolsets-read-only-off", &args).await);
    let meta = read().await;
    assert_eq!(meta["read_only"], Value::Bool(false));
    assert_eq!(meta["tools"].as_array().map(Vec::len), Some(1));
    // including an Image tool prints the API's 400
    let (image, _) = generators::tool(
        &[&user.group],
        "1.0.0",
        &generators::gen_image_spec(),
        &user.client,
    )
    .await?;
    let image_id = image.id.to_string();
    let out = user
        .run(
            "toolsets-include-image",
            &["toolsets", "include", "editable", &image_id],
        )
        .await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("400"), "{stderr}");
    // a selector without `=` fails before any request
    let out = user
        .run(
            "toolsets-selector-bad",
            &["toolsets", "selector", "no-such-toolset", "toolset"],
        )
        .await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("key=value"), "{stderr}");
    assert!(!stderr.contains("No toolset"), "{stderr}");
    Ok(())
}

/// `toolsets update` changes fields from typed flags, `--set`, `--unset`, `--patch`, `--file` and
/// stdin, prints the result as JSON, and a spec round trips without changes
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn toolsets_update_forms() -> Result<(), Error> {
    // a user named after rhea in a group named after ceres, and a second group
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "toolsets-update-forms",
        &generators::readable_name("rhea", &suffix),
        &generators::readable_name("ceres", &suffix),
    )
    .await?;
    let other = extra_group(&user, &generators::readable_name("vesta", &suffix)).await?;
    // the first tool is in both groups so the toolset can move between them
    let (first, _) = generators::tool(
        &[&user.group, &other],
        "1.0.0",
        &generators::gen_agentic_spec(),
        &user.client,
    )
    .await?;
    let (second, second_r1) = agentic_tool(&user).await?;
    let args = [
        "toolsets",
        "create",
        "kit",
        "-g",
        &user.group,
        "--tool",
        &first.name,
    ];
    assert_success(&user.run("toolsets-create-kit", &args).await);
    let id = toolset_id(&user, "kit").await?;
    // a rename and a description
    let args = [
        "toolsets",
        "update",
        "kit",
        "--rename",
        "kit2",
        "-d",
        "reverse engineering",
    ];
    let out = user.run("toolsets-update-rename", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("updated"), "{}", stdout(&out));
    let (entity, _) = read_toolset(&user, id).await?;
    assert_eq!(entity.name, "kit2");
    assert_eq!(entity.description.as_deref(), Some("reverse engineering"));
    // a group is added and removed
    let args = ["toolsets", "update", "kit2", "--add-group", &other];
    assert_success(&user.run("toolsets-update-add-group", &args).await);
    let (entity, _) = read_toolset(&user, id).await?;
    assert!(entity.groups.contains(&other) && entity.groups.contains(&user.group));
    let args = ["toolsets", "update", "kit2", "--remove-group", &user.group];
    assert_success(&user.run("toolsets-update-remove-group", &args).await);
    assert_eq!(read_toolset(&user, id).await?.0.groups, vec![other.clone()]);
    // both flags in one update swap the groups back, so the tools are still in the toolset's group
    let args = [
        "toolsets",
        "update",
        "kit2",
        "--add-group",
        &user.group,
        "--remove-group",
        &other,
    ];
    assert_success(&user.run("toolsets-update-swap-groups", &args).await);
    assert_eq!(
        read_toolset(&user, id).await?.0.groups,
        vec![user.group.clone()]
    );
    // the instance switch and the read-only setting, with the JSON result pinned
    let args = ["toolsets", "update", "kit2", "--auto-instance", "off"];
    assert_success(&user.run("toolsets-update-auto-instance", &args).await);
    assert!(!read_toolset(&user, id).await?.1.auto_instance);
    let args = [
        "toolsets",
        "update",
        "kit2",
        "--set",
        "read_only=true",
        "-o",
        "json",
    ];
    let result = json(&user.run("toolsets-update-set-json", &args).await);
    assert_eq!(result["kind"], "toolset");
    assert_eq!(result["name"], "kit2");
    assert_eq!(result["status"], "updated");
    assert_eq!(result["changed"], true);
    assert_eq!(result["diff"][0]["path"], "read_only");
    assert_eq!(result["diff"][0]["old"], false);
    assert_eq!(result["diff"][0]["new"], true);
    assert_eq!(result["resource"]["read_only"], true);
    assert!(read_toolset(&user, id).await?.1.read_only);
    // a selector is set from --set, replaced from a patch, and removed with --unset
    let args = ["toolsets", "update", "kit2", "--set", "selector=suite=re"];
    assert_success(&user.run("toolsets-update-selector", &args).await);
    assert_eq!(
        read_toolset(&user, id).await?.1.selector,
        Some(ToolSetSelector::new("suite", "re"))
    );
    let args = [
        "toolsets",
        "update",
        "kit2",
        "--patch",
        r#"{"selector": "suite=asm"}"#,
    ];
    assert_success(&user.run("toolsets-update-patch", &args).await);
    assert_eq!(
        read_toolset(&user, id).await?.1.selector,
        Some(ToolSetSelector::new("suite", "asm"))
    );
    let args = ["toolsets", "update", "kit2", "--unset", "selector"];
    assert_success(&user.run("toolsets-update-unset", &args).await);
    assert_eq!(read_toolset(&user, id).await?.1.selector, None);
    // the description is cleared
    let args = ["toolsets", "update", "kit2", "--clear-description"];
    assert_success(&user.run("toolsets-update-clear-description", &args).await);
    assert_eq!(read_toolset(&user, id).await?.0.description, None);
    // tools are swapped: the followed one for a pinned version of another
    let tools = format!("tools=[{{tool: {}, version: \"1.0.0\"}}]", second.name);
    let args = ["toolsets", "update", "kit2", "--set", &tools];
    assert_success(&user.run("toolsets-update-tools", &args).await);
    assert_eq!(
        out_edges(&user, id, AssociationKind::Includes).await?,
        vec![second_r1.id]
    );
    // the spec lists the tools by name, and updating with it changes nothing
    let spec = stdout(
        &user
            .run("toolsets-spec", &["toolsets", "spec", "kit2"])
            .await,
    );
    for expected in [
        "kit2",
        "*id*",
        second.name.as_str(),
        "1.0.0",
        "auto_instance",
    ] {
        assert!(spec.contains(expected), "spec lacks {expected}:\n{spec}");
    }
    let args = ["toolsets", "update", "kit2", "--file", "-"];
    let out = run_with(&user, "toolsets-update-spec-same", &args, &[], Some(&spec)).await;
    assert_success(&out);
    assert!(stdout(&out).contains("No changes"), "{}", stdout(&out));
    // a changed spec applies from stdin and from a file
    let changed = spec.replace("read_only: true", "read_only: false");
    let out = run_with(
        &user,
        "toolsets-update-spec-stdin",
        &args,
        &[],
        Some(&changed),
    )
    .await;
    assert_success(&out);
    assert!(!read_toolset(&user, id).await?.1.read_only);
    let path = write_file("spec.yaml", &spec);
    let args = ["toolsets", "update", "kit2", "--file", &path];
    assert_success(&user.run("toolsets-update-spec-file", &args).await);
    assert!(read_toolset(&user, id).await?.1.read_only);
    // spec as JSON is a document, and -o writes a file
    let args = ["toolsets", "spec", "kit2", "--format", "json"];
    let doc = json(&user.run("toolsets-spec-json", &args).await);
    assert_eq!(doc["name"], "kit2");
    assert_eq!(doc["tools"][0]["version"], "1.0.0");
    let out_path = std::env::temp_dir().join(format!("{}-spec.yaml", Uuid::new_v4().simple()));
    let out_str = out_path.to_string_lossy().into_owned();
    let args = ["toolsets", "spec", "kit2", "-o", &out_str];
    assert_success(&user.run("toolsets-spec-file", &args).await);
    assert!(std::fs::read_to_string(&out_path)?.contains("kit2"));
    Ok(())
}

/// `toolsets update` refuses a bad change before sending anything, reports the API's refusal,
/// does nothing for no change or a dry run, and stops when the toolset changed meanwhile
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn toolsets_update_refusals() -> Result<(), Error> {
    // a user named after dione in a group named after pallas
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "toolsets-update-refusals",
        &generators::readable_name("dione", &suffix),
        &generators::readable_name("pallas", &suffix),
    )
    .await?;
    let args = ["toolsets", "create", "guard", "-g", &user.group];
    assert_success(&user.run("toolsets-create-guard", &args).await);
    let id = toolset_id(&user, "guard").await?;
    // nothing to update without a change flag
    let out = user
        .run("toolsets-update-nothing", &["toolsets", "update", "guard"])
        .await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("Nothing to update"));
    // setting the current value changes nothing
    let args = ["toolsets", "update", "guard", "--set", "read_only=false"];
    let out = user.run("toolsets-update-no-change", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("No changes"), "{}", stdout(&out));
    // a dry run shows the change and writes nothing
    let args = [
        "toolsets",
        "update",
        "guard",
        "--set",
        "read_only=true",
        "--dry-run",
    ];
    let out = user.run("toolsets-update-dry-run", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("read_only"), "{}", stdout(&out));
    assert!(!read_toolset(&user, id).await?.1.read_only);
    // a read-only field cannot be set
    let args = ["toolsets", "update", "guard", "--set", "id=x"];
    let out = user.run("toolsets-update-read-only", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("read-only"));
    // a document with an unknown key is refused
    let path = write_file("bad.yaml", "colour: red\n");
    let args = ["toolsets", "update", "guard", "--file", &path];
    let out = user.run("toolsets-update-unknown-key", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("colour"));
    // a selector without a value is refused before any request
    let args = ["toolsets", "update", "guard", "--set", "selector=suite"];
    let out = user.run("toolsets-update-bad-selector", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("key=value"));
    // a tool that doesn't exist is refused before the update is sent
    let args = [
        "toolsets",
        "update",
        "guard",
        "--set",
        "tools=[{tool: no-such-tool}]",
    ];
    let out = user.run("toolsets-update-unknown-tool", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no-such-tool"));
    // the API's refusal is printed: a tool that isn't agentic
    let (image, _) = generators::tool(
        &[&user.group],
        "1.0.0",
        &generators::gen_image_spec(),
        &user.client,
    )
    .await?;
    let tools = format!("tools=[{{tool: {}}}]", image.name);
    let args = ["toolsets", "update", "guard", "--set", &tools];
    let out = user.run("toolsets-update-api-refusal", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("400"));
    assert_eq!(
        out_edges(&user, id, AssociationKind::Includes).await?,
        Vec::<Uuid>::new()
    );
    // a toolset changed between the read and the apply is refused, and --force applies anyway
    let hook_env = hook(&user, "toolsets update guard -d changed-remotely");
    let envs = [("THORCTL_EDIT_BEFORE_APPLY", hook_env.as_str())];
    let args = ["toolsets", "update", "guard", "--set", "read_only=true"];
    let out = run_with(&user, "toolsets-update-remote-change", &args, &envs, None).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("changed remotely"));
    assert!(!read_toolset(&user, id).await?.1.read_only);
    let args = [
        "toolsets",
        "update",
        "guard",
        "--set",
        "read_only=true",
        "--force",
    ];
    let out = run_with(&user, "toolsets-update-remote-force", &args, &envs, None).await;
    assert_success(&out);
    assert!(read_toolset(&user, id).await?.1.read_only);
    Ok(())
}

/// `toolsets edit` in an editor applies a change, shows the header and read-only fields, leaves
/// an unchanged buffer alone, reopens after a bad edit, saves a cancelled bad edit to a recovery
/// file, and reports the API's refusal with the recovery file
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn toolsets_edit_interactive() -> Result<(), Error> {
    // a user named after eris in a group named after sylvia
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "toolsets-edit-interactive",
        &generators::readable_name("eris", &suffix),
        &generators::readable_name("sylvia", &suffix),
    )
    .await?;
    let args = ["toolsets", "create", "editable", "-g", &user.group];
    assert_success(&user.run("toolsets-create-editable", &args).await);
    let id = toolset_id(&user, "editable").await?;
    // a change made in the editor is applied, and the buffer had a header and the read-only id
    let script = EditorScript::replace("read_only: false", "read_only: true");
    let editor = script.editor();
    let args = ["toolsets", "edit", "editable", "--editor", &editor];
    let out = run_with(&user, "toolsets-edit-change", &args, &EDITOR_ENV, None).await;
    assert_success(&out);
    assert!(stdout(&out).contains("updated"), "{}", stdout(&out));
    assert!(read_toolset(&user, id).await?.1.read_only);
    let seen = script.seen(0).expect("the editor saw no buffer");
    assert!(seen.contains("# Editing toolset 'editable'"), "{seen}");
    assert!(
        seen.contains("*id*") && seen.contains("read-only"),
        "{seen}"
    );
    // an unchanged buffer changes nothing
    let script = EditorScript::noop();
    let editor = script.editor();
    let args = ["toolsets", "edit", "editable", "--editor", &editor];
    let out = run_with(&user, "toolsets-edit-noop", &args, &EDITOR_ENV, None).await;
    assert_success(&out);
    assert!(stdout(&out).contains("No changes"), "{}", stdout(&out));
    // an empty buffer cancels
    let script = EditorScript::write("");
    let editor = script.editor();
    let args = ["toolsets", "edit", "editable", "--editor", &editor];
    let out = run_with(&user, "toolsets-edit-cancel", &args, &EDITOR_ENV, None).await;
    assert_success(&out);
    assert!(stdout(&out).contains("Cancelled"), "{}", stdout(&out));
    // editing the read-only id is reported and ignored
    let script = EditorScript::replace(&id.to_string(), "00000000-0000-0000-0000-000000000000");
    let editor = script.editor();
    let args = ["toolsets", "edit", "editable", "--editor", &editor];
    let out = run_with(&user, "toolsets-edit-read-only", &args, &EDITOR_ENV, None).await;
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
    let args = ["toolsets", "edit", "editable", "--editor", &editor];
    let out = run_with(&user, "toolsets-edit-reedit", &args, &envs, None).await;
    assert_success(&out);
    assert_eq!(script.invocations(), 2);
    assert!(String::from_utf8_lossy(&out.stderr).contains("colour"));
    // cancelling after a bad edit fails and saves the text to a recovery file
    let script = EditorScript::append("colour: red");
    let editor = script.editor();
    let args = ["toolsets", "edit", "editable", "--editor", &editor];
    let out = run_with(&user, "toolsets-edit-bad-cancel", &args, &EDITOR_ENV, None).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let path = recovery_path(&stderr).expect("no recovery file named");
    assert!(std::fs::read_to_string(&path)?.contains("colour: red"));
    std::fs::remove_file(&path)?;
    // a change the API refuses is reported with its reason and the recovery file
    let (image, _) = generators::tool(
        &[&user.group],
        "1.0.0",
        &generators::gen_image_spec(),
        &user.client,
    )
    .await?;
    let script = EditorScript::replace("tools: []", &format!("tools: [{{tool: {}}}]", image.name));
    let editor = script.editor();
    let args = ["toolsets", "edit", "editable", "--editor", &editor];
    let out = run_with(&user, "toolsets-edit-api-refusal", &args, &EDITOR_ENV, None).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(stderr.contains("400"), "{stderr}");
    let path = recovery_path(&stderr).expect("no recovery file named");
    std::fs::remove_file(&path)?;
    // a dry run shows the changes and writes nothing
    let script = EditorScript::replace("auto_instance: true", "auto_instance: false");
    let editor = script.editor();
    let args = [
        "toolsets",
        "edit",
        "editable",
        "--editor",
        &editor,
        "--dry-run",
    ];
    let out = run_with(&user, "toolsets-edit-dry-run", &args, &EDITOR_ENV, None).await;
    assert_success(&out);
    assert!(stdout(&out).contains("auto_instance"), "{}", stdout(&out));
    assert!(read_toolset(&user, id).await?.1.auto_instance);
    // declining the confirmation applies nothing and saves the edit
    let declined = [
        ("THORCTL_EDIT_ASSUME_TTY", "1"),
        ("THORCTL_EDIT_PROMPT", "cancel"),
    ];
    let editor = script.editor();
    let args = ["toolsets", "edit", "editable", "--editor", &editor];
    let out = run_with(&user, "toolsets-edit-declined", &args, &declined, None).await;
    assert_success(&out);
    assert!(stdout(&out).contains("Cancelled"), "{}", stdout(&out));
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let path = recovery_path(&stderr.replace("Your edits were saved to", "saved to"))
        .expect("no recovery file named");
    std::fs::remove_file(&path)?;
    assert!(read_toolset(&user, id).await?.1.auto_instance);
    // a toolset that changed during the edit is refused unless forced
    let hook_env = hook(&user, "toolsets update editable -d changed-remotely");
    let envs = [
        ("THORCTL_EDIT_ASSUME_TTY", "1"),
        ("THORCTL_EDIT_BEFORE_APPLY", hook_env.as_str()),
    ];
    let args = ["toolsets", "edit", "editable", "--editor", &editor];
    let out = run_with(&user, "toolsets-edit-remote-change", &args, &envs, None).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(stderr.contains("changed remotely"), "{stderr}");
    if let Some(path) = recovery_path(&stderr) {
        std::fs::remove_file(path)?;
    }
    let args = [
        "toolsets", "edit", "editable", "--editor", &editor, "--force",
    ];
    let out = run_with(&user, "toolsets-edit-remote-force", &args, &envs, None).await;
    assert_success(&out);
    assert!(!read_toolset(&user, id).await?.1.auto_instance);
    Ok(())
}

/// `toolsets create --spec` builds a toolset from a printed spec on stdin or in a file, flags
/// override the spec, and a bad spec fails before anything is created
#[tokio::test]
async fn toolsets_create_from_spec() -> Result<(), Error> {
    // a user named after leda in a group named after maia
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "toolsets-create-spec",
        &generators::readable_name("leda", &suffix),
        &generators::readable_name("maia", &suffix),
    )
    .await?;
    // a toolset with a tool, a selector and both settings changed
    let (tool, _) = agentic_tool(&user).await?;
    let args = [
        "toolsets",
        "create",
        "original",
        "-g",
        &user.group,
        "--tool",
        &tool.name,
        "--selector",
        "suite=re",
        "--read-only",
        "--no-auto-instance",
    ];
    assert_success(&user.run("toolsets-create-original", &args).await);
    let spec = stdout(
        &user
            .run("toolsets-spec-original", &["toolsets", "spec", "original"])
            .await,
    );
    // the spec on stdin creates a copy under another name, with the same settings and tools
    let spec = spec.replace("name: original", "name: copy");
    let args = ["toolsets", "create", "--spec", "-"];
    let out = run_with(&user, "toolsets-create-spec-stdin", &args, &[], Some(&spec)).await;
    assert_success(&out);
    assert!(
        stdout(&out).contains("Created toolset copy"),
        "{}",
        stdout(&out)
    );
    let copy = toolset_id(&user, "copy").await?;
    let (entity, meta) = read_toolset(&user, copy).await?;
    assert_eq!(entity.groups, vec![user.group.clone()]);
    assert!(meta.read_only && !meta.auto_instance);
    assert_eq!(meta.selector, Some(ToolSetSelector::new("suite", "re")));
    assert_eq!(
        out_edges(&user, copy, AssociationKind::Includes).await?,
        vec![tool.id]
    );
    // the same spec from a file, with a name and a description from flags
    let path = write_file("spec.yaml", &spec);
    let args = [
        "toolsets",
        "create",
        "second-copy",
        "--spec",
        &path,
        "-d",
        "made from a spec",
    ];
    assert_success(&user.run("toolsets-create-spec-file", &args).await);
    let second = toolset_id(&user, "second-copy").await?;
    let (entity, meta) = read_toolset(&user, second).await?;
    assert_eq!(entity.description.as_deref(), Some("made from a spec"));
    assert!(meta.read_only);
    // a spec with an unknown key fails before anything is created
    let bad = write_file("bad.yaml", "name: broken\ngroups: [x]\ncolour: red\n");
    let args = ["toolsets", "create", "--spec", &bad];
    let out = user.run("toolsets-create-spec-bad", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("colour"));
    // a spec that names a tool that doesn't exist fails the same way
    let missing = write_file(
        "missing.yaml",
        &format!(
            "name: ghost\ngroups: [{}]\ntools: [{{tool: no-such-tool}}]\n",
            user.group
        ),
    );
    let args = ["toolsets", "create", "--spec", &missing];
    let out = user.run("toolsets-create-spec-missing-tool", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no-such-tool"));
    let opts = EntityListOpts::default()
        .groups(vec![user.group.clone()])
        .kind(EntityKinds::ToolSet)
        .limit(100);
    let listed = user.client.entities.list(&opts).await?;
    assert!(
        !listed
            .data
            .iter()
            .any(|line| line.name == "ghost" || line.name == "broken")
    );
    // a spec file that doesn't exist is named in the error
    let args = ["toolsets", "create", "--spec", "/no/such/spec.yaml"];
    let out = user.run("toolsets-create-spec-no-file", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("/no/such/spec.yaml"));
    Ok(())
}

/// `toolsets delete` does nothing for a dry run, needs `--skip-confirm` without a terminal, and
/// deletes the toolset otherwise
#[tokio::test]
async fn toolsets_delete() -> Result<(), Error> {
    // a user named after thalia in a group named after clio
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "toolsets-delete",
        &generators::readable_name("thalia", &suffix),
        &generators::readable_name("clio", &suffix),
    )
    .await?;
    let args = ["toolsets", "create", "doomed", "-g", &user.group];
    assert_success(&user.run("toolsets-create-doomed", &args).await);
    let id = toolset_id(&user, "doomed").await?;
    // a dry run names what would be deleted and keeps the toolset
    let args = ["toolsets", "delete", "doomed", "--dry-run"];
    let out = user.run("toolsets-delete-dry-run", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("doomed"), "{}", stdout(&out));
    assert!(user.client.entities.get(id).await.is_ok());
    // without a terminal the command asks for the flag instead of prompting
    let args = ["toolsets", "delete", "doomed"];
    let out = user.run("toolsets-delete-no-tty", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--skip-confirm"));
    assert!(user.client.entities.get(id).await.is_ok());
    // with the flag the toolset is deleted
    let args = ["toolsets", "delete", "doomed", "--yes"];
    let out = user.run("toolsets-delete", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("Deleted"), "{}", stdout(&out));
    assert!(user.client.entities.get(id).await.is_err());
    // deleting it again says there is no such toolset
    let out = user.run("toolsets-delete-again", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("doomed"));
    Ok(())
}

/// `toolsets delete` and `tools delete` print the API's refusal as one clean line while an
/// agent calls the toolset or a toolset pins the version, and delete once the referrer is gone
#[tokio::test]
async fn deletes_refused_while_referenced() -> Result<(), Error> {
    // a user named after urania in a group named after erato, with a toolset pinning a tool
    // version that an agent calls through the toolset
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "toolsets-delete-refused",
        &generators::readable_name("urania", &suffix),
        &generators::readable_name("erato", &suffix),
    )
    .await?;
    let (_, tool) = agentic_tool(&user).await?;
    let args = ["toolsets", "create", "kit", "-g", &user.group];
    assert_success(&user.run("toolsets-delete-refused-create", &args).await);
    let kit = toolset_id(&user, "kit").await?;
    let pin = EntityMetadataUpdate::ToolSet {
        read_only: None,
        selector: None,
        clear_selector: None,
        auto_instance: None,
        add_tools: vec![tool.id],
        remove_tools: Vec::new(),
        extend_target_groups: false,
    };
    user.client
        .entities
        .update(kit, EntityUpdate::default().metadata(pin))
        .await?;
    let config = generators::gen_agent_config("caller");
    let request = AgentEntityRequest::new(config).tool_set(kit);
    let caller = user
        .client
        .entities
        .create(generators::gen_agent("caller", &[&user.group], request))
        .await?
        .id;
    // the toolset delete is one line naming the agent, and nothing is deleted
    let args = ["toolsets", "delete", "kit", "--yes"];
    let out = user.run("toolsets-delete-refused", &args).await;
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(err.trim().lines().count(), 1, "{err}");
    assert!(err.contains("caller"), "{err}");
    assert!(err.contains("still referenced"), "{err}");
    assert!(!err.contains("Code"), "{err}");
    assert!(user.client.entities.get(kit).await.is_ok());
    // the pinned version's delete is one line naming the toolset
    let args = ["tools", "delete", &tool.name, "--yes"];
    let out = user.run("tools-delete-refused-pinned", &args).await;
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(err.trim().lines().count(), 1, "{err}");
    assert!(err.contains("kit"), "{err}");
    assert!(user.client.entities.get(tool.id).await.is_ok());
    // once the agent is deleted the toolset deletes
    user.client.entities.delete(caller).await?;
    let args = ["toolsets", "delete", "kit", "--yes"];
    let out = user.run("toolsets-delete-unreferenced", &args).await;
    assert_success(&out);
    assert!(user.client.entities.get(kit).await.is_err());
    Ok(())
}

/// A tool that misses one of the toolset's groups is refused, and `--extend-target-groups` adds
/// the group to the tool instead
#[tokio::test]
async fn toolsets_update_extends_target_groups() -> Result<(), Error> {
    // a user named after selene in a group named after luna, and a second group
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "toolsets-update-extend",
        &generators::readable_name("selene", &suffix),
        &generators::readable_name("luna", &suffix),
    )
    .await?;
    let other = extra_group(&user, &generators::readable_name("terra", &suffix)).await?;
    // a toolset in both groups and a tool in only one of them
    let args = [
        "toolsets",
        "create",
        "wide",
        "-g",
        &user.group,
        "-g",
        &other,
    ];
    assert_success(&user.run("toolsets-create-wide", &args).await);
    let id = toolset_id(&user, "wide").await?;
    let (tool, _) = agentic_tool(&user).await?;
    // the tool is refused with the API's reason
    let tools = format!("tools=[{{tool: {}}}]", tool.name);
    let args = ["toolsets", "update", "wide", "--set", &tools];
    let out = user.run("toolsets-update-missing-group", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("isn't in groups"));
    assert_eq!(
        out_edges(&user, id, AssociationKind::Includes).await?,
        Vec::<Uuid>::new()
    );
    // the flag widens the tool's groups and includes it
    let args = [
        "toolsets",
        "update",
        "wide",
        "--set",
        &tools,
        "--extend-target-groups",
    ];
    assert_success(&user.run("toolsets-update-extend", &args).await);
    assert_eq!(
        out_edges(&user, id, AssociationKind::Includes).await?,
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
