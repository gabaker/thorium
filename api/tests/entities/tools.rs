//! Integration tests for tools and tool instances
//!
//! These cover publishing tools and their versions, the latest and no-op rules, and the group
//! rules shared definitions follow: instances carry their tool's groups, edits and deletes act on
//! every group, and a referenced group can't be removed.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use reqwest::multipart::Form;
use serde_json::json;
use thorium::models::entities::digests::ThorDigest;
use thorium::models::{
    AgenticToolBacking, AssociationKind, AssociationRequest, AssociationTarget,
    ContainerToolBacking, Entity, EntityListOpts, EntityMetadata, EntityMetadataUpdate,
    EntityRequest, EntityUpdate, GroupAllowed, GroupAllowedUpdate, GroupUpdate,
    HostPathWhitelistUpdate, NetworkPolicyRequest, ScopedTokenRequest, SecurityContext,
    SystemSettingsUpdate, SystemSettingsUpdateParams, ToolEntity, ToolInstanceEntity,
    ToolInstanceEntityRequest, ToolKind, ToolSpec, ToolVersion, UserRole, Volume, VolumeTypes,
    WorkflowGraphChange, WorkloadEnvironment,
};
use thorium::test_utilities::generators::{GroupRoles, TestUser};
use thorium::test_utilities::{self, events, generators};
use thorium::{Error, Thorium, fail, is};
use uuid::Uuid;

mod harness_tools;

/// Get the queued workflow graph changes naming a tool, oldest first
///
/// # Arguments
///
/// * `admin` - The admin client to peek with
/// * `tool` - The tool the changes must name
async fn tool_changes(admin: &Thorium, tool: Uuid) -> Result<Vec<WorkflowGraphChange>, Error> {
    // read the queued workflow graph events naming the tool
    let queued = events::graph_changes(admin, tool).await?;
    Ok(queued
        .iter()
        .filter_map(events::graph_event)
        .map(|graph| graph.change.clone())
        .collect())
}

/// Get the new latest instance of every `ToolUpdated` change queued for a tool, oldest first
///
/// # Arguments
///
/// * `admin` - The admin client to peek with
/// * `tool` - The tool whose changes to read
async fn tool_updates(admin: &Thorium, tool: Uuid) -> Result<Vec<Uuid>, Error> {
    // keep the latest instance each tool update names
    Ok(tool_changes(admin, tool)
        .await?
        .into_iter()
        .filter_map(|change| match change {
            WorkflowGraphChange::ToolUpdated {
                tool: changed,
                tool_instance,
            } if changed == tool => Some(tool_instance),
            _ => None,
        })
        .collect())
}

/// Get a tool's metadata from an entity
///
/// # Arguments
///
/// * `entity` - The tool entity
fn tool_meta(entity: &Entity) -> &ToolEntity {
    // only tools have tool metadata
    match &entity.metadata {
        EntityMetadata::Tool(tool) => tool,
        other => panic!("expected a tool, got {other:?}"),
    }
}

/// Get a tool instance's metadata from an entity
///
/// # Arguments
///
/// * `entity` - The tool instance entity
fn instance_meta(entity: &Entity) -> &ToolInstanceEntity {
    // only tool instances have tool instance metadata
    match &entity.metadata {
        EntityMetadata::ToolInstance(instance) => instance,
        other => panic!("expected a tool instance, got {other:?}"),
    }
}

/// Get the values of one tag on an entity
///
/// # Arguments
///
/// * `entity` - The entity to read the tag from
/// * `key` - The tag's key
fn tag_values(entity: &Entity, key: &str) -> Vec<String> {
    // collect the values under this key, if there are any
    entity
        .tags
        .get(key)
        .map_or_else(Vec::new, |values| values.keys().cloned().collect())
}

/// Sort a list of groups so group lists can be compared
///
/// # Arguments
///
/// * `groups` - The groups to sort
fn sorted(groups: &[String]) -> Vec<String> {
    // copy and sort the groups
    let mut groups = groups.to_vec();
    groups.sort();
    groups
}

/// Create a plain `User` role user
///
/// # Arguments
///
/// * `admin` - The admin client to create the user with
async fn plain_user(admin: &Thorium) -> Result<TestUser, Error> {
    // create a user without a developer role
    generators::user_with_role(admin, UserRole::User).await
}

/// Create a group where a user holds the user role
///
/// # Arguments
///
/// * `user` - The user to add
/// * `admin` - The admin client to create the group with
async fn group_for(user: &str, admin: &Thorium) -> Result<String, Error> {
    // create a group that allows everything with the user as a user
    generators::group_with_roles(
        &GroupRoles::default().user(user),
        GroupAllowed::default(),
        admin,
    )
    .await
}

/// Publish a new version of a tool through a tool update
///
/// # Arguments
///
/// * `client` - The client to publish with
/// * `tool` - The tool to publish a version of
/// * `version` - The version label
/// * `spec` - The spec to publish
async fn publish(
    client: &Thorium,
    tool: Uuid,
    version: &str,
    spec: &ToolSpec,
) -> Result<(), Error> {
    // send the version and spec as a tool update
    client
        .entities
        .update(tool, generators::gen_tool_publish(version, spec))
        .await?;
    Ok(())
}

/// Create a tool instance directly for a tool
///
/// # Arguments
///
/// * `client` - The client to create the instance with
/// * `tool` - The tool the instance belongs to
/// * `groups` - The groups sent with the request
/// * `version` - The version label
/// * `spec` - The spec to publish
async fn direct_instance(
    client: &Thorium,
    tool: Uuid,
    groups: &[String],
    version: &str,
    spec: &ToolSpec,
) -> Result<Uuid, Error> {
    // build the direct instance request
    let meta = ToolInstanceEntityRequest::new(tool, version, spec)?;
    let req = EntityRequest::new(
        "ignored",
        thorium::models::EntityMetadataRequest::ToolInstance(meta),
        groups.to_vec(),
    );
    // create it and return its id
    Ok(client.entities.create(req).await?.id)
}

/// Make an agentic spec unique by changing its description
///
/// # Arguments
///
/// * `spec` - The spec to change
/// * `marker` - The text to put in the description
fn with_marker(spec: &ToolSpec, marker: &str) -> ToolSpec {
    // change a field each kind of spec has
    let mut spec = spec.clone();
    match &mut spec {
        ToolSpec::Agentic(agentic) => marker.clone_into(&mut agentic.model_description),
        ToolSpec::Image(workload) => workload.execution.timeout = Some(marker.len() as u64),
        ToolSpec::Harness(harness) => {
            harness.workload.execution.timeout = Some(marker.len() as u64);
        }
    }
    spec
}

/// Get the ids of the instances `GET /instances` lists for a tool
///
/// # Arguments
///
/// * `client` - The client to list with
/// * `tool` - The tool to list instances of
async fn listed(client: &Thorium, tool: Uuid) -> Result<Vec<Uuid>, Error> {
    // list the instances and keep their ids
    Ok(client
        .entities
        .tool_instances(tool)
        .await?
        .into_iter()
        .map(|summary| summary.id)
        .collect())
}

/// Get the groups an entity's rows are stored in
///
/// # Arguments
///
/// * `id` - The entity to inspect
async fn stored_groups(id: Uuid) -> Result<Vec<String>, Error> {
    // read the entity's rows and keep each group once
    let mut groups = test_utilities::entity_rows(id)
        .await?
        .into_iter()
        .map(|(group, _)| group)
        .collect::<Vec<String>>();
    groups.sort();
    groups.dedup();
    Ok(groups)
}

/// Build an association target for an entity
///
/// # Arguments
///
/// * `entity` - The entity to target
fn target(entity: &Entity) -> AssociationTarget {
    // target the entity by its id and name
    AssociationTarget::Entity {
        id: entity.id,
        name: entity.name.clone(),
    }
}

/// Add or delete an entity's tags as a user, returning the status code
///
/// # Arguments
///
/// * `host` - The API's url
/// * `user` - The user to send the request as
/// * `id` - The entity to change the tags of
/// * `delete` - Whether to delete the tags instead of adding them
/// * `tags` - The tags to add or delete
async fn raw_tags(
    host: &str,
    user: &TestUser,
    id: Uuid,
    delete: bool,
    tags: serde_json::Value,
) -> Result<u16, Error> {
    // build the tag request for this entity
    let url = format!("{host}/api/entities/tags/{id}");
    let client = reqwest::Client::new();
    let req = if delete {
        client.delete(url)
    } else {
        client.post(url)
    };
    // send it as the user
    let resp = req
        .header("authorization", user.auth_header())
        .json(&tags)
        .send()
        .await?;
    Ok(resp.status().as_u16())
}

/// A tool's user tag change is queued only in the groups of the tag request, even when the tool
/// is in others, and a delete that removed no tag queues nothing
#[tokio::test]
async fn tag_changes_queue_in_request_groups() -> Result<(), Error> {
    // a tool in two groups
    let admin = test_utilities::admin_client().await?;
    let groups = generators::groups(2, &admin).await?;
    let (a, b) = (groups[0].name.clone(), groups[1].name.clone());
    let spec = generators::gen_agentic_spec();
    let (tool, _) = generators::tool(&[&a, &b], "1.0.0", &spec, &admin).await?;
    let tagger = generators::user_with_role(&admin, UserRole::Admin).await?;
    let queued = events::graph_changes(&admin, tool.id).await?.len();
    // a pair is added in the first group, a pair the tool doesn't carry is deleted, and the
    // added pair is deleted again
    let tags = json!({"groups": [a.clone()], "tags": {"Team": ["red"]}});
    let status = raw_tags(&admin.host, &tagger, tool.id, false, tags.clone()).await?;
    is!(status, 204);
    let missing = json!({"groups": [a.clone()], "tags": {"Team": ["blue"]}});
    let status = raw_tags(&admin.host, &tagger, tool.id, true, missing).await?;
    is!(status, 204);
    let status = raw_tags(&admin.host, &tagger, tool.id, true, tags).await?;
    is!(status, 204);
    // the add and the real delete each queued one change in only the first group
    let since = events::graph_changes(&admin, tool.id)
        .await?
        .split_off(queued)
        .iter()
        .filter_map(events::graph_event)
        .map(|graph| (graph.groups.clone(), graph.change.clone()))
        .collect::<Vec<(Vec<String>, WorkflowGraphChange)>>();
    let pair = BTreeMap::from([("Team".to_owned(), BTreeSet::from(["red".to_owned()]))]);
    let change = WorkflowGraphChange::ToolTagsChanged {
        tool: tool.id,
        tags: pair,
    };
    let expected = vec![(vec![a.clone()], change.clone()), (vec![a], change)];
    is!(since, expected);
    Ok(())
}

/// Creating an agentic tool creates its root and first instance with latest, tags, and an edge
#[tokio::test]
async fn create_agentic_tool() -> Result<(), Error> {
    // create a tool named yara_scan in two groups
    let admin = test_utilities::admin_client().await?;
    let groups = generators::groups(2, &admin).await?;
    let (a, b) = (groups[0].name.as_str(), groups[1].name.as_str());
    let spec = generators::gen_agentic_spec();
    let mut req = generators::gen_tool(&[a, b], "1.0.0", &spec);
    req.name = "yara_scan".into();
    let resp = admin.entities.create(req).await?;
    // the response holds both ids
    let instance_id = resp.tool_instance.expect("no tool instance id");
    assert_ne!(instance_id, resp.id);
    let tool = admin.entities.get(resp.id).await?;
    let instance = admin.entities.get(instance_id).await?;
    // the instance is named after its tool and version and carries the tool's groups
    is!(instance.name, "yara_scan@1.0.0".to_owned());
    is!(sorted(&instance.groups), sorted(&tool.groups));
    is!(sorted(&tool.groups), sorted(&[a.to_owned(), b.to_owned()]));
    // latest names the instance
    let meta = tool_meta(&tool);
    let inst = instance_meta(&instance);
    is!(meta.kind, ToolKind::Agentic);
    is!(meta.latest, Some(instance_id));
    is!(meta.latest_version, Some(ToolVersion::from("1.0.0")));
    is!(meta.latest_spec_hash.as_ref(), Some(&inst.spec_hash));
    is!(meta.modified, tool.created.max(meta.modified));
    // the spec hash is a v1 hash of the spec
    assert!(inst.spec_hash.starts_with("v1:") && inst.spec_hash.len() == 67);
    is!(inst.spec_hash.clone(), ThorDigest::tool_spec(&spec, None)?);
    // the intrinsic tags are written
    is!(tag_values(&instance, "ToolId"), vec![tool.id.to_string()]);
    is!(
        tag_values(&instance, "ToolVersion"),
        vec!["1.0.0".to_owned()]
    );
    is!(
        tag_values(&instance, "ToolKind"),
        vec!["Agentic".to_owned()]
    );
    is!(
        tag_values(&instance, "ToolSpecHash"),
        vec![inst.spec_hash.clone()]
    );
    is!(
        tag_values(&instance, "ToolFunction"),
        vec![spec.function_name().unwrap().to_owned()]
    );
    // the PublishedFrom edge links the instance to its tool in both groups
    let edges = admin.entities.associations(instance_id).await?;
    let published = edges
        .iter()
        .filter(|edge| edge.kind == AssociationKind::PublishedFrom)
        .collect::<Vec<_>>();
    assert!(!published.is_empty(), "no PublishedFrom edge: {edges:?}");
    assert!(published.iter().all(|edge| edge.other == target(&tool)));
    // the tool lists its one instance
    is!(listed(&admin, tool.id).await?, vec![instance_id]);
    // one tool update is queued by the creator in both of the tool's groups
    let queued = events::graph_changes(&admin, tool.id).await?;
    is!(queued.len(), 1);
    let graph = events::graph_event(&queued[0]).expect("a workflow graph event");
    is!(queued[0].user.clone(), admin.users.info().await?.username);
    is!(sorted(&graph.groups), sorted(&[a.to_owned(), b.to_owned()]));
    is!(
        graph.change.clone(),
        WorkflowGraphChange::ToolUpdated {
            tool: tool.id,
            tool_instance: instance_id
        }
    );
    Ok(())
}

/// Harness tools are published and versioned like any tool but must run in K8s, while Image
/// tools may use any environment; a harness's workload goes through the same publish checks
#[tokio::test]
async fn harness_tools_run_in_k8s() -> Result<(), Error> {
    // a plain user publishes a K8s harness, which keeps its kind and default file layout
    let admin = test_utilities::admin_client().await?;
    let user = plain_user(&admin).await?;
    let group = group_for(&user.username, &admin).await?;
    let spec = generators::gen_harness_spec();
    let (tool, first) = generators::tool(&[&group], "1.0.0", &spec, &user.client).await?;
    is!(tool_meta(&tool).kind, ToolKind::Harness);
    is!(instance_meta(&first).spec.clone(), spec.clone());
    assert!(instance_meta(&first).portability.is_none());
    is!(
        instance_meta(&first).spec_hash.clone(),
        ThorDigest::tool_spec(&spec, None)?
    );
    // a new harness version moves latest, and an Agentic spec can't be published for it
    let ToolSpec::Harness(mut changed) = spec.clone() else {
        unreachable!()
    };
    changed.interface.inputs.models = "/work/models.json".into();
    publish(&user.client, tool.id, "1.1.0", &ToolSpec::Harness(changed)).await?;
    let tool = user.client.entities.get(tool.id).await?;
    is!(
        tool_meta(&tool).latest_version.clone(),
        Some(ToolVersion::from("1.1.0"))
    );
    assert_ne!(tool_meta(&tool).latest, Some(first.id));
    let resp = publish(
        &user.client,
        tool.id,
        "2.0.0",
        &generators::gen_agentic_spec(),
    )
    .await;
    fail!(resp, 400);
    // a harness in any other environment is refused, naming K8s, while an Image tool is accepted
    let ToolSpec::Image(image) = generators::gen_image_spec() else {
        unreachable!()
    };
    let others = [
        json!({"BareMetal": {"scheduling": {"resources": {"cpu": 1, "memory": 1}, "spawn_limit": "Unlimited"}}}),
        json!({"Windows": {"image": "repo/x", "scheduling": {"resources": {"cpu": 1, "memory": 1}, "spawn_limit": "Unlimited"}}}),
        json!("External"),
    ];
    for environment in others {
        let mut workload = serde_json::to_value(&image)?;
        workload["environment"] = environment.clone();
        for (kind, raw_spec, code) in [
            ("Harness", json!({"Harness": {"workload": workload}}), 400),
            ("Image", json!({"Image": workload}), 200),
        ] {
            let form = Form::new()
                .text(
                    "name",
                    format!("hrn{}", generators::gen_string(10).to_lowercase()),
                )
                .text("kind", "Tool")
                .text("groups[]", group.clone())
                .text("metadata[tool_kind]", kind)
                .text("metadata[tool_version]", "1.0.0")
                .text("metadata[tool_spec]", raw_spec.to_string());
            let status = raw_create(&admin, form).await?;
            is!(status, code, raw_spec);
        }
    }
    // a harness workload with a non-default security context needs an admin publisher
    let mut privileged = generators::gen_harness_spec();
    if let ToolSpec::Harness(harness) = &mut privileged
        && let WorkloadEnvironment::K8s(env) = &mut harness.workload.environment
    {
        env.security_context = SecurityContext {
            user: Some(0),
            ..SecurityContext::default()
        };
    }
    let resp = user
        .client
        .entities
        .create(generators::gen_tool(&[&group], "1.0.0", &privileged))
        .await;
    fail!(resp, 400);
    admin
        .entities
        .create(generators::gen_tool(&[&group], "1.0.0", &privileged))
        .await?;
    Ok(())
}

/// Image tools are created by any group member, and a container tool pins one and hashes it
#[tokio::test]
async fn create_image_tool_and_container() -> Result<(), Error> {
    // a plain user that isn't a developer creates an Image tool
    let admin = test_utilities::admin_client().await?;
    let user = plain_user(&admin).await?;
    let group = group_for(&user.username, &admin).await?;
    let image_spec = generators::gen_image_spec();
    let (_, image_instance) =
        generators::tool(&[&group], "1.0.0", &image_spec, &user.client).await?;
    let pinned_hash = instance_meta(&image_instance).spec_hash.clone();
    // an agentic tool with a container backing pins that instance
    let ToolSpec::Agentic(mut agentic) = generators::gen_agentic_spec() else {
        unreachable!()
    };
    agentic.backing = AgenticToolBacking::Container(ContainerToolBacking {
        tool_instance: image_instance.id,
        ..ContainerToolBacking::default()
    });
    let container_spec = ToolSpec::Agentic(agentic);
    let (_, container_instance) =
        generators::tool(&[&group], "1.0.0", &container_spec, &user.client).await?;
    // its spec hash folds in the pinned instance's hash
    is!(
        instance_meta(&container_instance).spec_hash.clone(),
        ThorDigest::tool_spec(&container_spec, Some(&pinned_hash))?
    );
    Ok(())
}

/// Publishing new versions chains `previous`, moves latest, tags each version, and lists them
/// newest first without storing the list
#[tokio::test]
async fn publish_new_versions() -> Result<(), Error> {
    // create a tool and publish two more versions
    let admin = test_utilities::admin_client().await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let spec = generators::gen_agentic_spec();
    let (tool, first) = generators::tool(&[&group], "1.0.0", &spec, &admin).await?;
    publish(&admin, tool.id, "1.1.0", &with_marker(&spec, "v2")).await?;
    publish(&admin, tool.id, "1.2.0", &with_marker(&spec, "v3")).await?;
    // the tool lists three instances newest first
    let summaries = admin.entities.tool_instances(tool.id).await?;
    is!(summaries.len(), 3);
    let versions = summaries
        .iter()
        .map(|summary| summary.version.to_string())
        .collect::<Vec<String>>();
    is!(
        versions,
        vec!["1.2.0".to_owned(), "1.1.0".to_owned(), "1.0.0".to_owned()]
    );
    assert!(
        summaries
            .windows(2)
            .all(|pair| pair[0].created >= pair[1].created)
    );
    // each previous names the version before it
    let third = admin.entities.get(summaries[0].id).await?;
    let second = admin.entities.get(summaries[1].id).await?;
    is!(instance_meta(&third).previous, Some(second.id));
    is!(instance_meta(&second).previous, Some(first.id));
    is!(instance_meta(&first).previous, None::<Uuid>);
    // latest names the newest version
    let tool = admin.entities.get(tool.id).await?;
    let meta = tool_meta(&tool);
    is!(meta.latest, Some(third.id));
    is!(meta.latest_version, Some(ToolVersion::from("1.2.0")));
    is!(
        meta.latest_spec_hash.clone(),
        Some(instance_meta(&third).spec_hash.clone())
    );
    // each version carries its tool and version tags
    for (instance, version) in [(&first, "1.0.0"), (&second, "1.1.0"), (&third, "1.2.0")] {
        is!(tag_values(instance, "ToolId"), vec![tool.id.to_string()]);
        is!(
            tag_values(instance, "ToolVersion"),
            vec![version.to_owned()]
        );
    }
    // each publish queued one tool update naming its new latest instance
    is!(
        tool_updates(&admin, tool.id).await?,
        vec![first.id, second.id, third.id]
    );
    // deprecated instances are still listed
    admin
        .entities
        .update(first.id, generators::gen_tool_deprecate(true))
        .await?;
    let summaries = admin.entities.tool_instances(tool.id).await?;
    is!(summaries.len(), 3);
    assert!(summaries[2].deprecated);
    // deprecating an instance that isn't latest queues nothing
    is!(tool_changes(&admin, tool.id).await?.len(), 3);
    // the get fills the summaries with their versions, hashes, and timestamps
    let tool = admin.entities.get(tool.id).await?;
    is!(tool_meta(&tool).instances.len(), 3);
    is!(
        tool_meta(&tool).instances[0].spec_hash.clone(),
        instance_meta(&third).spec_hash.clone()
    );
    // the stored rows never hold the instance list but do hold latest and modified
    for (_, kind_data) in test_utilities::entity_rows(tool.id).await? {
        let stored: serde_json::Value = serde_json::from_str(&kind_data)?;
        let stored = &stored["Tool"];
        assert!(stored.get("instances").is_none(), "{stored}");
        for key in ["latest", "latest_version", "latest_spec_hash", "modified"] {
            assert!(stored.get(key).is_some(), "{key} missing from {stored}");
        }
    }
    // an unknown tool has no instances to list
    let resp = admin.entities.tool_instances(Uuid::new_v4()).await;
    fail!(resp, 404);
    Ok(())
}

/// A spec of another kind can't be published for a tool
#[tokio::test]
async fn publish_other_kind() -> Result<(), Error> {
    // create an agentic tool
    let admin = test_utilities::admin_client().await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let (tool, _) =
        generators::tool(&[&group], "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    // publishing an Image spec for it is refused and creates nothing
    let resp = publish(&admin, tool.id, "2.0.0", &generators::gen_image_spec()).await;
    fail!(resp, 400);
    is!(listed(&admin, tool.id).await?.len(), 1);
    Ok(())
}

/// A direct tool instance create goes through the same publish path as a tool update
#[tokio::test]
async fn direct_instance_create() -> Result<(), Error> {
    // create a tool and publish a version through an update
    let admin = test_utilities::admin_client().await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let spec = generators::gen_agentic_spec();
    let (tool, first) = generators::tool(&[&group], "1.0.0", &spec, &admin).await?;
    publish(&admin, tool.id, "1.1.0", &with_marker(&spec, "patched")).await?;
    let patched = admin.entities.tool_instances(tool.id).await?[0].id;
    // create the next version directly
    let direct = direct_instance(
        &admin,
        tool.id,
        std::slice::from_ref(&group),
        "1.2.0",
        &with_marker(&spec, "direct"),
    )
    .await?;
    let direct_entity = admin.entities.get(direct).await?;
    let patched_entity = admin.entities.get(patched).await?;
    // both chain previous and carry the same kinds of tags
    is!(instance_meta(&patched_entity).previous, Some(first.id));
    is!(instance_meta(&direct_entity).previous, Some(patched));
    for key in [
        "ToolId",
        "ToolVersion",
        "ToolKind",
        "ToolSpecHash",
        "ToolFunction",
    ] {
        assert!(!tag_values(&direct_entity, key).is_empty(), "{key}");
        assert!(!tag_values(&patched_entity, key).is_empty(), "{key}");
    }
    // the direct instance became latest and has the tool's groups and name
    let tool = admin.entities.get(tool.id).await?;
    is!(tool_meta(&tool).latest, Some(direct));
    is!(direct_entity.name, format!("{}@1.2.0", tool.name));
    is!(sorted(&direct_entity.groups), sorted(&tool.groups));
    // the update and the direct create each queued a tool update like the tool's create
    is!(
        tool_updates(&admin, tool.id).await?,
        vec![first.id, patched, direct]
    );
    // a duplicate version is refused
    let resp = direct_instance(
        &admin,
        tool.id,
        std::slice::from_ref(&group),
        "1.2.0",
        &with_marker(&spec, "again"),
    )
    .await;
    fail!(resp, 409);
    is!(listed(&admin, tool.id).await?.len(), 3);
    // the refused create queued nothing
    is!(tool_changes(&admin, tool.id).await?.len(), 3);
    Ok(())
}

/// A container backing must pin a visible, non-deprecated Image instance in all the tool's groups
#[tokio::test]
async fn container_validation() -> Result<(), Error> {
    // an admin creates groups a and b with our user in both
    let admin = test_utilities::admin_client().await?;
    let user = plain_user(&admin).await?;
    let a = group_for(&user.username, &admin).await?;
    let b = group_for(&user.username, &admin).await?;
    let hidden = generators::groups(1, &admin).await?.remove(0).name;
    // an Image instance in a only, one hidden from the user, and an agentic instance
    let (_, image_a) =
        generators::tool(&[&a], "1.0.0", &generators::gen_image_spec(), &admin).await?;
    let (_, image_hidden) =
        generators::tool(&[&hidden], "1.0.0", &generators::gen_image_spec(), &admin).await?;
    let (_, agentic) =
        generators::tool(&[&a, &b], "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    // build a container spec pinning an instance
    let pin = |instance: Uuid| {
        let ToolSpec::Agentic(mut spec) = generators::gen_agentic_spec() else {
            unreachable!()
        };
        spec.backing = AgenticToolBacking::Container(ContainerToolBacking {
            tool_instance: instance,
            ..ContainerToolBacking::default()
        });
        ToolSpec::Agentic(spec)
    };
    // a missing instance and a hidden one get the same 400
    let missing = user
        .client
        .entities
        .create(generators::gen_tool(&[&a], "1.0.0", &pin(Uuid::new_v4())))
        .await;
    fail!(missing, 400);
    let hidden_resp = user
        .client
        .entities
        .create(generators::gen_tool(&[&a], "1.0.0", &pin(image_hidden.id)))
        .await;
    fail!(hidden_resp, 400);
    let (Err(missing), Err(hidden_err)) = (missing, hidden_resp) else {
        unreachable!()
    };
    is!(
        missing.msg().unwrap().replace(&missing_id(&missing), "ID"),
        hidden_err
            .msg()
            .unwrap()
            .replace(&image_hidden.id.to_string(), "ID")
    );
    // an agentic instance can't be pinned
    let resp = user
        .client
        .entities
        .create(generators::gen_tool(&[&a, &b], "1.0.0", &pin(agentic.id)))
        .await;
    fail!(resp, 400);
    // an instance in a can't be pinned by a tool in a and b, and b is named
    let resp = user
        .client
        .entities
        .create(generators::gen_tool(&[&a, &b], "1.0.0", &pin(image_a.id)))
        .await;
    fail!(resp, 400);
    assert!(resp.unwrap_err().msg().unwrap().contains(&b));
    // a deprecated instance can't be newly pinned
    admin
        .entities
        .update(image_a.id, generators::gen_tool_deprecate(true))
        .await?;
    let resp = user
        .client
        .entities
        .create(generators::gen_tool(&[&a], "1.0.0", &pin(image_a.id)))
        .await;
    fail!(resp, 400);
    // once it's un-deprecated it can be pinned again
    admin
        .entities
        .update(image_a.id, generators::gen_tool_deprecate(false))
        .await?;
    user.client
        .entities
        .create(generators::gen_tool(&[&a], "1.0.0", &pin(image_a.id)))
        .await?;
    Ok(())
}

/// Get the id a "doesn't exist" message names
///
/// # Arguments
///
/// * `error` - The error naming the id
fn missing_id(error: &Error) -> String {
    // the id is the first backticked value in the message
    let msg = error.msg().unwrap_or_default();
    msg.split('`').nth(1).unwrap_or_default().to_owned()
}

/// A direct instance create needs a visible tool, edit rights in all of its groups, and its kind
#[tokio::test]
async fn direct_instance_refusals() -> Result<(), Error> {
    // a tool in a group the user edits and one the user can't see
    let admin = test_utilities::admin_client().await?;
    let user = plain_user(&admin).await?;
    let a = group_for(&user.username, &admin).await?;
    let monitored = generators::group_with_roles(
        &GroupRoles::default().monitor(&user.username),
        GroupAllowed::default(),
        &admin,
    )
    .await?;
    let hidden = generators::groups(1, &admin).await?.remove(0).name;
    let spec = generators::gen_agentic_spec();
    let (hidden_tool, _) = generators::tool(&[&hidden], "1.0.0", &spec, &admin).await?;
    let (shared_tool, _) = generators::tool(&[&a, &monitored], "1.0.0", &spec, &admin).await?;
    let (own_tool, _) = generators::tool(&[&a], "1.0.0", &spec, &admin).await?;
    // an unknown tool and a hidden tool are 404
    let resp = direct_instance(
        &user.client,
        Uuid::new_v4(),
        std::slice::from_ref(&a),
        "2.0.0",
        &spec,
    )
    .await;
    fail!(resp, 404);
    let resp = direct_instance(
        &user.client,
        hidden_tool.id,
        std::slice::from_ref(&a),
        "2.0.0",
        &spec,
    )
    .await;
    fail!(resp, 404);
    // a tool the user can't edit in every group is 401
    let resp = direct_instance(
        &user.client,
        shared_tool.id,
        std::slice::from_ref(&a),
        "2.0.0",
        &with_marker(&spec, "x"),
    )
    .await;
    fail!(resp, 401);
    // a spec of another kind is 400
    let resp = direct_instance(
        &user.client,
        own_tool.id,
        std::slice::from_ref(&a),
        "2.0.0",
        &generators::gen_image_spec(),
    )
    .await;
    fail!(resp, 400);
    // none of them created an instance
    is!(listed(&admin, hidden_tool.id).await?.len(), 1);
    is!(listed(&admin, shared_tool.id).await?.len(), 1);
    is!(listed(&admin, own_tool.id).await?.len(), 1);
    Ok(())
}

/// Concurrent publishes get unique labels and leave one consistent latest
#[tokio::test]
async fn concurrent_publishes() -> Result<(), Error> {
    // create a tool
    let admin = test_utilities::admin_client().await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let spec = generators::gen_agentic_spec();
    let (tool, _) = generators::tool(&[&group], "0.1.0", &spec, &admin).await?;
    // publish 10 distinct versions at once
    let publishes = (0..10).map(|index| {
        let admin = admin.clone();
        let spec = with_marker(&spec, &format!("concurrent {index}"));
        async move { publish(&admin, tool.id, &format!("1.0.{index}"), &spec).await }
    });
    for result in futures::future::join_all(publishes).await {
        result?;
    }
    // every version was created and latest names the most recently created one
    let summaries = admin.entities.tool_instances(tool.id).await?;
    is!(summaries.len(), 11);
    let labels = summaries
        .iter()
        .map(|summary| summary.version.to_string())
        .collect::<HashSet<String>>();
    is!(labels.len(), 11);
    let tool_entity = admin.entities.get(tool.id).await?;
    let meta = tool_meta(&tool_entity);
    is!(meta.latest, Some(summaries[0].id));
    is!(
        meta.latest_version.clone(),
        Some(summaries[0].version.clone())
    );
    is!(
        meta.latest_spec_hash.clone(),
        Some(summaries[0].spec_hash.clone())
    );
    // two publishes of one label at once: exactly one succeeds
    let (left, right) = (with_marker(&spec, "left"), with_marker(&spec, "right"));
    let first = publish(&admin, tool.id, "2.0.0", &left);
    let second = publish(&admin, tool.id, "2.0.0", &right);
    let (first, second) = tokio::join!(first, second);
    let results = [first, second];
    is!(results.iter().filter(|result| result.is_ok()).count(), 1);
    for result in results {
        if let Err(err) = result {
            is!(err.status().map(|status| status.as_u16()), Some(409));
        }
    }
    is!(listed(&admin, tool.id).await?.len(), 12);
    Ok(())
}

/// Tool creates without a version or a valid spec are refused and nothing is created
#[tokio::test]
async fn create_refusals() -> Result<(), Error> {
    // get an admin client and a group
    let admin = test_utilities::admin_client().await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let spec = generators::gen_agentic_spec();
    // build raw forms with some tool fields left out
    let raw = serde_json::to_string(&spec)?;
    let cases: Vec<(&str, Vec<(&str, String)>)> = vec![
        (
            "no version",
            vec![
                ("metadata[tool_kind]", "Agentic".into()),
                ("metadata[tool_spec]", raw.clone()),
            ],
        ),
        (
            "no spec",
            vec![
                ("metadata[tool_kind]", "Agentic".into()),
                ("metadata[tool_version]", "1.0.0".into()),
            ],
        ),
        (
            "bad spec",
            vec![
                ("metadata[tool_kind]", "Agentic".into()),
                ("metadata[tool_version]", "1.0.0".into()),
                ("metadata[tool_spec]", "{\"Agentic\": 5}".into()),
            ],
        ),
        (
            "external MCP backing",
            vec![
                ("metadata[tool_kind]", "Agentic".into()),
                ("metadata[tool_version]", "1.0.0".into()),
                (
                    "metadata[tool_spec]",
                    raw.replace(
                        "\"ThoriumMcp\":{\"tool\":\"get_sample\"}",
                        "\"ExternalMcp\":{\"url\":\"http://x\",\"tool\":\"t\"}",
                    ),
                ),
            ],
        ),
    ];
    for (case, fields) in cases {
        // send the form as a raw create
        let name = format!("refused{}", generators::gen_string(10).to_lowercase());
        let form = fields.into_iter().fold(
            Form::new()
                .text("name", name.clone())
                .text("kind", "Tool")
                .text("groups[]", group.clone()),
            |form, (key, value)| form.text(key, value),
        );
        let status = raw_create(&admin, form).await?;
        is!(status, 400, case);
        // nothing named after it was created
        assert!(
            names_in(&admin, &group)
                .await?
                .iter()
                .all(|listed| !listed.starts_with(&name)),
            "{case}"
        );
    }
    Ok(())
}

/// Tool specs with an unknown MCP tool, no timeout, or a bad environment are refused
#[tokio::test]
async fn create_spec_refusals() -> Result<(), Error> {
    // get an admin client and a group
    let admin = test_utilities::admin_client().await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let spec = generators::gen_agentic_spec();
    // an unknown MCP tool is refused, naming it
    let ToolSpec::Agentic(mut unknown) = spec.clone() else {
        unreachable!()
    };
    unknown.backing = AgenticToolBacking::ThoriumMcp {
        tool: "no_such_tool".into(),
    };
    let resp = admin
        .entities
        .create(generators::gen_tool(
            &[&group],
            "1.0.0",
            &ToolSpec::Agentic(unknown),
        ))
        .await;
    fail!(resp, 400);
    assert!(resp.unwrap_err().msg().unwrap().contains("no_such_tool"));
    // an MCP backing naming one of Thorium's tools is accepted
    admin
        .entities
        .create(generators::gen_tool(&[&group], "1.0.0", &spec))
        .await?;
    // a missing timeout is refused
    let ToolSpec::Agentic(mut no_timeout) = spec.clone() else {
        unreachable!()
    };
    no_timeout.timeout = None;
    let resp = admin
        .entities
        .create(generators::gen_tool(
            &[&group],
            "1.0.0",
            &ToolSpec::Agentic(no_timeout),
        ))
        .await;
    fail!(resp, 400);
    // an Image spec needs an environment, and an External one is accepted
    for (raw_spec, code) in [
        (json!({"Image": {}}), 400),
        (
            json!({"Image": {"environment": {"K8s": {"scheduling": {"resources": {"cpu": 1, "memory": 1}, "spawn_limit": "Unlimited"}}}}}),
            400,
        ),
        (
            json!({"Image": {"environment": {"K8s": {"image": "repo/x"}}}}),
            400,
        ),
        (json!({"Image": {"environment": "External"}}), 200),
    ] {
        let form = Form::new()
            .text(
                "name",
                format!("img{}", generators::gen_string(10).to_lowercase()),
            )
            .text("kind", "Tool")
            .text("groups[]", group.clone())
            .text("metadata[tool_kind]", "Image")
            .text("metadata[tool_version]", "1.0.0")
            .text("metadata[tool_spec]", raw_spec.to_string());
        let status = raw_create(&admin, form).await?;
        is!(status, code, raw_spec);
    }
    Ok(())
}

/// Send a raw entity create form as a new admin and get the status code
///
/// # Arguments
///
/// * `admin` - An admin client to get the host from
/// * `form` - The form to send
async fn raw_create(admin: &Thorium, form: Form) -> Result<u16, Error> {
    // create an admin user to send the raw request as
    let sender = generators::user_with_role(admin, UserRole::Admin).await?;
    // send the form
    let resp = reqwest::Client::new()
        .post(format!("{}/api/entities/", admin.host))
        .header("authorization", sender.auth_header())
        .multipart(form)
        .send()
        .await?;
    Ok(resp.status().as_u16())
}

/// List the names of every entity in a group
///
/// # Arguments
///
/// * `client` - The client to list with
/// * `group` - The group to list
async fn names_in(client: &Thorium, group: &str) -> Result<Vec<String>, Error> {
    // list the group's entities
    let opts = EntityListOpts::default()
        .groups(vec![group.to_owned()])
        .limit(10_000);
    let mut cursor = client.entities.list(&opts).await?;
    let mut names = cursor
        .data
        .drain(..)
        .map(|line| line.name)
        .collect::<Vec<String>>();
    // page through the rest of the entities
    while !cursor.exhausted() {
        cursor.refill().await?;
        names.extend(cursor.data.drain(..).map(|line| line.name));
    }
    Ok(names)
}

/// Dependency settings name tools: a tool instance, a missing id, and a tool the publisher
/// can't see each get the same `400`, and a visible tool publishes
#[tokio::test]
async fn dependency_settings_name_visible_tools() -> Result<(), Error> {
    // a user in one group, a tool in that group, and a tool in a group hidden from the user
    let admin = test_utilities::admin_client().await?;
    let user = plain_user(&admin).await?;
    let group = group_for(&user.username, &admin).await?;
    let hidden = generators::groups(1, &admin).await?.remove(0).name;
    let (visible, visible_instance) =
        generators::tool(&[&group], "1.0.0", &generators::gen_image_spec(), &admin).await?;
    let (hidden_tool, _) =
        generators::tool(&[&hidden], "1.0.0", &generators::gen_image_spec(), &admin).await?;
    // build an Image spec naming a tool in one of its dependency lists
    let naming = |kind: usize, id: Uuid| {
        let mut spec = generators::gen_image_spec();
        if let ToolSpec::Image(workload) = &mut spec {
            let deps = &mut workload.data.dependencies;
            match kind {
                0 => deps.results.tools = vec![id],
                1 => deps.children.tools = vec![id],
                _ => deps.filesystems.tools = vec![id],
            }
        }
        spec
    };
    // a tool instance, a missing id, and a hidden tool are refused alike in every list
    let refused = [visible_instance.id, Uuid::new_v4(), hidden_tool.id];
    for (kind, id) in refused.iter().enumerate() {
        let resp = user
            .client
            .entities
            .create(generators::gen_tool(&[&group], "1.0.0", &naming(kind, *id)))
            .await;
        fail!(resp, 400);
        let Err(err) = resp else { unreachable!() };
        let msg = err.msg().unwrap_or_default();
        assert!(
            msg.contains(&id.to_string()) && msg.contains("you can't see it"),
            "{msg}"
        );
    }
    // publishing a new version is checked the same way
    let resp = user
        .client
        .entities
        .update(
            visible.id,
            generators::gen_tool_publish("2.0.0", &naming(0, hidden_tool.id)),
        )
        .await;
    fail!(resp, 400);
    // a visible tool publishes in every list
    for kind in 0..3 {
        let (_, instance) =
            generators::tool(&[&group], "1.0.0", &naming(kind, visible.id), &user.client).await?;
        let ToolSpec::Image(workload) = &instance_meta(&instance).spec else {
            unreachable!()
        };
        is!(workload.data.dependencies.tools(), vec![visible.id]);
    }
    Ok(())
}

/// Spec env values are shared and permanent, and the tools dependencies name are hashed
#[tokio::test]
async fn spec_env_and_image_names() -> Result<(), Error> {
    // an Image tool with a secret in its K8s env publishes without a warning
    let admin = test_utilities::admin_client().await?;
    let reader = plain_user(&admin).await?;
    let group = group_for(&reader.username, &admin).await?;
    let mut spec = generators::gen_image_spec();
    if let ToolSpec::Image(workload) = &mut spec
        && let WorkloadEnvironment::K8s(env) = &mut workload.environment
    {
        env.env.insert("API_KEY".into(), Some("s3cret".into()));
    }
    let (tool, v1) = generators::tool(&[&group], "v1", &spec, &admin).await?;
    // another member of the group reads the value on the instance
    let read = reader.client.entities.get(v1.id).await?;
    let ToolSpec::Image(workload) = &instance_meta(&read).spec else {
        unreachable!()
    };
    let WorkloadEnvironment::K8s(env) = &workload.environment else {
        unreachable!()
    };
    is!(env.env.get("API_KEY"), Some(&Some("s3cret".to_owned())));
    // publishing v2 without it leaves v1 unchanged
    let mut v2_spec = spec.clone();
    if let ToolSpec::Image(workload) = &mut v2_spec
        && let WorkloadEnvironment::K8s(env) = &mut workload.environment
    {
        env.env.clear();
    }
    publish(&admin, tool.id, "v2", &v2_spec).await?;
    let still = admin.entities.get(v1.id).await?;
    is!(instance_meta(&still).spec.clone(), spec.clone());
    // deleting v1 removes it
    admin.entities.delete(v1.id).await?;
    let resp = admin.entities.get(v1.id).await;
    fail!(resp, 404);
    // tools named in result dependencies are hashed
    let mut deps = generators::gen_image_spec();
    if let ToolSpec::Image(workload) = &mut deps {
        workload.data.dependencies.results.tools = vec![tool.id];
    }
    let (other, deps_instance) = generators::tool(&[&group], "1.0.0", &deps, &admin).await?;
    let mut changed = deps.clone();
    if let ToolSpec::Image(workload) = &mut changed {
        workload.data.dependencies.results.tools = vec![other.id];
    }
    assert_ne!(
        instance_meta(&deps_instance).spec_hash,
        ThorDigest::tool_spec(&changed, None)?
    );
    Ok(())
}

/// The client form carries the spec as one field and the server hashes the same typed spec;
/// unknown metadata fields are refused
#[tokio::test]
async fn form_transport() -> Result<(), Error> {
    // create a tool through the client form
    let admin = test_utilities::admin_client().await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let spec = generators::gen_agentic_spec();
    let (tool, instance) = generators::tool(&[&group], "1.0.0", &spec, &admin).await?;
    // the stored spec equals the typed spec and hashes the same
    is!(instance_meta(&instance).spec.clone(), spec.clone());
    is!(
        instance_meta(&instance).spec_hash.clone(),
        ThorDigest::tool_spec(&spec, None)?
    );
    is!(tool_meta(&tool).kind, ToolKind::Agentic);
    // an unknown metadata field is refused
    let form = Form::new()
        .text("name", "unknown")
        .text("kind", "Tool")
        .text("groups[]", group.clone())
        .text("metadata[tool_kind]", "Agentic")
        .text("metadata[tool_version]", "1.0.0")
        .text("metadata[tool_spec]", serde_json::to_string(&spec)?)
        .text("metadata[tool_provides][]", Uuid::new_v4().to_string());
    let status = raw_create(&admin, form).await?;
    is!(status, 400);
    Ok(())
}

/// Labels are unique per tool, are required to publish, and parse as semver or custom labels
#[tokio::test]
async fn version_labels() -> Result<(), Error> {
    // two tools with the same first label
    let admin = test_utilities::admin_client().await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let spec = generators::gen_agentic_spec();
    let (tool, _) = generators::tool(&[&group], "2.1.0", &spec, &admin).await?;
    generators::tool(
        &[&group],
        "2.1.0",
        &with_marker(&spec, "other tool"),
        &admin,
    )
    .await?;
    // a duplicate label on the same tool is refused
    let resp = publish(&admin, tool.id, "2.1.0", &with_marker(&spec, "dup")).await;
    fail!(resp, 409);
    // a spec without a version is refused
    let update = EntityUpdate::default().metadata(EntityMetadataUpdate::Tool {
        version: None,
        spec: Some(serde_json::to_string(&with_marker(&spec, "unlabeled"))?),
        provides: Vec::new(),
        harness_env: None,
        clear_harness_env: None,
        disable_models: BTreeSet::new(),
        enable_models: BTreeSet::new(),
    });
    let resp = admin.entities.update(tool.id, update).await;
    fail!(resp, 400);
    // a direct create without a version is refused
    let form = Form::new()
        .text("name", "x")
        .text("kind", "ToolInstance")
        .text("groups[]", group.clone())
        .text("metadata[tool_instance_tool]", tool.id.to_string())
        .text(
            "metadata[tool_spec]",
            serde_json::to_string(&with_marker(&spec, "unlabeled"))?,
        );
    let status = raw_create(&admin, form).await?;
    is!(status, 400);
    is!(listed(&admin, tool.id).await?.len(), 1);
    // custom labels are kept as written in tags, latest, and the name
    for label in ["v2.1.0", "migrated"] {
        publish(&admin, tool.id, label, &with_marker(&spec, label)).await?;
        let refreshed = admin.entities.get(tool.id).await?;
        let meta = tool_meta(&refreshed);
        is!(
            meta.latest_version.clone(),
            Some(ToolVersion::Custom(label.to_owned()))
        );
        let instance = admin.entities.get(meta.latest.unwrap()).await?;
        is!(instance.name, format!("{}@{label}", tool.name));
        is!(tag_values(&instance, "ToolVersion"), vec![label.to_owned()]);
    }
    // a semver label is parsed as semver
    let first = admin.entities.tool_instances(tool.id).await?.pop().unwrap();
    assert!(matches!(first.version, ToolVersion::SemVer(_)));
    Ok(())
}

/// Latest is the most recently created non-deprecated instance and only latest is compared
#[tokio::test]
async fn latest_rules() -> Result<(), Error> {
    // publish 1.0.0 then 2.0.0
    let admin = test_utilities::admin_client().await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let spec = generators::gen_agentic_spec();
    let spec_one = with_marker(&spec, "one");
    let spec_two = with_marker(&spec, "two");
    let (tool, one) = generators::tool(&[&group], "1.0.0", &spec_one, &admin).await?;
    publish(&admin, tool.id, "2.0.0", &spec_two).await?;
    // a hotfix published after 2.0.0 becomes latest
    publish(&admin, tool.id, "1.0.1", &with_marker(&spec, "hotfix")).await?;
    let latest = |tool: &Entity| tool_meta(tool).latest_version.clone();
    is!(
        latest(&admin.entities.get(tool.id).await?),
        Some(ToolVersion::from("1.0.1"))
    );
    // 2.0.0's spec again as 2.0.1 is accepted because only latest is compared
    publish(&admin, tool.id, "2.0.1", &spec_two).await?;
    is!(
        latest(&admin.entities.get(tool.id).await?),
        Some(ToolVersion::from("2.0.1"))
    );
    // the same spec again as 2.0.2 is a no-op naming 2.0.1, which queues nothing
    let resp = publish(&admin, tool.id, "2.0.2", &spec_two).await;
    fail!(resp, 409);
    assert!(resp.unwrap_err().msg().unwrap().contains("2.0.1"));
    is!(tool_updates(&admin, tool.id).await?.len(), 4);
    // a spec equal to an older deprecated instance's is accepted
    admin
        .entities
        .update(one.id, generators::gen_tool_deprecate(true))
        .await?;
    publish(&admin, tool.id, "3.0.0", &spec_one).await?;
    is!(tool_updates(&admin, tool.id).await?.len(), 5);
    // deprecating latest moves it to the newest non-deprecated instance, queuing a tool update
    let summaries = admin.entities.tool_instances(tool.id).await?;
    let newest = summaries[0].id;
    admin
        .entities
        .update(newest, generators::gen_tool_deprecate(true))
        .await?;
    let moved = admin.entities.get(tool.id).await?;
    is!(latest(&moved), Some(ToolVersion::from("2.0.1")));
    let updates = tool_updates(&admin, tool.id).await?;
    is!(updates.len(), 6);
    is!(updates.last().copied(), tool_meta(&moved).latest);
    // deprecating a non-latest instance leaves latest alone
    let hotfix = summaries
        .iter()
        .find(|summary| summary.version == ToolVersion::from("1.0.1"))
        .unwrap()
        .id;
    admin
        .entities
        .update(hotfix, generators::gen_tool_deprecate(true))
        .await?;
    is!(
        latest(&admin.entities.get(tool.id).await?),
        Some(ToolVersion::from("2.0.1"))
    );
    // un-deprecating an older instance leaves latest alone
    admin
        .entities
        .update(hotfix, generators::gen_tool_deprecate(false))
        .await?;
    is!(
        latest(&admin.entities.get(tool.id).await?),
        Some(ToolVersion::from("2.0.1"))
    );
    // neither change to an instance that isn't latest queued anything
    is!(tool_changes(&admin, tool.id).await?.len(), 6);
    // un-deprecating the newest instance makes it latest again, queuing a tool update
    admin
        .entities
        .update(newest, generators::gen_tool_deprecate(false))
        .await?;
    is!(
        latest(&admin.entities.get(tool.id).await?),
        Some(ToolVersion::from("3.0.0"))
    );
    is!(
        tool_updates(&admin, tool.id).await?.last().copied(),
        Some(newest)
    );
    // after every instance is deprecated latest is empty
    for summary in admin.entities.tool_instances(tool.id).await? {
        if !summary.deprecated {
            admin
                .entities
                .update(summary.id, generators::gen_tool_deprecate(true))
                .await?;
        }
    }
    let empty = admin.entities.get(tool.id).await?;
    is!(tool_meta(&empty).latest, None::<Uuid>);
    is!(
        tool_meta(&empty).latest_version.clone(),
        None::<ToolVersion>
    );
    is!(tool_meta(&empty).latest_spec_hash.clone(), None::<String>);
    // each of those deprecations moved latest, and the last one queued a tool update with no
    // latest instance
    let updates = tool_updates(&admin, tool.id).await?;
    is!(updates.len(), 11);
    is!(updates.last().copied(), Some(Uuid::nil()));
    // the next publish of any spec is accepted and becomes latest
    publish(&admin, tool.id, "4.0.0", &spec_two).await?;
    let republished = admin.entities.get(tool.id).await?;
    is!(latest(&republished), Some(ToolVersion::from("4.0.0")));
    is!(
        tool_updates(&admin, tool.id).await?.last().copied(),
        tool_meta(&republished).latest
    );
    Ok(())
}

/// The no-op check compares typed specs, not request text
#[tokio::test]
async fn typed_hashing() -> Result<(), Error> {
    // create a tool
    let admin = test_utilities::admin_client().await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let spec = generators::gen_agentic_spec();
    let (tool, first) = generators::tool(&[&group], "1.0.0", &spec, &admin).await?;
    // the stored hash is a v1 hash
    let hash = &instance_meta(&first).spec_hash;
    assert!(hash.starts_with("v1:") && hash[3..].len() == 64);
    assert!(
        hash[3..]
            .chars()
            .all(|c| matches!(c, '0'..='9' | 'a'..='f'))
    );
    // the same spec with other whitespace, key order, and explicit defaults is a no-op naming v1
    let mut value = serde_json::to_value(&spec)?;
    value["Agentic"]["capabilities"] =
        json!({"effects": [], "idempotent": false, "file_inputs": false, "file_outputs": false});
    value["Agentic"]["result_format"] = json!("Json");
    value["Agentic"]["requires_approval"] = json!(false);
    let reordered = serde_json::to_string_pretty(&value)?;
    let update = EntityUpdate::default().metadata(EntityMetadataUpdate::Tool {
        version: Some("1.0.1".into()),
        spec: Some(format!("  {reordered}\n")),
        provides: Vec::new(),
        harness_env: None,
        clear_harness_env: None,
        disable_models: BTreeSet::new(),
        enable_models: BTreeSet::new(),
    });
    let resp = admin.entities.update(tool.id, update).await;
    fail!(resp, 409);
    assert!(resp.unwrap_err().msg().unwrap().contains("1.0.0"));
    Ok(())
}

/// Unpinned image tags publish without a warning and differ in hash from digest-pinned images
#[tokio::test]
async fn unpinned_images() -> Result<(), Error> {
    // build a tag-pinned and a digest-pinned spec
    let admin = test_utilities::admin_client().await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let set_image = |image: String| {
        let mut spec = generators::gen_image_spec();
        if let ToolSpec::Image(workload) = &mut spec
            && let WorkloadEnvironment::K8s(env) = &mut workload.environment
        {
            env.image = image;
        }
        spec
    };
    let latest = set_image("repo/tool:latest".into());
    let digest = set_image(format!("repo/tool@sha256:{}", "b".repeat(64)));
    // both publish and their hashes differ
    let latest_resp = admin
        .entities
        .create(generators::gen_tool(&[&group], "1.0.0", &latest))
        .await?;
    let (_, digest_instance) = generators::tool(&[&group], "1.0.0", &digest, &admin).await?;
    let latest_instance = admin
        .entities
        .get(latest_resp.tool_instance.unwrap())
        .await?;
    assert_ne!(
        instance_meta(&latest_instance).spec_hash,
        instance_meta(&digest_instance).spec_hash
    );
    Ok(())
}

/// Intrinsic tags find identical instances per visible group and can't be removed
#[tokio::test]
async fn intrinsic_tags() -> Result<(), Error> {
    // a user in group a only
    let admin = test_utilities::admin_client().await?;
    let user = plain_user(&admin).await?;
    let a = group_for(&user.username, &admin).await?;
    let b = generators::groups(1, &admin).await?.remove(0).name;
    // the same Image spec published under two tools in a and b
    let spec = generators::gen_image_spec();
    let (_, in_a) = generators::tool(&[&a], "1.0.0", &spec, &admin).await?;
    let (_, in_b) = generators::tool(&[&b], "x-1", &spec, &admin).await?;
    let hash = instance_meta(&in_a).spec_hash.clone();
    is!(instance_meta(&in_b).spec_hash.clone(), hash.clone());
    // Image instances have a kind and hash tag but no function tag
    is!(tag_values(&in_a, "ToolKind"), vec!["Image".to_owned()]);
    is!(tag_values(&in_a, "ToolSpecHash"), vec![hash.clone()]);
    assert_eq!(tag_values(&in_a, "ToolFunction").len(), 0);
    // a tag query by hash finds both for the admin and only the visible one for the user
    let query = |groups: Vec<String>| {
        EntityListOpts::default()
            .groups(groups)
            .tag("ToolSpecHash", hash.clone())
            .limit(100)
    };
    let found = admin
        .entities
        .list(&query(vec![a.clone(), b.clone()]))
        .await?
        .data
        .into_iter()
        .map(|line| line.id)
        .collect::<HashSet<Uuid>>();
    is!(found, HashSet::from([in_a.id, in_b.id]));
    let visible = user
        .client
        .entities
        .list(&query(vec![a.clone()]))
        .await?
        .data
        .into_iter()
        .map(|line| line.id)
        .collect::<Vec<Uuid>>();
    is!(visible, vec![in_a.id]);
    // users can't remove the intrinsic tags but can remove their own
    let remove = json!({"tags": {"ToolSpecHash": [hash.clone()]}});
    let status = raw_tags(&admin.host, &user, in_a.id, true, remove).await?;
    is!(status, 400);
    let add = json!({"tags": {"Owner": ["me"]}});
    let status = raw_tags(&admin.host, &user, in_a.id, false, add).await?;
    is!(status, 204);
    let own = json!({"tags": {"Owner": ["me"]}});
    let status = raw_tags(&admin.host, &user, in_a.id, true, own).await?;
    is!(status, 204);
    let after = admin.entities.get(in_a.id).await?;
    is!(tag_values(&after, "ToolSpecHash"), vec![hash.clone()]);
    Ok(())
}

/// Create a tool with a name in one group
///
/// # Arguments
///
/// * `name` - The tool's name
/// * `group` - The tool's group
/// * `client` - The client to create the tool with
async fn named_tool(name: &str, group: &str, client: &Thorium) -> Result<Uuid, Error> {
    // build a tool request and give it the name
    let mut req = generators::gen_tool(&[group], "1.0.0", &generators::gen_agentic_spec());
    name.clone_into(&mut req.name);
    Ok(client.entities.create(req).await?.id)
}

/// List the names and ids of the tools in a group whose names contain a marker, the way the
/// scaler lists a group's tools to resolve network policies' tool-name entries
///
/// # Arguments
///
/// * `group` - The group to list tools in
/// * `marker` - The text the names must contain
/// * `client` - The client to list with
async fn named_tools(
    group: &str,
    marker: &str,
    client: &Thorium,
) -> Result<BTreeSet<(String, Uuid)>, Error> {
    // list only the group's tools, one page at a time
    let opts = EntityListOpts::default()
        .groups(vec![group])
        .kind(thorium::models::EntityKinds::Tool)
        .page_size(500);
    let mut cursor = client.entities.list(&opts).await?;
    let mut tools = BTreeSet::new();
    loop {
        // keep the tools whose names hold the marker
        for line in cursor.data.drain(..) {
            if line.name.contains(marker) {
                tools.insert((line.name, line.id));
            }
        }
        // stop once every page has been read
        if cursor.exhausted() {
            break;
        }
        cursor.refill().await?;
    }
    Ok(tools)
}

/// The tool listing the scaler resolves network policies' tool-name entries with gives each
/// tool in a group with its current exact name, so tools whose names differ only in case or
/// punctuation, and tools with the same name in other groups, stay apart
#[tokio::test]
async fn tool_listing_resolves_exact_names_per_group() -> Result<(), Error> {
    // two groups and a marker unique to this test
    let admin = test_utilities::admin_client().await?;
    let groups = generators::groups(2, &admin).await?;
    let (a, b) = (groups[0].name.clone(), groups[1].name.clone());
    let marker = generators::gen_string(10).to_lowercase();
    // a granted name, a similar name in the same group, and the granted name in another group
    let granted = named_tool(&format!("ghidra-{marker}"), &a, &admin).await?;
    let similar = named_tool(&format!("Ghidra {marker}"), &a, &admin).await?;
    let elsewhere = named_tool(&format!("ghidra-{marker}"), &b, &admin).await?;
    // rename the similar tool to a name that loses its punctuation to the same stage label
    admin
        .entities
        .update(
            similar,
            EntityUpdate::default().name(format!("ghidra_{marker}")),
        )
        .await?;
    // each group lists its own tools under their current names
    let in_a = BTreeSet::from([
        (format!("ghidra-{marker}"), granted),
        (format!("ghidra_{marker}"), similar),
    ]);
    is!(named_tools(&a, &marker, &admin).await?, in_a);
    let in_b = BTreeSet::from([(format!("ghidra-{marker}"), elsewhere)]);
    is!(named_tools(&b, &marker, &admin).await?, in_b);
    Ok(())
}

/// Only publishing creates tool instances
#[tokio::test]
async fn nothing_else_creates_instances() -> Result<(), Error> {
    // create a tool with two versions
    let admin = test_utilities::admin_client().await?;
    let groups = generators::groups(2, &admin).await?;
    let (a, b) = (groups[0].name.clone(), groups[1].name.clone());
    let spec = generators::gen_agentic_spec();
    let (tool, first) = generators::tool(&[&a], "1.0.0", &spec, &admin).await?;
    publish(&admin, tool.id, "1.1.0", &with_marker(&spec, "v2")).await?;
    let before = listed(&admin, tool.id).await?;
    let queued = tool_changes(&admin, tool.id).await?.len();
    // editing the name, description, and tags creates nothing
    admin
        .entities
        .update(
            tool.id,
            EntityUpdate::default().name("renamed").description("new"),
        )
        .await?;
    let tagger = generators::user_with_role(&admin, UserRole::Admin).await?;
    let tags = json!({"groups": [a.clone()], "tags": {"Team": ["red"]}});
    let status = raw_tags(&admin.host, &tagger, tool.id, false, tags).await?;
    is!(status, 204);
    // the user tag edit queued one tag change naming its pair, and the rename queued nothing
    let pair = BTreeMap::from([("Team".to_owned(), BTreeSet::from(["red".to_owned()]))]);
    let tag_change = vec![WorkflowGraphChange::ToolTagsChanged {
        tool: tool.id,
        tags: pair,
    }];
    let since = tool_changes(&admin, tool.id).await?.split_off(queued);
    is!(since, tag_change);
    // tagging and untagging an instance queues nothing at all
    let instance_queued = tool_changes(&admin, first.id).await?.len();
    let tags = json!({"groups": [a.clone()], "tags": {"Reviewed": ["yes"]}});
    let status = raw_tags(&admin.host, &tagger, first.id, false, tags.clone()).await?;
    is!(status, 204);
    let status = raw_tags(&admin.host, &tagger, first.id, true, tags).await?;
    is!(status, 204);
    is!(tool_changes(&admin, first.id).await?.len(), instance_queued);
    // adding and removing a group creates nothing
    admin
        .entities
        .update(tool.id, EntityUpdate::default().group(&b))
        .await?;
    admin
        .entities
        .update(tool.id, EntityUpdate::default().remove_group(&b))
        .await?;
    // deprecating and un-deprecating a non-latest instance creates nothing
    admin
        .entities
        .update(first.id, generators::gen_tool_deprecate(true))
        .await?;
    admin
        .entities
        .update(first.id, generators::gen_tool_deprecate(false))
        .await?;
    // the instance list is unchanged
    is!(listed(&admin, tool.id).await?, before);
    // none of these edits queued a tool update or any other change but the tag change
    let since = tool_changes(&admin, tool.id).await?.split_off(queued);
    is!(since, tag_change);
    Ok(())
}

/// A tool instance only takes deprecation; its spec, version, name, and groups never change
#[tokio::test]
async fn instance_updates() -> Result<(), Error> {
    // create a tool
    let admin = test_utilities::admin_client().await?;
    let groups = generators::groups(2, &admin).await?;
    let (a, b) = (groups[0].name.clone(), groups[1].name.clone());
    let spec = generators::gen_agentic_spec();
    let (_, instance) = generators::tool(&[&a], "1.0.0", &spec, &admin).await?;
    // a spec or version is refused
    let update = EntityUpdate::default().metadata(EntityMetadataUpdate::Tool {
        version: Some("2.0.0".into()),
        spec: Some(serde_json::to_string(&spec)?),
        provides: Vec::new(),
        harness_env: None,
        clear_harness_env: None,
        disable_models: BTreeSet::new(),
        enable_models: BTreeSet::new(),
    });
    let resp = admin.entities.update(instance.id, update).await;
    fail!(resp, 400);
    // a name is refused
    fail!(
        admin
            .entities
            .update(instance.id, EntityUpdate::default().name("other"))
            .await,
        400
    );
    // adding or removing a group is refused
    fail!(
        admin
            .entities
            .update(instance.id, EntityUpdate::default().group(&b))
            .await,
        400
    );
    fail!(
        admin
            .entities
            .update(instance.id, EntityUpdate::default().remove_group(&a))
            .await,
        400
    );
    // deprecation is accepted
    admin
        .entities
        .update(instance.id, generators::gen_tool_deprecate(true))
        .await?;
    let deprecated = admin.entities.get(instance.id).await?;
    assert!(instance_meta(&deprecated).deprecated);
    is!(instance_meta(&deprecated).spec.clone(), spec);
    // existing kinds still add and remove groups
    let (_, device) = generators::entity(
        generators::gen_entity(&a, generators::gen_device_meta(Vec::new())),
        &admin,
    )
    .await?;
    admin
        .entities
        .update(device.id, EntityUpdate::default().group(&b))
        .await?;
    admin
        .entities
        .update(device.id, EntityUpdate::default().remove_group(&a))
        .await?;
    let groups = admin.entities.get(device.id).await?.groups;
    is!(groups, vec![b.clone()]);
    Ok(())
}

/// A tool with instances can't be deleted until its instances are
#[tokio::test]
async fn delete_tool_with_instances() -> Result<(), Error> {
    // create a tool with two instances
    let admin = test_utilities::admin_client().await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let spec = generators::gen_agentic_spec();
    let (tool, _) = generators::tool(&[&group], "1.0.0", &spec, &admin).await?;
    publish(&admin, tool.id, "1.1.0", &with_marker(&spec, "v2")).await?;
    // deleting the tool is refused while it has instances
    let resp = admin.entities.delete(tool.id).await;
    fail!(resp, 409);
    // after its instances are deleted the tool is deleted
    for id in listed(&admin, tool.id).await? {
        admin.entities.delete(id).await?;
    }
    admin.entities.delete(tool.id).await?;
    let resp = admin.entities.get(tool.id).await;
    fail!(resp, 404);
    Ok(())
}

/// Deleting a tool instance removes its stored runtime, and deleting it again changes nothing
#[tokio::test]
async fn delete_instance_removes_runtime() -> Result<(), Error> {
    // create an image tool with two instances
    let admin = test_utilities::admin_client().await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let spec = generators::gen_image_spec();
    let (tool, first) = generators::tool(&[&group], "1.0.0", &spec, &admin).await?;
    publish(&admin, tool.id, "1.1.0", &with_marker(&spec, "v2")).await?;
    let ids = listed(&admin, tool.id).await?;
    is!(ids.len(), 2);
    // every instance starts with a stored runtime
    let seeded = admin.jobs.workload_runtimes().await?.runtimes;
    for id in &ids {
        assert!(seeded.contains_key(id), "instance {id} has no runtime");
    }
    // deleting one instance removes only its runtime
    admin.entities.delete(first.id).await?;
    let after = admin.jobs.workload_runtimes().await?.runtimes;
    assert!(!after.contains_key(&first.id));
    let kept = ids
        .iter()
        .find(|id| **id != first.id)
        .expect("a second instance");
    assert!(after.contains_key(kept));
    // deleting the same instance again is refused without changing the runtimes
    let resp = admin.entities.delete(first.id).await;
    fail!(resp, 404);
    let again = admin.jobs.workload_runtimes().await?.runtimes;
    assert!(again.contains_key(kept));
    // deleting the last instance and the tool leaves no runtime of theirs
    admin.entities.delete(*kept).await?;
    admin.entities.delete(tool.id).await?;
    let none = admin.jobs.workload_runtimes().await?.runtimes;
    assert!(ids.iter().all(|id| !none.contains_key(id)));
    Ok(())
}

/// Deleting a shared definition needs rights in all of its groups and removes every copy
#[tokio::test]
async fn delete_everywhere() -> Result<(), Error> {
    // a manager of a only, a manager of both, and the submitter
    let admin = test_utilities::admin_client().await?;
    let partial = plain_user(&admin).await?;
    let full = plain_user(&admin).await?;
    let submitter = plain_user(&admin).await?;
    let a = generators::group_with_roles(
        &GroupRoles::default()
            .manager(&partial.username)
            .manager(&full.username)
            .user(&submitter.username),
        GroupAllowed::default(),
        &admin,
    )
    .await?;
    let b = generators::group_with_roles(
        &GroupRoles::default()
            .manager(&full.username)
            .user(&submitter.username)
            .user(&partial.username),
        GroupAllowed::default(),
        &admin,
    )
    .await?;
    let spec = generators::gen_agentic_spec();
    // a plain member who isn't the submitter can't delete
    let (tool, instance) = generators::tool(&[&a, &b], "1.0.0", &spec, &submitter.client).await?;
    let member = plain_user(&admin).await?;
    admin
        .groups
        .update(
            &a,
            &GroupUpdate::default()
                .users(thorium::models::GroupUsersUpdate::default().direct_add(&member.username)),
        )
        .await?;
    let resp = member.client.entities.delete(instance.id).await;
    fail!(resp, 401);
    // a manager of only one group can't delete and nothing is removed
    let resp = partial.client.entities.delete(instance.id).await;
    fail!(resp, 401);
    is!(
        stored_groups(instance.id).await?,
        sorted(&[a.clone(), b.clone()])
    );
    // a manager of both groups deletes it from every group
    full.client.entities.delete(instance.id).await?;
    is!(stored_groups(instance.id).await?, Vec::<String>::new());
    let ToolSpec::Agentic(_) = &spec else {
        unreachable!()
    };
    // its tags and edges are gone in both groups
    let tagged = admin
        .entities
        .list(
            &EntityListOpts::default()
                .groups(vec![a.clone(), b.clone()])
                .tag("ToolId", tool.id.to_string())
                .limit(100),
        )
        .await?;
    assert_eq!(tagged.data.len(), 0);
    let resp = admin.entities.associations(instance.id).await;
    fail!(resp, 404);
    is!(listed(&admin, tool.id).await?, Vec::<Uuid>::new());
    // the submitter and an admin can each delete an instance
    publish(
        &submitter.client,
        tool.id,
        "2.0.0",
        &with_marker(&spec, "two"),
    )
    .await?;
    let second = listed(&admin, tool.id).await?[0];
    submitter.client.entities.delete(second).await?;
    publish(
        &submitter.client,
        tool.id,
        "3.0.0",
        &with_marker(&spec, "three"),
    )
    .await?;
    let third = listed(&admin, tool.id).await?[0];
    admin.entities.delete(third).await?;
    is!(stored_groups(third).await?, Vec::<String>::new());
    Ok(())
}

/// Adding a group to a tool adds it to every instance, with its edges, in one request
#[tokio::test]
async fn widening() -> Result<(), Error> {
    // create a tool in a with three instances
    let admin = test_utilities::admin_client().await?;
    let groups = generators::groups(2, &admin).await?;
    let (a, b) = (groups[0].name.clone(), groups[1].name.clone());
    let spec = generators::gen_agentic_spec();
    let (tool, _) = generators::tool(&[&a], "1.0.0", &spec, &admin).await?;
    publish(&admin, tool.id, "1.1.0", &with_marker(&spec, "v2")).await?;
    publish(&admin, tool.id, "1.2.0", &with_marker(&spec, "v3")).await?;
    let instances = listed(&admin, tool.id).await?;
    // widening the tool to b reaches every instance in the same request
    admin
        .entities
        .update(tool.id, EntityUpdate::default().group(&b))
        .await?;
    is!(
        stored_groups(tool.id).await?,
        sorted(&[a.clone(), b.clone()])
    );
    for id in &instances {
        is!(stored_groups(*id).await?, sorted(&[a.clone(), b.clone()]));
        // each instance's edge to its tool is in b too
        let edges = admin.entities.associations(*id).await?;
        assert!(
            edges.iter().any(|edge| edge.kind == AssociationKind::PublishedFrom
                && edge.groups.contains(&b)),
            "{edges:?}"
        );
    }
    // a b-only viewer lists the instances in b
    let viewer = plain_user(&admin).await?;
    admin
        .groups
        .update(
            &b,
            &GroupUpdate::default()
                .users(thorium::models::GroupUsersUpdate::default().direct_add(&viewer.username)),
        )
        .await?;
    is!(listed(&viewer.client, tool.id).await?.len(), 3);
    Ok(())
}

/// A group can't be removed from a tool while data in it references the tool
#[tokio::test]
async fn removal_blocked_by_referrers() -> Result<(), Error> {
    // the tool's submitter edits a and b
    let admin = test_utilities::admin_client().await?;
    let user = plain_user(&admin).await?;
    let a = group_for(&user.username, &admin).await?;
    let b = group_for(&user.username, &admin).await?;
    let spec = generators::gen_agentic_spec();
    let (tool, _) = generators::tool(&[&a, &b], "1.0.0", &spec, &user.client).await?;
    publish(&user.client, tool.id, "1.1.0", &with_marker(&spec, "v2")).await?;
    // a device in b references the tool
    let (_, device) = generators::entity(
        generators::gen_entity(&b, generators::gen_device_meta(Vec::new())),
        &admin,
    )
    .await?;
    let req = AssociationRequest::new(AssociationKind::AssociatedWith, target(&device))
        .target(target(&tool));
    admin.associations.create(&req).await?;
    // removing b is refused, naming the referrer under b
    let resp = user
        .client
        .entities
        .update(tool.id, EntityUpdate::default().remove_group(&b))
        .await;
    fail!(resp, 409);
    let msg = resp.unwrap_err().msg().unwrap();
    assert!(msg.contains(&device.name) && msg.contains(&b), "{msg}");
    // the refused removal wrote nothing
    is!(
        stored_groups(tool.id).await?,
        sorted(&[a.clone(), b.clone()])
    );
    // a referrer in a doesn't block removing b, but the device in b still does
    let (_, other) = generators::entity(
        generators::gen_entity(&a, generators::gen_device_meta(Vec::new())),
        &admin,
    )
    .await?;
    let req = AssociationRequest::new(AssociationKind::AssociatedWith, target(&other))
        .target(target(&tool));
    admin.associations.create(&req).await?;
    // once the reference in b is deleted the removal cascades to every instance
    admin.entities.delete(device.id).await?;
    user.client
        .entities
        .update(tool.id, EntityUpdate::default().remove_group(&b))
        .await?;
    is!(stored_groups(tool.id).await?, vec![a.clone()]);
    for id in listed(&admin, tool.id).await? {
        is!(stored_groups(id).await?, vec![a.clone()]);
    }
    Ok(())
}

/// Edits need rights in every group, name only visible blocking groups, and rewrite every row
#[tokio::test]
async fn full_view_edits() -> Result<(), Error> {
    // a tool in a, b, and a hidden group
    let admin = test_utilities::admin_client().await?;
    let user = plain_user(&admin).await?;
    let a = group_for(&user.username, &admin).await?;
    let b = generators::group_with_roles(
        &GroupRoles::default().monitor(&user.username),
        GroupAllowed::default(),
        &admin,
    )
    .await?;
    let hidden = generators::groups(1, &admin).await?.remove(0).name;
    let spec = generators::gen_agentic_spec();
    let (visible_block, _) = generators::tool(&[&a, &b], "1.0.0", &spec, &admin).await?;
    let (hidden_block, _) = generators::tool(&[&a, &hidden], "1.0.0", &spec, &admin).await?;
    // a visible group that blocks the edit is named
    let resp = user
        .client
        .entities
        .update(visible_block.id, EntityUpdate::default().description("x"))
        .await;
    fail!(resp, 401);
    let msg = resp.unwrap_err().msg().unwrap();
    assert!(msg.contains("you don't have permission to edit"), "{msg}");
    assert!(msg.contains(&b), "{msg}");
    // a hidden group that blocks the edit is neither named nor counted
    let resp = user
        .client
        .entities
        .update(hidden_block.id, EntityUpdate::default().description("x"))
        .await;
    fail!(resp, 401);
    let msg = resp.unwrap_err().msg().unwrap();
    assert!(msg.contains("you don't have permission to edit"), "{msg}");
    assert!(!msg.contains(&hidden), "{msg}");
    assert!(!msg.contains(" in "), "{msg}");
    // a group that disallows entities blocks the edit and is named
    let (allowed, _) = generators::tool(&[&a, &b], "1.0.0", &spec, &admin).await?;
    let b_user = plain_user(&admin).await?;
    let c = group_for(&b_user.username, &admin).await?;
    let d = group_for(&b_user.username, &admin).await?;
    let (flagged, _) = generators::tool(&[&c, &d], "1.0.0", &spec, &admin).await?;
    let disable = GroupUpdate {
        allowed: GroupAllowedUpdate::default().disable_entities(),
        ..GroupUpdate::default()
    };
    admin.groups.update(&d, &disable).await?;
    let resp = b_user
        .client
        .entities
        .update(flagged.id, EntityUpdate::default().description("x"))
        .await;
    fail!(resp, 401);
    assert!(resp.unwrap_err().msg().unwrap().contains(&d));
    // an allowed edit rewrites the row in every group
    admin
        .entities
        .update(allowed.id, EntityUpdate::default().description("rewritten"))
        .await?;
    let rows = test_utilities::entity_rows(allowed.id).await?;
    is!(rows.len(), 2);
    let refreshed = admin.entities.get(allowed.id).await?;
    is!(refreshed.description, Some("rewritten".to_owned()));
    Ok(())
}

/// Widening a harness tool needs the network policies its instances name in each added group
#[tokio::test]
async fn harness_network_policies_on_widening() -> Result<(), Error> {
    // a policy in minerva and a harness in minerva whose instance names it
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let callisto = generators::moon_user(&admin, "callisto", &suffix).await?;
    let roles = GroupRoles::default().user(&callisto.username);
    let minerva = generators::god_group(&admin, "minerva", &suffix, &roles).await?;
    let vulcan = generators::god_group(&admin, "vulcan", &suffix, &roles).await?;
    let policy_name = format!("np{}", generators::gen_string(10).to_lowercase());
    let mut policy = generators::gen_network_policy(std::slice::from_ref(&minerva));
    policy.name = policy_name.clone();
    admin.network_policies.create(policy).await?;
    let mut spec = generators::gen_harness_spec();
    if let ToolSpec::Harness(harness) = &mut spec
        && let WorkloadEnvironment::K8s(env) = &mut harness.workload.environment
    {
        env.network_policies.insert(policy_name.clone());
    }
    let (tool, instance) = generators::tool(&[&minerva], "1.0.0", &spec, &callisto.client).await?;
    // widening the harness to a group without the policy is refused, naming the policy
    let resp = callisto
        .client
        .entities
        .update(tool.id, EntityUpdate::default().group(&vulcan))
        .await;
    fail!(resp, 400);
    let msg = resp.unwrap_err().msg().unwrap();
    assert!(
        msg.contains(&policy_name) && msg.contains(vulcan.as_str()),
        "{msg}"
    );
    is!(
        stored_groups(instance.id).await?,
        sorted(std::slice::from_ref(&minerva))
    );
    Ok(())
}

/// Widening a tool needs the network policies its instances name in each added group
#[tokio::test]
async fn network_policies_on_widening() -> Result<(), Error> {
    // a policy in a and an Image tool in a whose instance names it
    let admin = test_utilities::admin_client().await?;
    let groups = generators::groups(5, &admin).await?;
    let names = groups
        .iter()
        .map(|group| group.name.clone())
        .collect::<Vec<String>>();
    let (a, same, differs, missing, admin_add) =
        (&names[0], &names[1], &names[2], &names[3], &names[4]);
    let policy_name = format!("np{}", generators::gen_string(10).to_lowercase());
    let mut policy = generators::gen_network_policy(std::slice::from_ref(a));
    policy.name = policy_name.clone();
    admin.network_policies.create(policy.clone()).await?;
    // a same-named policy with the same rules in one group and different rules in another
    let identical = NetworkPolicyRequest {
        groups: vec![same.clone()],
        ..policy.clone()
    };
    admin.network_policies.create(identical).await?;
    let mut different = generators::gen_network_policy(std::slice::from_ref(differs));
    different.name = policy_name.clone();
    different.ingress = Some(Vec::new());
    different.egress = Some(Vec::new());
    admin.network_policies.create(different).await?;
    // the tool's instance names the policy
    let mut spec = generators::gen_image_spec();
    if let ToolSpec::Image(workload) = &mut spec
        && let WorkloadEnvironment::K8s(env) = &mut workload.environment
    {
        env.network_policies.insert(policy_name.clone());
    }
    let user = plain_user(&admin).await?;
    for group in [a, same, differs, missing] {
        admin
            .groups
            .update(
                group,
                &GroupUpdate::default()
                    .users(thorium::models::GroupUsersUpdate::default().direct_add(&user.username)),
            )
            .await?;
    }
    let (tool, instance) = generators::tool(&[a], "1.0.0", &spec, &user.client).await?;
    // a group with an identical same-named policy is accepted
    user.client
        .entities
        .update(tool.id, EntityUpdate::default().group(same))
        .await?;
    // a group whose same-named policy has different rules is refused, naming the policy
    let resp = user
        .client
        .entities
        .update(tool.id, EntityUpdate::default().group(differs))
        .await;
    fail!(resp, 400);
    let msg = resp.unwrap_err().msg().unwrap();
    assert!(
        msg.contains(&policy_name) && msg.contains(differs.as_str()),
        "{msg}"
    );
    // a non-admin widening to a group without the policy gets the exact message
    let resp = user
        .client
        .entities
        .update(tool.id, EntityUpdate::default().group(missing))
        .await;
    fail!(resp, 400);
    let msg = resp.unwrap_err().msg().unwrap();
    let msg: serde_json::Value = serde_json::from_str(&msg)?;
    is!(
        msg["error"].as_str().unwrap_or_default().to_owned(),
        format!(
            "network policy {policy_name} is not available in group {missing}; ask an admin to add it, or remove {missing}"
        )
    );
    is!(
        stored_groups(instance.id).await?,
        sorted(&[a.clone(), same.clone()])
    );
    // an admin's widening adds the group to the policy
    admin
        .entities
        .update(tool.id, EntityUpdate::default().group(admin_add))
        .await?;
    let listed = admin
        .network_policies
        .list_details(&thorium::models::NetworkPolicyListOpts::default().group(admin_add.clone()))
        .await?;
    let policy = listed
        .data
        .into_iter()
        .find(|policy| policy.name == policy_name)
        .expect("the policy wasn't added to the group");
    // the policy the tool's group resolves is the one that gained the group
    assert!(policy.groups.contains(admin_add), "{:?}", policy.groups);
    assert!(policy.groups.contains(a), "{:?}", policy.groups);
    assert!(stored_groups(instance.id).await?.contains(admin_add));
    Ok(())
}

/// Widening a tool with a long history returns only once every instance holds the group
#[tokio::test]
async fn widen_long_history() -> Result<(), Error> {
    // create a tool with 200 instances
    let admin = test_utilities::admin_client().await?;
    let groups = generators::groups(2, &admin).await?;
    let (a, b) = (groups[0].name.clone(), groups[1].name.clone());
    let spec = generators::gen_agentic_spec();
    let (tool, _) = generators::tool(&[&a], "0.0.0", &spec, &admin).await?;
    for index in 1..200 {
        publish(
            &admin,
            tool.id,
            &format!("0.0.{index}"),
            &with_marker(&spec, &format!("v{index}")),
        )
        .await?;
    }
    let instances = listed(&admin, tool.id).await?;
    is!(instances.len(), 200);
    // widen the tool to b
    admin
        .entities
        .update(tool.id, EntityUpdate::default().group(&b))
        .await?;
    // right after the response every instance and the tool hold b
    let mut in_b = usize::from(stored_groups(tool.id).await?.contains(&b));
    for id in &instances {
        in_b += usize::from(stored_groups(*id).await?.contains(&b));
    }
    is!(in_b, 201);
    // each instance's get lists b
    for id in instances.iter().take(5) {
        assert!(admin.entities.get(*id).await?.groups.contains(&b));
    }
    Ok(())
}

/// Creating a tool needs a group and edit rights with entities allowed in every requested group
#[tokio::test]
async fn contained_create_rules() -> Result<(), Error> {
    // a user who edits a, monitors b, edits a group that disallows entities, and isn't in c
    let admin = test_utilities::admin_client().await?;
    let user = plain_user(&admin).await?;
    let a = group_for(&user.username, &admin).await?;
    let monitored = generators::group_with_roles(
        &GroupRoles::default().monitor(&user.username),
        GroupAllowed::default(),
        &admin,
    )
    .await?;
    let disallowed = generators::group_with_roles(
        &GroupRoles::default().user(&user.username),
        GroupAllowed {
            entities: false,
            ..GroupAllowed::default()
        },
        &admin,
    )
    .await?;
    let outside = generators::groups(1, &admin).await?.remove(0).name;
    let spec = generators::gen_agentic_spec();
    // no groups is a bad request
    let mut req = generators::gen_tool(&[], "1.0.0", &spec);
    req.groups.clear();
    let resp = user.client.entities.create(req).await;
    fail!(resp, 400);
    // each group the user can't create in is refused
    for group in [&outside, &monitored, &disallowed] {
        let req = generators::gen_tool(&[&a, group], "1.0.0", &spec);
        let name = req.name.clone();
        let resp = user.client.entities.create(req).await;
        fail!(resp, 401);
        // nothing was written in a
        assert!(
            names_in(&admin, &a)
                .await?
                .iter()
                .all(|listed| !listed.starts_with(&name))
        );
    }
    Ok(())
}

/// Adding groups to a tool needs edit rights with entities allowed in each added group
#[tokio::test]
async fn contained_group_adds() -> Result<(), Error> {
    // a plain user edits a, monitors m, and edits a group that disallows entities
    let admin = test_utilities::admin_client().await?;
    let user = plain_user(&admin).await?;
    let manager = plain_user(&admin).await?;
    let a = generators::group_with_roles(
        &GroupRoles::default()
            .user(&user.username)
            .manager(&manager.username),
        GroupAllowed::default(),
        &admin,
    )
    .await?;
    let b = generators::group_with_roles(
        &GroupRoles::default().manager(&manager.username),
        GroupAllowed::default(),
        &admin,
    )
    .await?;
    let monitored = generators::group_with_roles(
        &GroupRoles::default().monitor(&user.username),
        GroupAllowed::default(),
        &admin,
    )
    .await?;
    let disallowed = generators::group_with_roles(
        &GroupRoles::default().user(&user.username),
        GroupAllowed {
            entities: false,
            ..GroupAllowed::default()
        },
        &admin,
    )
    .await?;
    // a tool in a with two instances
    let spec = generators::gen_agentic_spec();
    let (tool, _) = generators::tool(&[&a], "1.0.0", &spec, &admin).await?;
    publish(&admin, tool.id, "1.1.0", &with_marker(&spec, "v2")).await?;
    let instances = listed(&admin, tool.id).await?;
    // adding a group the user can't edit or that disallows entities is refused
    for group in [&monitored, &disallowed] {
        fail!(
            user.client
                .entities
                .update(tool.id, EntityUpdate::default().group(group))
                .await,
            401
        );
    }
    // the same is refused through a scoped token limited to a
    let scoped = user
        .client
        .users
        .create_scoped_token(
            &ScopedTokenRequest::new(format!("s{}", generators::gen_string(10))).group(&a),
        )
        .await?;
    let scoped_client = Thorium::build(&admin.host)
        .token(&scoped.token)
        .build()
        .await?;
    fail!(
        scoped_client
            .entities
            .update(tool.id, EntityUpdate::default().group(&b))
            .await,
        401
    );
    // neither the tool nor any instance gained a group
    is!(stored_groups(tool.id).await?, vec![a.clone()]);
    for id in &instances {
        is!(stored_groups(*id).await?, vec![a.clone()]);
    }
    Ok(())
}

/// Removing a tool's groups needs the delete rule and cascades, and hidden groups stay untouched
#[tokio::test]
async fn contained_group_removals() -> Result<(), Error> {
    // a plain user edits a and a manager manages a and b
    let admin = test_utilities::admin_client().await?;
    let user = plain_user(&admin).await?;
    let manager = plain_user(&admin).await?;
    let a = generators::group_with_roles(
        &GroupRoles::default()
            .user(&user.username)
            .manager(&manager.username),
        GroupAllowed::default(),
        &admin,
    )
    .await?;
    let b = generators::group_with_roles(
        &GroupRoles::default()
            .manager(&manager.username)
            .user(&user.username),
        GroupAllowed::default(),
        &admin,
    )
    .await?;
    let hidden = generators::groups(1, &admin).await?.remove(0).name;
    // a tool in a with two instances
    let spec = generators::gen_agentic_spec();
    let (tool, _) = generators::tool(&[&a], "1.0.0", &spec, &admin).await?;
    publish(&admin, tool.id, "1.1.0", &with_marker(&spec, "v2")).await?;
    let instances = listed(&admin, tool.id).await?;
    // adding a group the tool already holds but the user can't see leaves its rows unchanged
    admin
        .entities
        .update(tool.id, EntityUpdate::default().group(&hidden))
        .await?;
    let before = test_utilities::entity_rows(tool.id).await?;
    fail!(
        user.client
            .entities
            .update(tool.id, EntityUpdate::default().group(&hidden))
            .await,
        401
    );
    is!(test_utilities::entity_rows(tool.id).await?, before);
    admin
        .entities
        .update(tool.id, EntityUpdate::default().remove_group(&hidden))
        .await?;
    // removing a group as a plain user who isn't the submitter is refused
    admin
        .entities
        .update(tool.id, EntityUpdate::default().group(&b))
        .await?;
    fail!(
        user.client
            .entities
            .update(tool.id, EntityUpdate::default().remove_group(&a))
            .await,
        401
    );
    // a manager of both groups removes it, cascading to every instance
    manager
        .client
        .entities
        .update(tool.id, EntityUpdate::default().remove_group(&a))
        .await?;
    is!(stored_groups(tool.id).await?, vec![b.clone()]);
    for id in &instances {
        is!(stored_groups(*id).await?, vec![b.clone()]);
    }
    Ok(())
}

/// A user in none of a tool's groups can't see it or its instances anywhere
#[tokio::test]
async fn visibility() -> Result<(), Error> {
    // a tool in a and a user outside it
    let admin = test_utilities::admin_client().await?;
    let member = plain_user(&admin).await?;
    let outsider = plain_user(&admin).await?;
    let a = group_for(&member.username, &admin).await?;
    let other = group_for(&outsider.username, &admin).await?;
    let spec = generators::gen_agentic_spec();
    let (tool, instance) = generators::tool(&[&a], "1.0.0", &spec, &admin).await?;
    // the outsider gets 404 for both and its route
    let resp = outsider.client.entities.get(tool.id).await;
    fail!(resp, 404);
    let resp = outsider.client.entities.get(instance.id).await;
    fail!(resp, 404);
    let resp = outsider.client.entities.tool_instances(tool.id).await;
    fail!(resp, 404);
    // neither appears in the outsider's lists or tag listings
    let names = names_in(&outsider.client, &other).await?;
    assert!(!names.contains(&tool.name) && !names.contains(&instance.name));
    let tagged = outsider
        .client
        .entities
        .list(
            &EntityListOpts::default()
                .groups(vec![other.clone()])
                .tag("ToolId", tool.id.to_string())
                .limit(10),
        )
        .await?;
    assert_eq!(tagged.data.len(), 0);
    // a member of a sees both
    member.client.entities.get(tool.id).await?;
    member.client.entities.get(instance.id).await?;
    is!(listed(&member.client, tool.id).await?, vec![instance.id]);
    Ok(())
}

/// Workload security checks run when an instance is published
#[tokio::test]
async fn publish_time_workload_checks() -> Result<(), Error> {
    // a non-developer user and a group
    let admin = test_utilities::admin_client().await?;
    let user = plain_user(&admin).await?;
    let group = group_for(&user.username, &admin).await?;
    // a non-default security context is refused for a non-admin and accepted for an admin
    let mut privileged = generators::gen_image_spec();
    if let ToolSpec::Image(workload) = &mut privileged
        && let WorkloadEnvironment::K8s(env) = &mut workload.environment
    {
        env.security_context = SecurityContext {
            user: Some(0),
            ..SecurityContext::default()
        };
    }
    let req = generators::gen_tool(&[&group], "1.0.0", &privileged);
    let name = req.name.clone();
    let resp = user.client.entities.create(req).await;
    fail!(resp, 400);
    assert!(
        names_in(&admin, &group)
            .await?
            .iter()
            .all(|listed| !listed.starts_with(&name))
    );
    admin
        .entities
        .create(generators::gen_tool(&[&group], "1.0.0", &privileged))
        .await?;
    // a host path volume off the whitelist is refused
    let mut volume_spec = generators::gen_image_spec();
    if let ToolSpec::Image(workload) = &mut volume_spec
        && let WorkloadEnvironment::K8s(env) = &mut workload.environment
    {
        env.volumes.push(Volume {
            name: "hostvol".into(),
            archetype: VolumeTypes::HostPath,
            mount_path: "/mnt".into(),
            host_path: Some(thorium::models::HostPath {
                path: format!("/not/whitelisted/{}", generators::gen_string(8)),
                path_type: None,
            }),
            ..Volume::default()
        });
    }
    fail!(
        user.client
            .entities
            .create(generators::gen_tool(&[&group], "1.0.0", &volume_spec))
            .await,
        400
    );
    // a volume on the whitelist publishes
    let path = format!("/whitelisted/{}", generators::gen_string(12).to_lowercase());
    let no_scan = SystemSettingsUpdateParams { scan: false };
    let add = SystemSettingsUpdate::default()
        .host_path_whitelist(HostPathWhitelistUpdate::default().add_path(&path));
    admin.system.update_settings(&add, &no_scan).await?;
    let mut allowed_spec = volume_spec.clone();
    if let ToolSpec::Image(workload) = &mut allowed_spec
        && let WorkloadEnvironment::K8s(env) = &mut workload.environment
        && let Some(host_path) = env.volumes[0].host_path.as_mut()
    {
        host_path.path.clone_from(&path);
    }
    let (tool, instance) =
        generators::tool(&[&group], "1.0.0", &allowed_spec, &user.client).await?;
    // narrowing the whitelist afterwards doesn't revoke the published instance
    let remove = SystemSettingsUpdate::default()
        .host_path_whitelist(HostPathWhitelistUpdate::default().remove_path(&path));
    admin.system.update_settings(&remove, &no_scan).await?;
    let tool = admin.entities.get(tool.id).await?;
    is!(tool_meta(&tool).latest, Some(instance.id));
    let still = admin.entities.get(instance.id).await?;
    is!(instance_meta(&still).spec.clone(), allowed_spec);
    Ok(())
}

/// A tool create writes its instance first, so a failure after it leaves only an orphan
#[tokio::test]
async fn write_order() -> Result<(), Error> {
    // fail the next create of a named tool after its instance is written
    let admin = test_utilities::admin_client().await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let spec = generators::gen_agentic_spec();
    let req = generators::gen_tool(&[&group], "1.0.0", &spec);
    test_utilities::faults::fail_after_instance_write(&req.name);
    // the create fails with a 500
    let resp = admin.entities.create(req.clone()).await;
    fail!(resp, 500);
    // the orphaned instance exists but no tool lists it
    let orphans = admin
        .entities
        .list(
            &EntityListOpts::default()
                .groups(vec![group.clone()])
                .tag("ToolSpecHash", ThorDigest::tool_spec(&spec, None)?)
                .limit(10),
        )
        .await?;
    is!(orphans.data.len(), 1);
    let orphan = admin.entities.get(orphans.data[0].id).await?;
    let orphan_tool = instance_meta(&orphan).tool;
    let resp = admin.entities.get(orphan_tool).await;
    fail!(resp, 404);
    assert!(!names_in(&admin, &group).await?.contains(&req.name));
    // the same create retried succeeds with a new instance
    let resp = admin.entities.create(req).await?;
    let instance = resp.tool_instance.unwrap();
    assert_ne!(instance, orphan.id);
    is!(listed(&admin, resp.id).await?, vec![instance]);
    Ok(())
}

/// Reading a tool or publishing an instance of it reads each instance's entity row but not its
/// tags, since the summaries and the latest instance come from the rows alone
#[tokio::test]
async fn tool_reads_do_not_read_instance_tags() -> Result<(), Error> {
    // a tool with four instances
    let admin = test_utilities::admin_client().await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let spec = generators::gen_image_spec();
    let (tool, first) = generators::tool(&[&group], "1.0.0", &spec, &admin).await?;
    for version in ["1.0.1", "1.0.2", "1.0.3"] {
        // each version gets its own spec, since republishing the latest spec is refused
        let update = generators::gen_tool_publish(version, &generators::gen_image_spec());
        admin.entities.update(tool.id, update).await?;
    }
    let instances = admin.entities.tool_instances(tool.id).await?;
    is!(instances.len(), 4);
    // note how often each instance's tags have been read so far
    let tag_reads = |ids: &[Uuid]| -> Vec<u64> {
        ids.iter()
            .map(|id| test_utilities::faults::tag_reads(*id))
            .collect()
    };
    let ids: Vec<Uuid> = instances.iter().map(|instance| instance.id).collect();
    let before = tag_reads(&ids);
    // reading the tool and listing its instances reads no instance's tags
    admin.entities.get(tool.id).await?;
    admin.entities.tool_instances(tool.id).await?;
    is!(tag_reads(&ids), before);
    // publishing another instance reads none of the existing instances' tags either
    let update = generators::gen_tool_publish("1.0.4", &generators::gen_image_spec());
    admin.entities.update(tool.id, update).await?;
    is!(tag_reads(&ids), before);
    // the instances still list newest first and the first is still readable with its tags
    is!(admin.entities.tool_instances(tool.id).await?.len(), 5);
    is!(admin.entities.get(first.id).await?.id, first.id);
    Ok(())
}

/// The instances route needs a token and hides tools outside the user's groups
#[tokio::test]
async fn instances_route() -> Result<(), Error> {
    // a tool in a group the user isn't in, while the user has a group of their own
    let admin = test_utilities::admin_client().await?;
    let user = plain_user(&admin).await?;
    group_for(&user.username, &admin).await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let (tool, _) =
        generators::tool(&[&group], "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    // no token is a 401
    let resp = reqwest::Client::new()
        .get(format!(
            "{}/api/entities/tools/{}/instances",
            admin.host, tool.id
        ))
        .send()
        .await?;
    is!(resp.status().as_u16(), 401);
    // an unknown id and a user outside the tool's groups are 404
    let resp = admin.entities.tool_instances(Uuid::new_v4()).await;
    fail!(resp, 404);
    let resp = user.client.entities.tool_instances(tool.id).await;
    fail!(resp, 404);
    // a non-tool entity has no instances
    let (_, device) = generators::entity(
        generators::gen_entity(&group, generators::gen_device_meta(Vec::new())),
        &admin,
    )
    .await?;
    let resp = admin.entities.tool_instances(device.id).await;
    fail!(resp, 404);
    Ok(())
}

/// A user publishing in a and b without rights in b writes nothing
#[tokio::test]
async fn publish_without_rights() -> Result<(), Error> {
    // a user who edits a only, and a group that disallows entities
    let admin = test_utilities::admin_client().await?;
    let user = plain_user(&admin).await?;
    let a = group_for(&user.username, &admin).await?;
    let b = generators::groups(1, &admin).await?.remove(0).name;
    let c = generators::group_with_roles(
        &GroupRoles::default().user(&user.username),
        GroupAllowed {
            entities: false,
            ..GroupAllowed::default()
        },
        &admin,
    )
    .await?;
    let spec = generators::gen_agentic_spec();
    for other in [&b, &c] {
        // the create is refused
        let req = generators::gen_tool(&[&a, other], "1.0.0", &spec);
        let name = req.name.clone();
        let resp = user.client.entities.create(req).await;
        fail!(resp, 401);
        // no root or instance was written
        assert!(
            names_in(&admin, &a)
                .await?
                .iter()
                .all(|listed| !listed.starts_with(&name))
        );
        let instances = admin
            .entities
            .list(
                &EntityListOpts::default()
                    .groups(vec![a.clone()])
                    .tag("ToolSpecHash", ThorDigest::tool_spec(&spec, None)?)
                    .limit(10),
            )
            .await?;
        assert_eq!(instances.data.len(), 0);
    }
    Ok(())
}

/// Wait long enough for requests sent while a tool's lock is held to be waiting on it
async fn let_requests_queue() {
    // requests retry the lock every few milliseconds, so a second is plenty
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
}

/// Edits and group changes of a tool wait for a publish holding its lock, and a publish sees
/// the groups and fields they wrote
#[tokio::test]
// one scenario checks a removal and an addition racing a publish
#[allow(clippy::too_many_lines)]
async fn tool_writes_serialize() -> Result<(), Error> {
    // a tool in a and b
    let admin = test_utilities::admin_client().await?;
    let groups = generators::groups(3, &admin).await?;
    let (a, b, c) = (
        groups[0].name.clone(),
        groups[1].name.clone(),
        groups[2].name.clone(),
    );
    let spec = generators::gen_agentic_spec();
    let (tool, first) = generators::tool(&[&a, &b], "1.0.0", &spec, &admin).await?;
    // while another request holds the tool's lock, publish a version, remove b, and edit it
    test_utilities::hold_instance_lock(tool.id).await?;
    // queue a publish of a second version first
    let publish_v2 = tokio::spawn({
        let (admin, groups, spec) = (admin.clone(), vec![a.clone()], with_marker(&spec, "v2"));
        async move { direct_instance(&admin, tool.id, &groups, "2.0.0", &spec).await }
    });
    let_requests_queue().await;
    // then queue the removal of b and the edit behind it
    let remove_b = tokio::spawn({
        let (admin, b) = (admin.clone(), b.clone());
        async move {
            admin
                .entities
                .update(tool.id, EntityUpdate::default().remove_group(&b))
                .await
                .map(|_| ())
        }
    });
    let describe = tokio::spawn({
        let admin = admin.clone();
        async move {
            admin
                .entities
                .update(tool.id, EntityUpdate::default().description("new"))
                .await
                .map(|_| ())
        }
    });
    let_requests_queue().await;
    // free the lock and wait for every queued write
    test_utilities::release_instance_lock(tool.id).await?;
    let second = publish_v2
        .await
        .map_err(|err| Error::new(err.to_string()))??;
    remove_b
        .await
        .map_err(|err| Error::new(err.to_string()))??;
    describe
        .await
        .map_err(|err| Error::new(err.to_string()))??;
    // the tool and both instances are only in a, and the edit and the new latest both stuck
    is!(stored_groups(tool.id).await?, vec![a.clone()]);
    is!(stored_groups(first.id).await?, vec![a.clone()]);
    is!(stored_groups(second).await?, vec![a.clone()]);
    let read = admin.entities.get(tool.id).await?;
    is!(read.description.as_deref(), Some("new"));
    is!(tool_meta(&read).latest, Some(second));
    // while the lock is held again, publish a version and add c
    test_utilities::hold_instance_lock(tool.id).await?;
    // queue a publish of a third version first
    let publish_v3 = tokio::spawn({
        let (admin, groups, spec) = (admin.clone(), vec![a.clone()], with_marker(&spec, "v3"));
        async move { direct_instance(&admin, tool.id, &groups, "3.0.0", &spec).await }
    });
    let_requests_queue().await;
    // then queue the addition of c behind it
    let add_c = tokio::spawn({
        let (admin, c) = (admin.clone(), c.clone());
        async move {
            admin
                .entities
                .update(tool.id, EntityUpdate::default().group(&c))
                .await
                .map(|_| ())
        }
    });
    let_requests_queue().await;
    // free the lock and wait for both queued writes
    test_utilities::release_instance_lock(tool.id).await?;
    let third = publish_v3
        .await
        .map_err(|err| Error::new(err.to_string()))??;
    add_c.await.map_err(|err| Error::new(err.to_string()))??;
    // the tool and every instance, including the new one, are in a and c
    let both = sorted(&[a.clone(), c.clone()]);
    is!(stored_groups(tool.id).await?, both.clone());
    for id in [first.id, second, third] {
        is!(stored_groups(id).await?, both.clone());
    }
    is!(
        tool_meta(&admin.entities.get(tool.id).await?).latest,
        Some(third)
    );
    Ok(())
}

/// Deleting or deprecating an instance while its tool's lock stays held is refused without
/// changing anything, so latest never names a deleted or deprecated instance
#[tokio::test]
async fn instance_changes_wait_for_lock() -> Result<(), Error> {
    // two tools, each with two versions
    let admin = test_utilities::admin_client().await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let spec = generators::gen_agentic_spec();
    let (deleted, _) = generators::tool(&[&group], "1.0.0", &spec, &admin).await?;
    publish(&admin, deleted.id, "2.0.0", &with_marker(&spec, "v2")).await?;
    let (deprecated, _) = generators::tool(&[&group], "1.0.0", &spec, &admin).await?;
    publish(&admin, deprecated.id, "2.0.0", &with_marker(&spec, "v2")).await?;
    let deleted_v2 = listed(&admin, deleted.id).await?[0];
    let deprecated_v2 = listed(&admin, deprecated.id).await?[0];
    // another request holds both locks for longer than a request waits
    test_utilities::hold_instance_lock(deleted.id).await?;
    test_utilities::hold_instance_lock(deprecated.id).await?;
    // delete one tool's second version and deprecate the other's while the locks are held
    let (delete, deprecate) = tokio::join!(
        admin.entities.delete(deleted_v2),
        admin
            .entities
            .update(deprecated_v2, generators::gen_tool_deprecate(true))
    );
    // free both locks
    test_utilities::release_instance_lock(deleted.id).await?;
    test_utilities::release_instance_lock(deprecated.id).await?;
    // both changes were refused and left the instances and latest alone
    fail!(delete, 409);
    fail!(deprecate, 409);
    admin.entities.get(deleted_v2).await?;
    is!(
        tool_meta(&admin.entities.get(deleted.id).await?).latest,
        Some(deleted_v2)
    );
    is!(
        instance_meta(&admin.entities.get(deprecated_v2).await?).deprecated,
        false
    );
    is!(
        tool_meta(&admin.entities.get(deprecated.id).await?).latest,
        Some(deprecated_v2)
    );
    // with the lock free both changes go through and move latest
    admin.entities.delete(deleted_v2).await?;
    assert_ne!(
        tool_meta(&admin.entities.get(deleted.id).await?).latest,
        Some(deleted_v2)
    );
    admin
        .entities
        .update(deprecated_v2, generators::gen_tool_deprecate(true))
        .await?;
    assert_ne!(
        tool_meta(&admin.entities.get(deprecated.id).await?).latest,
        Some(deprecated_v2)
    );
    Ok(())
}

/// Wait for a request that must not wait on a tool's lock
///
/// # Arguments
///
/// * `request` - The request to wait for
async fn without_waiting<T, F>(request: F) -> Result<T, Error>
where
    F: std::future::Future<Output = T>,
{
    // a request blocked on the lock would wait far longer than this
    tokio::time::timeout(std::time::Duration::from_secs(10), request)
        .await
        .map_err(|_| Error::new("the request waited on the tool's lock"))
}

/// A slow update's form is read before the tool's lock is taken, so other writes to the tool go
/// through while it arrives and the slow update applies to the tool as they left it
#[tokio::test]
async fn slow_update_forms_hold_no_lock() -> Result<(), Error> {
    // a tool in a and b
    let admin = test_utilities::admin_client().await?;
    let groups = generators::groups(2, &admin).await?;
    let (a, b) = (groups[0].name.clone(), groups[1].name.clone());
    let spec = generators::gen_agentic_spec();
    let (tool, first) = generators::tool(&[&a, &b], "1.0.0", &spec, &admin).await?;
    // start an update of the description whose value arrives only once the test sends it
    let (send, wait) = tokio::sync::oneshot::channel::<()>();
    let value = futures::stream::once(async move {
        // hold the value back until the test releases it
        let _ = wait.await;
        Ok::<Vec<u8>, std::io::Error>(b"slow".to_vec())
    });
    let form = Form::new().part(
        "description",
        reqwest::multipart::Part::stream(reqwest::Body::wrap_stream(value)),
    );
    // send it as another admin in the background
    let sender = generators::user_with_role(&admin, UserRole::Admin).await?;
    let slow = tokio::spawn({
        let url = format!("{}/api/entities/{}", admin.host, tool.id);
        let auth = sender.auth_header();
        async move {
            reqwest::Client::new()
                .patch(url)
                .header("authorization", auth)
                .multipart(form)
                .send()
                .await
                .map(|resp| resp.status().as_u16())
        }
    });
    // let the update reach the API and wait on its form
    let_requests_queue().await;
    // removing b goes through while the slow update's form is still arriving
    without_waiting(
        admin
            .entities
            .update(tool.id, EntityUpdate::default().remove_group(&b)),
    )
    .await??;
    // finish the slow update
    send.send(())
        .map_err(|()| Error::new("the slow update stopped early"))?;
    let status = slow.await.map_err(|err| Error::new(err.to_string()))??;
    is!(status, 204);
    // the slow update kept the removal and set the description
    is!(stored_groups(tool.id).await?, vec![a.clone()]);
    is!(stored_groups(first.id).await?, vec![a.clone()]);
    is!(
        admin.entities.get(tool.id).await?.description.as_deref(),
        Some("slow")
    );
    Ok(())
}

/// A user who can only view a tool is refused at once, even while another request holds the
/// tool's lock
#[tokio::test]
async fn viewers_refused_without_waiting() -> Result<(), Error> {
    // a tool in a group where the user is only a monitor
    let admin = test_utilities::admin_client().await?;
    let user = plain_user(&admin).await?;
    let group = generators::group_with_roles(
        &GroupRoles::default().monitor(&user.username),
        GroupAllowed::default(),
        &admin,
    )
    .await?;
    let spec = generators::gen_agentic_spec();
    let (tool, first) = generators::tool(&[&group], "1.0.0", &spec, &admin).await?;
    // while another request holds the tool's lock, the user edits the tool, deletes its
    // instance, and creates a new instance
    let (groups, v2) = (vec![group.clone()], with_marker(&spec, "v2"));
    test_utilities::hold_instance_lock(tool.id).await?;
    let (update, delete, create) = tokio::join!(
        without_waiting(
            user.client
                .entities
                .update(tool.id, EntityUpdate::default().description("viewer"))
        ),
        without_waiting(user.client.entities.delete(first.id)),
        without_waiting(direct_instance(
            &user.client,
            tool.id,
            &groups,
            "2.0.0",
            &v2,
        )),
    );
    test_utilities::release_instance_lock(tool.id).await?;
    // each was refused as unauthorized without waiting for the lock
    let (update, delete, create) = (update?, delete?, create?);
    fail!(update, 401);
    fail!(delete, 401);
    fail!(create, 401);
    // nothing changed
    let read = admin.entities.get(tool.id).await?;
    is!(read.description, tool.description);
    is!(listed(&admin, tool.id).await?, vec![first.id]);
    Ok(())
}

/// An update queued behind a tool's lock applies to the tool as it is once the lock is free,
/// keeping a change written while it waited
#[tokio::test]
async fn queued_updates_reread_the_tool() -> Result<(), Error> {
    // a tool with two versions, so its latest is the second
    let admin = test_utilities::admin_client().await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let spec = generators::gen_agentic_spec();
    let (tool, first) = generators::tool(&[&group], "1.0.0", &spec, &admin).await?;
    publish(&admin, tool.id, "2.0.0", &with_marker(&spec, "v2")).await?;
    // get the second version's id
    let second = tool_meta(&admin.entities.get(tool.id).await?)
        .latest
        .ok_or_else(|| Error::new("the tool has no latest"))?;
    assert_ne!(second, first.id);
    // while another request holds the tool's lock, queue an edit of its description
    test_utilities::hold_instance_lock(tool.id).await?;
    let describe = tokio::spawn({
        let admin = admin.clone();
        async move {
            admin
                .entities
                .update(tool.id, EntityUpdate::default().description("queued"))
                .await
                .map(|_| ())
        }
    });
    // let the edit start waiting on the lock
    let_requests_queue().await;
    // move the tool's latest back to its first version straight in the database
    let mut data: serde_json::Value = serde_json::from_str(
        &test_utilities::entity_rows(tool.id)
            .await?
            .first()
            .ok_or_else(|| Error::new("the tool has no rows"))?
            .1,
    )?;
    data["Tool"]["latest"] = json!(first.id);
    test_utilities::set_entity_kind_data(tool.id, &data.to_string()).await?;
    // let the edit run
    test_utilities::release_instance_lock(tool.id).await?;
    describe
        .await
        .map_err(|err| Error::new(err.to_string()))??;
    // the edit stuck without writing the latest it would have read before waiting
    let read = admin.entities.get(tool.id).await?;
    is!(read.description.as_deref(), Some("queued"));
    is!(tool_meta(&read).latest, Some(first.id));
    Ok(())
}

/// Send an update of an entity with one PNG image, returning the status code
///
/// # Arguments
///
/// * `host` - The API's url
/// * `auth` - The authorization header of the user sending the update
/// * `id` - The entity to update
/// * `name` - The file name of the image
async fn send_image(host: &str, auth: &str, id: Uuid, name: &str) -> Result<u16, Error> {
    // build a form with only the image
    let form = Form::new().part(
        "image",
        reqwest::multipart::Part::bytes(b"image".to_vec())
            .file_name(name.to_owned())
            .mime_str("image/png")?,
    );
    // send it with the user's token
    let resp = reqwest::Client::new()
        .patch(format!("{host}/api/entities/{id}"))
        .header("authorization", auth)
        .multipart(form)
        .send()
        .await?;
    Ok(resp.status().as_u16())
}

/// Download an entity's image, returning the status code
///
/// # Arguments
///
/// * `host` - The API's url
/// * `auth` - The authorization header of the user downloading the image
/// * `id` - The entity whose image to download
async fn image_status(host: &str, auth: &str, id: Uuid) -> Result<u16, Error> {
    // request the image with the user's token
    let resp = reqwest::Client::new()
        .get(format!("{host}/api/entities/{id}/image"))
        .header("authorization", auth)
        .send()
        .await?;
    Ok(resp.status().as_u16())
}

/// An update refused while it waits on a tool's lock keeps an image another request gave the
/// tool under the same name meanwhile
#[tokio::test]
async fn refused_queued_updates_keep_current_image() -> Result<(), Error> {
    // a tool with no image
    let admin = test_utilities::admin_client().await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let spec = generators::gen_agentic_spec();
    let (tool, _) = generators::tool(&[&group], "1.0.0", &spec, &admin).await?;
    let auth = generators::user_with_role(&admin, UserRole::Admin)
        .await?
        .auth_header();
    // while another request holds the tool's lock, queue an update that uploads logo.png
    test_utilities::hold_instance_lock(tool.id).await?;
    let update = tokio::spawn({
        let (host, auth) = (admin.host.clone(), auth.clone());
        async move { send_image(&host, &auth, tool.id, "logo.png").await }
    });
    let_requests_queue().await;
    // give the tool that image straight in the database, as another request's write would
    let key = format!("{}/logo.png", tool.id);
    test_utilities::set_entity_image(tool.id, Some(&key)).await?;
    // the queued update gives up on the lock
    let status = update.await.map_err(|err| Error::new(err.to_string()));
    test_utilities::release_instance_lock(tool.id).await?;
    let status = status??;
    is!(status, 409);
    // the tool keeps its image
    is!(
        admin.entities.get(tool.id).await?.image.as_deref(),
        Some(key.as_str())
    );
    is!(image_status(&admin.host, &auth, tool.id).await?, 200);
    Ok(())
}

/// An update that fails after the tool is written keeps the image the tool now has
#[tokio::test]
async fn failures_after_the_write_keep_the_new_image() -> Result<(), Error> {
    // a tool with no image
    let admin = test_utilities::admin_client().await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let spec = generators::gen_agentic_spec();
    let (tool, _) = generators::tool(&[&group], "1.0.0", &spec, &admin).await?;
    let auth = generators::user_with_role(&admin, UserRole::Admin)
        .await?
        .auth_header();
    // an update with an image fails once the tool is written
    test_utilities::faults::fail_after_entity_write(tool.id);
    is!(
        send_image(&admin.host, &auth, tool.id, "new.png").await?,
        500
    );
    // the tool has the new image and it's still stored
    let key = format!("{}/new.png", tool.id);
    is!(
        admin.entities.get(tool.id).await?.image.as_deref(),
        Some(key.as_str())
    );
    is!(image_status(&admin.host, &auth, tool.id).await?, 200);
    Ok(())
}

/// Build an agentic spec whose input and output schemas are each just under the size limit
fn large_spec() -> ToolSpec {
    // a schema with one property whose description fills it
    let schema = |name: &str| {
        let mut schema = serde_json::Map::new();
        schema.insert("type".into(), "object".into());
        schema.insert(
            "properties".into(),
            json!({ name: {"type": "string", "description": "x".repeat(63 * 1024)} }),
        );
        schema
    };
    // put the large schemas on a random agentic tool
    let mut spec = generators::gen_agentic_spec();
    if let ToolSpec::Agentic(agentic) = &mut spec {
        agentic.input_schema = schema("input");
        agentic.output_schema = Some(schema("output"));
    }
    spec
}

/// A tool instance with large schemas in many groups can still be deprecated and widened
#[tokio::test]
async fn large_specs_in_many_groups() -> Result<(), Error> {
    // a tool with large schemas in 8 groups and 2 more groups to add
    let admin = test_utilities::admin_client().await?;
    let groups = generators::groups(10, &admin)
        .await?
        .into_iter()
        .map(|group| group.name)
        .collect::<Vec<String>>();
    let initial = groups[..8]
        .iter()
        .map(String::as_str)
        .collect::<Vec<&str>>();
    let (tool, instance) = generators::tool(&initial, "1.0.0", &large_spec(), &admin).await?;
    // widening the tool to all 10 groups reaches the instance
    let update = EntityUpdate::default()
        .group(groups[8].clone())
        .group(groups[9].clone());
    admin.entities.update(tool.id, update).await?;
    is!(stored_groups(tool.id).await?, sorted(&groups));
    is!(stored_groups(instance.id).await?, sorted(&groups));
    // deprecating the instance rewrites it in all 10 groups
    admin
        .entities
        .update(instance.id, generators::gen_tool_deprecate(true))
        .await?;
    let rows = test_utilities::entity_rows(instance.id).await?;
    is!(rows.len(), 10);
    for (group, data) in rows {
        let meta: EntityMetadata = serde_json::from_str(&data)?;
        match meta {
            EntityMetadata::ToolInstance(meta) => is!(meta.deprecated, true, group),
            other => panic!("expected a tool instance, got {other:?}"),
        }
    }
    Ok(())
}
