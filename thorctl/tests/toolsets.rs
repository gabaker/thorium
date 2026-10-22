//! Tests the thorctl toolsets commands and `agents link-toolset` against the test API

mod common;

use common::{TestUser, assert_success, json, stdout};
use serde_json::Value;
use thorium::Error;
use thorium::models::{
    AssociationKind, AssociationTarget, Directionality, Entity, EntityKinds, EntityListOpts,
    ToolSetSelector,
};
use thorium::test_utilities::generators;
use uuid::Uuid;

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
