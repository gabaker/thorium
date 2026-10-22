//! Integration tests for harness capabilities, native tools and the `Provides` edges a harness
//! publish creates, and the harness env and disabled models on a harness tool's root

use std::collections::BTreeSet;

use serde_json::json;
use thorium::models::entities::digests::ThorDigest;
use thorium::models::{
    AgentEntityRequest, AgenticToolBacking, AssociationKind, AssociationRequest, AssociationTarget,
    Directionality, Entity, EntityMetadataRequest, EntityMetadataUpdate, EntityRequest,
    EntityUpdate, HarnessEnvVar, HarnessToolAccess, McpConfigFormat, McpConfigTarget,
    ToolEntityRequest, ToolInstanceEntityRequest, ToolMechanism, ToolSpec, UserRole,
    WorkflowGraphChange,
};
use thorium::test_utilities::generators::{self, GroupRoles, god_group, moon_user};
use thorium::test_utilities::{self, events};
use thorium::{Error, Thorium, fail, is};
use uuid::Uuid;

use super::{instance_meta, listed, publish, tool_changes, tool_meta, tool_updates};

/// Get a refusal's status code and message
///
/// # Arguments
///
/// * `result` - The refused request
fn refusal<T>(result: Result<T, Error>) -> (u16, String) {
    // the status and message of the error
    let Err(err) = result else {
        panic!("expected a refusal");
    };
    let status = err.status().map_or(0, |status| status.as_u16());
    (status, err.msg().unwrap_or_default())
}

/// Create a harness tool whose first instance provides some native tools
///
/// # Arguments
///
/// * `client` - The client to create it with
/// * `groups` - The harness's groups
/// * `spec` - The harness's spec
/// * `provides` - The native tool instances it provides
async fn providing_harness(
    client: &Thorium,
    groups: &[&str],
    spec: &ToolSpec,
    provides: Vec<Uuid>,
) -> Result<(Entity, Entity), Error> {
    // send the spec with its native tools
    let meta = ToolEntityRequest::new("1.0.0", spec)?.provides(provides);
    let req = EntityRequest::new(
        format!("harness{}", generators::gen_string(10).to_lowercase()),
        EntityMetadataRequest::Tool(meta),
        groups.to_vec(),
    );
    let resp = client.entities.create(req).await?;
    // read back the tool and its first instance
    let tool = client.entities.get(resp.id).await?;
    let instance = client.entities.get(resp.tool_instance.unwrap()).await?;
    Ok((tool, instance))
}

/// Publish a new harness version providing some native tools through a tool update
///
/// # Arguments
///
/// * `client` - The client to publish with
/// * `tool` - The harness tool
/// * `version` - The new version label
/// * `spec` - The new version's spec
/// * `provides` - The native tool instances it provides
async fn publish_providing(
    client: &Thorium,
    tool: Uuid,
    version: &str,
    spec: &ToolSpec,
    provides: Vec<Uuid>,
) -> Result<(), Error> {
    // a tool update with a version, a spec, and the native tools
    let update = EntityUpdate::default().metadata(EntityMetadataUpdate::Tool {
        version: Some(version.to_owned()),
        spec: Some(serde_json::to_string(spec)?),
        provides,
        harness_env: None,
        clear_harness_env: None,
        disable_models: BTreeSet::new(),
        enable_models: BTreeSet::new(),
    });
    client.entities.update(tool, update).await?;
    Ok(())
}

/// Build a tool update that only changes a harness's root settings
///
/// # Arguments
///
/// * `env` - The new JSON encoded env, if any
/// * `clear` - Whether to clear the env
/// * `disable` - The model ids to disable
/// * `enable` - The model ids to enable
fn root_update(
    env: Option<String>,
    clear: bool,
    disable: &[&str],
    enable: &[&str],
) -> EntityUpdate {
    // no version or spec, so nothing is published
    EntityUpdate::default().metadata(EntityMetadataUpdate::Tool {
        version: None,
        spec: None,
        provides: Vec::new(),
        harness_env: env,
        clear_harness_env: clear.then_some(true),
        disable_models: disable.iter().map(|id| (*id).to_owned()).collect(),
        enable_models: enable.iter().map(|id| (*id).to_owned()).collect(),
    })
}

/// Get the targets of an entity's outgoing `Provides` edges, read with the admin route
///
/// # Arguments
///
/// * `admin` - The admin client
/// * `id` - The harness instance
async fn provided(admin: &Thorium, id: Uuid) -> Result<Vec<Uuid>, Error> {
    // keep the outgoing Provides edges
    let mut found = admin
        .entities
        .associations(id)
        .await?
        .into_iter()
        .filter(|assoc| {
            assoc.kind == AssociationKind::Provides && assoc.direction == Directionality::To
        })
        .filter_map(|assoc| match assoc.other {
            AssociationTarget::Entity { id, .. } => Some(id),
            _ => None,
        })
        .collect::<Vec<Uuid>>();
    found.sort();
    found.dedup();
    Ok(found)
}

/// Get the edge changes queued that name an entity
///
/// # Arguments
///
/// * `admin` - The admin client to peek with
/// * `id` - The entity the changes must name
async fn edge_changes(admin: &Thorium, id: Uuid) -> Result<usize, Error> {
    // count the queued edge changes naming the entity
    let queued = events::graph_changes(admin, id).await?;
    Ok(queued
        .iter()
        .filter_map(events::graph_event)
        .filter(|graph| matches!(graph.change, WorkflowGraphChange::EdgeChanged { .. }))
        .count())
}

/// A harness republished with an empty tool access block is the same spec as one without it, so
/// the publish is a no-op
#[tokio::test]
async fn harness_default_tool_access_is_unset() -> Result<(), Error> {
    // amalthea publishes a harness without tool access in ceres
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let amalthea = moon_user(&admin, "amalthea", &suffix).await?;
    let roles = GroupRoles::default().user(&amalthea.username);
    let ceres = god_group(&admin, "ceres", &suffix, &roles).await?;
    let spec = generators::gen_harness_spec();
    let (tool, first) = generators::tool(&[&ceres], "1.0.0", &spec, &amalthea.client).await?;
    // the same harness with a default tool access block is refused as identical
    let ToolSpec::Harness(mut defaulted) = spec.clone() else {
        unreachable!("a harness spec is always a Harness tool spec")
    };
    defaulted.interface.tool_access = Some(HarnessToolAccess::default());
    let resp = publish(
        &amalthea.client,
        tool.id,
        "1.1.0",
        &ToolSpec::Harness(defaulted),
    )
    .await;
    fail!(resp, 409);
    let msg = resp.unwrap_err().msg().unwrap();
    assert!(msg.contains("identical"), "{msg}");
    is!(listed(&admin, tool.id).await?, vec![first.id]);
    Ok(())
}

/// Publishing a harness with native tools creates one frozen `Provides` edge per tool that its
/// spec hash covers, refuses targets that can't be provided, and queues the harness's tool update
/// but no edge change
#[tokio::test]
// one scenario walks every provides rule against shared fixtures
#[allow(clippy::too_many_lines)]
async fn harness_provides_native_tools() -> Result<(), Error> {
    // europa edits in minerva and mars; two native tools live in both and a third only in minerva
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let europa = moon_user(&admin, "europa", &suffix).await?;
    let roles = GroupRoles::default().user(&europa.username);
    let minerva = god_group(&admin, "minerva", &suffix, &roles).await?;
    let mars = god_group(&admin, "mars", &suffix, &roles).await?;
    let both = [minerva.as_str(), mars.as_str()];
    let native_spec = generators::gen_native_spec();
    let (_, decompile) = generators::tool(&both, "1.0.0", &native_spec, &europa.client).await?;
    let (_, strings) = generators::tool(
        &both,
        "1.0.0",
        &generators::gen_native_spec(),
        &europa.client,
    )
    .await?;
    let (_, narrow) = generators::tool(
        &[&minerva],
        "1.0.0",
        &generators::gen_native_spec(),
        &europa.client,
    )
    .await?;
    // a native tool publishes without a timeout and isn't bound to any harness yet
    assert!(matches!(
        &instance_meta(&decompile).spec,
        ToolSpec::Agentic(agentic)
            if matches!(agentic.backing, AgenticToolBacking::HarnessNative { .. })
                && agentic.timeout.is_none()
    ));
    // a harness providing the first native tool gets one Provides edge to it
    let spec = generators::gen_harness_spec();
    let (harness, first) =
        providing_harness(&europa.client, &both, &spec, vec![decompile.id]).await?;
    is!(provided(&admin, first.id).await?, vec![decompile.id]);
    is!(instance_meta(&first).provides.clone(), vec![decompile.id]);
    // the instance's spec hash covers the native tool's spec hash
    let native_hash = instance_meta(&decompile).spec_hash.clone();
    is!(
        instance_meta(&first).spec_hash.clone(),
        ThorDigest::provided_tool_spec(&spec, None, &[&native_hash])?
    );
    assert_ne!(
        instance_meta(&first).spec_hash.clone(),
        ThorDigest::tool_spec(&spec, None)?
    );
    // the publish queued the harness's tool update and no edge change
    is!(tool_updates(&admin, harness.id).await?, vec![first.id]);
    is!(edge_changes(&admin, first.id).await?, 0);
    is!(edge_changes(&admin, decompile.id).await?, 0);
    // the same spec with a different native tool list is a new version, not a no-op
    let mut both_native = vec![decompile.id, strings.id];
    both_native.sort();
    publish_providing(
        &europa.client,
        harness.id,
        "1.1.0",
        &spec,
        both_native.clone(),
    )
    .await?;
    let harness = europa.client.entities.get(harness.id).await?;
    let second = tool_meta(&harness).latest.unwrap();
    assert_ne!(second, first.id);
    is!(provided(&admin, second).await?, both_native);
    // the first version's native tools are frozen with it
    is!(provided(&admin, first.id).await?, vec![decompile.id]);
    // the same spec and list as the latest is a no-op
    let resp = publish_providing(&europa.client, harness.id, "1.2.0", &spec, both_native).await;
    fail!(resp, 409);
    // a target that doesn't exist, isn't native, misses a harness group, or is deprecated is
    // refused
    let (_, mcp) = generators::tool(
        &both,
        "1.0.0",
        &generators::gen_agentic_spec(),
        &europa.client,
    )
    .await?;
    let (deprecated_root, deprecated) = generators::tool(
        &both,
        "1.0.0",
        &generators::gen_native_spec(),
        &europa.client,
    )
    .await?;
    publish_new_native(&europa.client, deprecated_root.id).await?;
    let update = EntityUpdate::default().metadata(EntityMetadataUpdate::ToolInstance {
        deprecated: Some(true),
    });
    europa.client.entities.update(deprecated.id, update).await?;
    for (target, fragment) in [
        (Uuid::new_v4(), "doesn't exist".to_owned()),
        (mcp.id, "HarnessNative".to_owned()),
        (narrow.id, mars.clone()),
        (deprecated.id, "deprecated".to_owned()),
    ] {
        let resp = providing_harness(
            &europa.client,
            &both,
            &generators::gen_harness_spec(),
            vec![target],
        )
        .await;
        let (status, msg) = refusal(resp);
        is!(status, 400, msg);
        assert!(msg.contains(&fragment), "{msg}");
        let resp = publish_providing(
            &europa.client,
            harness.id,
            "9.0.0",
            &generators::gen_harness_spec(),
            vec![target],
        )
        .await;
        fail!(resp, 400);
    }
    // a non-Harness tool can't provide anything, on create or on a direct instance create
    let meta = ToolEntityRequest::new("1.0.0", &generators::gen_agentic_spec())?
        .provides(vec![decompile.id]);
    let req = EntityRequest::new(
        "not-a-harness",
        EntityMetadataRequest::Tool(meta),
        both.to_vec(),
    );
    let (status, msg) = refusal(europa.client.entities.create(req).await);
    is!(status, 400);
    assert!(msg.contains("Harness"), "{msg}");
    let meta = ToolInstanceEntityRequest::new(
        mcp_root_of(&mcp),
        "2.0.0",
        &generators::gen_agentic_spec(),
    )?
    .provides(vec![decompile.id]);
    let req = EntityRequest::new(
        "ignored",
        EntityMetadataRequest::ToolInstance(meta),
        both.to_vec(),
    );
    let resp = europa.client.entities.create(req).await;
    fail!(resp, 400);
    // a direct harness instance create provides its native tools too
    let meta =
        ToolInstanceEntityRequest::new(harness.id, "3.0.0", &generators::gen_harness_spec())?
            .provides(vec![strings.id]);
    let req = EntityRequest::new(
        "ignored",
        EntityMetadataRequest::ToolInstance(meta),
        both.to_vec(),
    );
    let direct = europa.client.entities.create(req).await?;
    is!(provided(&admin, direct.id).await?, vec![strings.id]);
    // a harness declaring Native is refused, and the same spec without it publishes
    let ToolSpec::Harness(mut declared) = generators::gen_harness_spec() else {
        unreachable!()
    };
    declared
        .capabilities
        .mechanisms
        .insert(ToolMechanism::Native);
    let resp = providing_harness(
        &europa.client,
        &both,
        &ToolSpec::Harness(declared.clone()),
        vec![decompile.id],
    )
    .await;
    let (status, msg) = refusal(resp);
    is!(status, 400);
    assert!(msg.contains("Native"), "{msg}");
    declared
        .capabilities
        .mechanisms
        .remove(&ToolMechanism::Native);
    providing_harness(
        &europa.client,
        &both,
        &ToolSpec::Harness(declared),
        vec![decompile.id],
    )
    .await?;
    Ok(())
}

/// Get the tool a tool instance belongs to
///
/// # Arguments
///
/// * `instance` - The tool instance
fn mcp_root_of(instance: &Entity) -> Uuid {
    // a tool instance names its tool
    instance_meta(instance).tool
}

/// Publish a fresh native spec as a new version of a native tool, so its first version can be
/// deprecated
///
/// # Arguments
///
/// * `client` - The client to publish with
/// * `tool` - The native tool
async fn publish_new_native(client: &Thorium, tool: Uuid) -> Result<(), Error> {
    // publish a second version with a new spec
    client
        .entities
        .update(
            tool,
            generators::gen_tool_publish("2.0.0", &generators::gen_native_spec()),
        )
        .await?;
    Ok(())
}

/// A native backing needs no timeout, a tool name, and JSON config, and its config is hashed as
/// written
#[tokio::test]
async fn native_backing_publish_rules() -> Result<(), Error> {
    // ganymede edits in vesta
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let ganymede = moon_user(&admin, "ganymede", &suffix).await?;
    let roles = GroupRoles::default().user(&ganymede.username);
    let vesta = god_group(&admin, "vesta", &suffix, &roles).await?;
    // a native tool with compact config publishes without a timeout
    let ToolSpec::Agentic(mut tool) = generators::gen_native_spec() else {
        unreachable!()
    };
    let name = match &tool.backing {
        AgenticToolBacking::HarnessNative { tool, .. } => tool.clone(),
        _ => unreachable!(),
    };
    tool.backing = AgenticToolBacking::HarnessNative {
        tool: name.clone(),
        config: Some(r#"{"a":1}"#.to_owned()),
    };
    let (root, _) = generators::tool(
        &[&vesta],
        "1.0.0",
        &ToolSpec::Agentic(tool.clone()),
        &ganymede.client,
    )
    .await?;
    // the same config with spaces is a new version, not a no-op
    tool.backing = AgenticToolBacking::HarnessNative {
        tool: name.clone(),
        config: Some(r#"{ "a": 1 }"#.to_owned()),
    };
    ganymede
        .client
        .entities
        .update(
            root.id,
            generators::gen_tool_publish("1.0.1", &ToolSpec::Agentic(tool.clone())),
        )
        .await?;
    is!(listed(&ganymede.client, root.id).await?.len(), 2);
    // an empty tool name and config that isn't JSON are refused
    for backing in [
        AgenticToolBacking::HarnessNative {
            tool: String::new(),
            config: None,
        },
        AgenticToolBacking::HarnessNative {
            tool: name,
            config: Some("{not json".to_owned()),
        },
    ] {
        tool.backing = backing;
        let resp = ganymede
            .client
            .entities
            .create(generators::gen_tool(
                &[&vesta],
                "1.0.0",
                &ToolSpec::Agentic(tool.clone()),
            ))
            .await;
        fail!(resp, 400);
    }
    Ok(())
}

/// A harness MCP config file must be absolute and can't replace one of the harness's files
#[tokio::test]
async fn harness_mcp_config_publish() -> Result<(), Error> {
    // callisto edits in juno
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let callisto = moon_user(&admin, "callisto", &suffix).await?;
    let roles = GroupRoles::default().user(&callisto.username);
    let juno = god_group(&admin, "juno", &suffix, &roles).await?;
    // a relative path and the tools file's path are refused
    let ToolSpec::Harness(mut harness) = generators::gen_harness_spec() else {
        unreachable!()
    };
    let tools_path = harness.interface.inputs.tools.clone();
    for path in ["work/.mcp.json".to_owned(), tools_path] {
        harness.interface.mcp_config = Some(McpConfigTarget {
            format: McpConfigFormat::ClaudeCode,
            path,
        });
        let spec = ToolSpec::Harness(harness.clone());
        let resp = callisto
            .client
            .entities
            .create(generators::gen_tool(&[&juno], "1.0.0", &spec))
            .await;
        fail!(resp, 400);
    }
    // an absolute path of its own publishes and is returned on get
    harness.interface.mcp_config = Some(McpConfigTarget {
        format: McpConfigFormat::ClaudeCode,
        path: "/work/.mcp.json".to_owned(),
    });
    let spec = ToolSpec::Harness(harness);
    let (_, instance) = generators::tool(&[&juno], "1.0.0", &spec, &callisto.client).await?;
    is!(instance_meta(&instance).spec.clone(), spec);
    Ok(())
}

/// A harness's models file and Thorium keys file can't be in a directory results are uploaded from
#[tokio::test]
async fn harness_secret_inputs_publish() -> Result<(), Error> {
    // europa edits in juno
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let europa = moon_user(&admin, "europa", &suffix).await?;
    let roles = GroupRoles::default().user(&europa.username);
    let juno = god_group(&admin, "juno", &suffix, &roles).await?;
    // a models file or keys file in the result files directory or under the answer is refused
    let ToolSpec::Harness(mut harness) = generators::gen_harness_spec() else {
        unreachable!()
    };
    let models = harness.interface.inputs.models.clone();
    let keys = harness.interface.inputs.thorium_keys.clone();
    for path in [
        "/tmp/thorium/result-files/secrets.json",
        "/tmp/thorium/results/secrets.json",
    ] {
        // each file is refused at the bad path, naming the path
        for field in ["models", "thorium_keys"] {
            harness.interface.inputs.models.clone_from(&models);
            harness.interface.inputs.thorium_keys.clone_from(&keys);
            if field == "models" {
                harness.interface.inputs.models = path.to_owned();
            } else {
                harness.interface.inputs.thorium_keys = path.to_owned();
            }
            let spec = ToolSpec::Harness(harness.clone());
            let (status, msg) = refusal(
                europa
                    .client
                    .entities
                    .create(generators::gen_tool(&[&juno], "1.0.0", &spec))
                    .await,
            );
            is!(status, 400);
            assert!(msg.contains(path), "{msg}");
        }
    }
    // the default layout publishes
    harness.interface.inputs.models = models;
    harness.interface.inputs.thorium_keys = keys;
    let spec = ToolSpec::Harness(harness);
    generators::tool(&[&juno], "1.0.0", &spec, &europa.client).await?;
    Ok(())
}

/// A harness's env is saved on its root without a new instance or event, checked against the
/// names a pod already uses, and refused on other tools; its disabled models are added and
/// removed the same way and returned sorted
#[tokio::test]
// one scenario walks every root setting rule against shared fixtures
#[allow(clippy::too_many_lines)]
async fn harness_root_settings() -> Result<(), Error> {
    // io edits in minerva, which holds a harness and an agentic tool
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let roles = GroupRoles::default().user(&io.username);
    let minerva = god_group(&admin, "minerva", &suffix, &roles).await?;
    let (harness, _) = generators::tool(
        &[&minerva],
        "1.0.0",
        &generators::gen_harness_spec(),
        &io.client,
    )
    .await?;
    let (agentic, _) = generators::tool(
        &[&minerva],
        "1.0.0",
        &generators::gen_agentic_spec(),
        &io.client,
    )
    .await?;
    let instances = listed(&admin, harness.id).await?;
    let queued = tool_changes(&admin, harness.id).await?.len();
    // a renamed key is saved on the root and returned on get
    let env = json!({"export": ["OpenAiApiKey", "ModelId"], "names": {"OpenAiApiKey": "LITELLM_API_KEY"}});
    io.client
        .entities
        .update(
            harness.id,
            root_update(Some(env.to_string()), false, &[], &[]),
        )
        .await?;
    let read = io.client.entities.get(harness.id).await?;
    let saved = tool_meta(&read).harness_env.clone().unwrap();
    is!(
        saved.name_of(HarnessEnvVar::OpenAiApiKey),
        "LITELLM_API_KEY"
    );
    assert!(saved.export.contains(&HarnessEnvVar::ModelId));
    // renames onto names a pod already uses or another exported value's name are refused
    let refused = [
        json!({"export": ["OpenAiApiKey"], "names": {"OpenAiApiKey": "THORIUM_URL"}}),
        json!({"export": ["OpenAiApiKey"], "names": {"OpenAiApiKey": "THORIUM_TOKEN"}}),
        json!({"export": ["OpenAiApiKey"], "names": {"OpenAiApiKey": "THORIUM_MODELS"}}),
        json!({"export": ["OpenAiApiKey"], "names": {"OpenAiApiKey": "THORIUM_KEYS__API"}}),
        json!({"export": ["OpenAiApiKey"], "names": {"OpenAiApiKey": "PATH"}}),
        json!({"export": ["OpenAiApiKey"], "names": {"OpenAiApiKey": "HOME"}}),
        json!({"export": ["OpenAiApiKey"], "names": {"OpenAiApiKey": "LD_PRELOAD"}}),
        json!({"export": ["OpenAiApiKey"], "names": {"OpenAiApiKey": "https_proxy"}}),
        json!({"export": ["OpenAiApiKey", "AnthropicApiKey"], "names": {"OpenAiApiKey": "ANTHROPIC_API_KEY"}}),
        json!({"export": ["OpenAiApiKey", "GeminiApiKey"], "names": {"OpenAiApiKey": "KEY", "GeminiApiKey": "KEY"}}),
        json!({"export": ["SlackToken"]}),
    ];
    for env in refused {
        let resp = io
            .client
            .entities
            .update(
                harness.id,
                root_update(Some(env.to_string()), false, &[], &[]),
            )
            .await;
        fail!(resp, 400);
    }
    let resp = io
        .client
        .entities
        .update(
            harness.id,
            root_update(Some("{not json".into()), false, &[], &[]),
        )
        .await;
    fail!(resp, 400);
    // the refusals left the saved env alone
    let read = io.client.entities.get(harness.id).await?;
    assert_eq!(tool_meta(&read).harness_env, Some(saved));
    // disabling models adds them, sorted, and repeating or enabling an unknown id changes nothing
    io.client
        .entities
        .update(
            harness.id,
            root_update(None, false, &["zeta-1", "alpha-2"], &[]),
        )
        .await?;
    io.client
        .entities
        .update(
            harness.id,
            root_update(None, false, &["zeta-1"], &["never-disabled"]),
        )
        .await?;
    let read = io.client.entities.get(harness.id).await?;
    is!(
        tool_meta(&read)
            .disabled_models
            .iter()
            .cloned()
            .collect::<Vec<String>>(),
        vec!["alpha-2".to_owned(), "zeta-1".to_owned()]
    );
    // enabling a model removes it, and clearing the env removes it
    io.client
        .entities
        .update(harness.id, root_update(None, true, &[], &["zeta-1"]))
        .await?;
    let read = io.client.entities.get(harness.id).await?;
    assert!(tool_meta(&read).harness_env.is_none());
    is!(
        tool_meta(&read)
            .disabled_models
            .iter()
            .cloned()
            .collect::<Vec<String>>(),
        vec!["alpha-2".to_owned()]
    );
    // none of these edits created an instance or queued a change
    is!(listed(&admin, harness.id).await?, instances);
    is!(tool_changes(&admin, harness.id).await?.len(), queued);
    // the env and disabled models are refused on a tool that isn't a harness
    let env = json!({"export": ["ModelId"]}).to_string();
    for update in [
        root_update(Some(env.clone()), false, &[], &[]),
        root_update(None, false, &["alpha-2"], &[]),
    ] {
        let (status, msg) = refusal(io.client.entities.update(agentic.id, update).await);
        is!(status, 400);
        assert!(msg.contains("Harness"), "{msg}");
    }
    // native tools without a new version and spec are refused
    let update = EntityUpdate::default().metadata(EntityMetadataUpdate::Tool {
        version: None,
        spec: None,
        provides: vec![Uuid::new_v4()],
        harness_env: None,
        clear_harness_env: None,
        disable_models: BTreeSet::new(),
        enable_models: BTreeSet::new(),
    });
    let resp = io.client.entities.update(harness.id, update).await;
    fail!(resp, 400);
    // a harness created with an env and disabled models keeps them, and another kind can't be
    let meta = ToolEntityRequest::new("1.0.0", &generators::gen_harness_spec())?
        .harness_env(&serde_json::from_str(&env)?)?
        .disabled_models(["beta-3"]);
    let req = EntityRequest::new(
        "with-env",
        EntityMetadataRequest::Tool(meta),
        vec![minerva.clone()],
    );
    let created = io.client.entities.create(req).await?;
    let read = io.client.entities.get(created.id).await?;
    assert!(tool_meta(&read).harness_env.is_some());
    is!(tool_meta(&read).disabled_models.len(), 1);
    let meta = ToolEntityRequest::new("1.0.0", &generators::gen_agentic_spec())?
        .disabled_models(["beta-3"]);
    let req = EntityRequest::new("no-env", EntityMetadataRequest::Tool(meta), vec![minerva]);
    let resp = io.client.entities.create(req).await;
    fail!(resp, 400);
    Ok(())
}

/// `Provides` is managed by publishing a harness: the generic association routes refuse to add
/// or delete it for an admin and an owner alike, and an agent may call a native tool with or
/// without a harness
#[tokio::test]
async fn provides_frozen_on_association_routes() -> Result<(), Error> {
    // io owns minerva, which holds a native tool and a harness that provides it
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let roles = GroupRoles::default().owner(&io.username);
    let minerva = god_group(&admin, "minerva", &suffix, &roles).await?;
    let (native_root, native) = generators::tool(
        &[&minerva],
        "1.0.0",
        &generators::gen_native_spec(),
        &io.client,
    )
    .await?;
    let (_, other_native) = generators::tool(
        &[&minerva],
        "1.0.0",
        &generators::gen_native_spec(),
        &io.client,
    )
    .await?;
    let (_, harness) = providing_harness(
        &io.client,
        &[&minerva],
        &generators::gen_harness_spec(),
        vec![native.id],
    )
    .await?;
    // a POST of a Provides association is refused for the admin and the owner, naming the kind
    let target = |entity: &Entity| AssociationTarget::Entity {
        id: entity.id,
        name: entity.name.clone(),
    };
    let req = AssociationRequest::new(AssociationKind::Provides, target(&harness))
        .target(target(&other_native));
    for client in [&admin, &io.client] {
        let (status, msg) = refusal(client.associations.create(&req).await);
        is!(status, 400);
        assert!(msg.contains("Provides"), "{msg}");
    }
    // a DELETE on the association routes is refused too
    let sender = generators::user_with_role(&admin, UserRole::Admin).await?;
    let resp = reqwest::Client::new()
        .delete(format!("{}/api/associations/", admin.host))
        .header("authorization", sender.auth_header())
        .json(&req)
        .send()
        .await?;
    assert!(!resp.status().is_success(), "{}", resp.status());
    // the harness's frozen Provides list is unchanged
    is!(provided(&admin, harness.id).await?, vec![native.id]);
    // an agent calls the native tool with or without a harness edge
    let req = generators::gen_agent(
        "loner",
        &[&minerva],
        AgentEntityRequest::new(generators::gen_agent_config("loner")).tool(native_root.id),
    );
    io.client.entities.create(req).await?;
    let req = generators::gen_agent(
        "harnessed",
        &[&minerva],
        AgentEntityRequest::new(generators::gen_agent_config("harnessed"))
            .tool(native.id)
            .harness(harness.id),
    );
    io.client.entities.create(req).await?;
    Ok(())
}
