//! Shared helpers for the entity CRUD integration tests
//!
//! Each entity kind has its own test binary under `tests/entities/`. Every one of
//! those binaries pulls this module in with `mod common;` to reuse the setup and
//! assertion logic below, so some helpers may be unused in any single binary.
#![allow(dead_code)]

use reqwest::multipart::Form;
use std::time::{Duration, Instant};
use thorium::models::{
    Association, AssociationKind, AssociationTarget, Directionality, Entity, EntityKinds,
    EntityListOpts, EntityMetadata, EntityMetadataUpdate, EntityRequest, EntityUpdate,
    GroupAllowed, UserRole, WorkflowGraphChange,
};
use thorium::test_utilities::generators::{GroupRoles, TestUser};
use thorium::test_utilities::{self, events, generators};
use thorium::{Error, Thorium, fail, is, is_in};
use uuid::Uuid;

/// A queued workflow graph change, with who caused it and the groups it was queued for
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedChange {
    /// The user that caused the change
    pub user: String,
    /// Every group the change was queued for, sorted
    pub groups: Vec<String>,
    /// What changed
    pub change: WorkflowGraphChange,
}

/// Get every queued workflow graph change that names an entity, oldest first
///
/// Nothing pops these events in the entity tests, so the changes a call queued are the ones
/// past the count read before it.
///
/// # Arguments
///
/// * `admin` - The admin client to peek with
/// * `id` - The entity the changes must name
pub async fn changes(admin: &Thorium, id: Uuid) -> Result<Vec<QueuedChange>, Error> {
    // read the queued events naming this entity
    let queued = events::graph_changes(admin, id).await?;
    // keep who caused each change, its sorted groups, and the change
    Ok(queued
        .iter()
        .filter_map(|event| {
            events::graph_event(event).map(|graph| {
                let mut groups = graph.groups.clone();
                groups.sort();
                QueuedChange {
                    user: event.user.clone(),
                    groups,
                    change: graph.change.clone(),
                }
            })
        })
        .collect())
}

/// Get the workflow graph changes naming an entity that were queued after an earlier read
///
/// # Arguments
///
/// * `admin` - The admin client to peek with
/// * `id` - The entity the changes must name
/// * `seen` - How many changes naming the entity the earlier read found
pub async fn changes_since(
    admin: &Thorium,
    id: Uuid,
    seen: usize,
) -> Result<Vec<QueuedChange>, Error> {
    // drop the changes the earlier read already found
    let mut found = changes(admin, id).await?;
    Ok(found.split_off(seen.min(found.len())))
}

/// Build the queued change one write by a user in some groups is expected to queue
///
/// # Arguments
///
/// * `user` - The user who made the change
/// * `groups` - Every group of the changed entity
/// * `change` - What changed
pub fn queued(user: &str, groups: &[&str], change: WorkflowGraphChange) -> QueuedChange {
    // sort the groups the way queued changes are compared
    let mut groups = groups
        .iter()
        .map(|group| (*group).to_owned())
        .collect::<Vec<String>>();
    groups.sort();
    QueuedChange {
        user: user.to_owned(),
        groups,
        change,
    }
}

/// Get an admin client and create a single group to hold test entities
///
/// Returns the client and the name of the created group.
pub async fn setup() -> Result<(Thorium, String), Error> {
    // get an admin client
    let client = test_utilities::admin_client().await?;
    // create a group for our entities to live in
    let group = generators::groups(1, &client).await?.remove(0).name;
    Ok((client, group))
}

/// List all entity details in a group, paging through the entire cursor
///
/// # Arguments
///
/// * `client` - The client to list entities with
/// * `group` - The group to list entities from
pub async fn list_all(client: &Thorium, group: &str) -> Result<Vec<Entity>, Error> {
    // build the options to list all of the entities in our group
    let opts = EntityListOpts::default()
        .groups(vec![group.to_owned()])
        .limit(10_000);
    // get the first page of entity details
    let mut cursor = client.entities.list_details(&opts).await?;
    // start collecting the entities from the first page
    let mut all = std::mem::take(&mut cursor.data);
    // page through the rest of the cursor until it is exhausted
    while !cursor.exhausted() {
        cursor.refill().await?;
        all.append(&mut cursor.data);
    }
    Ok(all)
}

/// Create an entity from a request and verify it matches and is listed
///
/// # Arguments
///
/// * `client` - The client to create the entity with
/// * `group` - The group the entity should be created in
/// * `req` - The entity request to create
pub async fn check_create(client: &Thorium, group: &str, req: EntityRequest) -> Result<(), Error> {
    // create the entity and fetch its full details back
    let (req, entity) = generators::entity(req, client).await?;
    // make sure the created entity matches the request
    is!(entity, req);
    // make sure the entity shows up in a detailed listing for its group
    let listed = list_all(client, group).await?;
    is_in!(listed, req);
    Ok(())
}

/// Create an entity from a request, update it, and verify the update was applied
///
/// # Arguments
///
/// * `client` - The client to create and update the entity with
/// * `req` - The entity request to create then update
pub async fn check_update(client: &Thorium, req: EntityRequest) -> Result<(), Error> {
    // create the entity we are going to update
    let (_, entity) = generators::entity(req, client).await?;
    // create a second group to move the entity into
    let new_group = generators::groups(1, client).await?.remove(0).name;
    // build an update that touches the name, description, and groups
    let update = EntityUpdate::default()
        .name(generators::gen_string(16))
        .description(generators::gen_string(32))
        .group(new_group);
    // apply the update to our entity
    client.entities.update(entity.id, update.clone()).await?;
    // get the entity back and make sure the update was applied
    let updated = client.entities.get(entity.id).await?;
    is!(updated, update);
    Ok(())
}

/// Create an entity, update its generic and kind-specific fields, and verify
///
/// Builds a kind-specific metadata update from the created entity (so it can
/// exercise remove-semantics against existing values), applies it alongside the
/// generic name/description/group changes, and verifies every field was applied.
///
/// # Arguments
///
/// * `client` - The client to create and update the entity with
/// * `req` - The entity request to create then update
/// * `gen_meta` - Builds the kind-specific metadata update from the created entity
pub async fn check_update_meta(
    client: &Thorium,
    req: EntityRequest,
    gen_meta: impl FnOnce(&Entity) -> EntityMetadataUpdate,
) -> Result<(), Error> {
    // create the entity we are going to update
    let (_, entity) = generators::entity(req, client).await?;
    // create a second group to move the entity into
    let new_group = generators::groups(1, client).await?.remove(0).name;
    // build an update touching the name, description, groups, and kind metadata
    let update = EntityUpdate::default()
        .name(generators::gen_string(16))
        .description(generators::gen_string(32))
        .group(new_group)
        .metadata(gen_meta(&entity));
    // apply the update to our entity
    client.entities.update(entity.id, update.clone()).await?;
    // get the entity back and make sure the update was applied
    let updated = client.entities.get(entity.id).await?;
    is!(updated, update);
    Ok(())
}

/// Create an entity from a request, delete it, and verify it can't be fetched
///
/// # Arguments
///
/// * `client` - The client to create and delete the entity with
/// * `req` - The entity request to create then delete
pub async fn check_delete(client: &Thorium, req: EntityRequest) -> Result<(), Error> {
    // create the entity we are going to delete
    let (_, entity) = generators::entity(req, client).await?;
    // delete the entity
    client.entities.delete(entity.id).await?;
    // make sure the entity can no longer be fetched
    let resp = client.entities.get(entity.id).await;
    fail!(resp, 404);
    Ok(())
}

/// How the shared provenance checks build one kind of entity that records the tool instances
/// that produced it
pub struct ProvenanceKind {
    /// The kind of entity
    pub kind: EntityKinds,
    /// The form fields a new entity of this kind needs besides its tool instances
    pub base_fields: fn() -> Vec<(&'static str, String)>,
    /// The kind data of a row of this kind stored with the given tool strings
    pub stored_row: fn(&[String]) -> String,
    /// Build a provenance update of this kind from tool instances to add and remove and legacy
    /// names to remove
    pub update: fn(Vec<Uuid>, Vec<Uuid>, Vec<String>) -> EntityMetadataUpdate,
}

/// Get the tool instances and legacy tool names of a provenance entity
///
/// # Arguments
///
/// * `entity` - The entity to get tool references from
pub fn provenance(entity: &Entity) -> Result<(Vec<Uuid>, Vec<String>), Error> {
    // only the provenance kinds record tools
    match &entity.metadata {
        EntityMetadata::FileSystem(fs) => Ok((fs.tools.clone(), fs.legacy_tools.clone())),
        EntityMetadata::WindowsProcessTree(tree) => {
            Ok((tree.tools.clone(), tree.legacy_tools.clone()))
        }
        EntityMetadata::DecompiledFunction(func) => {
            Ok((func.tools.clone(), func.legacy_tools.clone()))
        }
        other => Err(Error::new(format!("{other:?} records no tools"))),
    }
}

/// Build a form from a list of fields
///
/// # Arguments
///
/// * `fields` - The fields to put in the form
fn build_form(fields: &[(&str, String)]) -> Form {
    // add each field in order
    fields.iter().fold(Form::new(), |form, (name, value)| {
        form.text((*name).to_owned(), value.clone())
    })
}

/// Send a raw entity create form as a user and get the status, body, and created id
///
/// # Arguments
///
/// * `host` - The API to send to
/// * `sender` - The user to send the form as
/// * `kind` - The kind of entity to create
/// * `group` - The group to create the entity in
/// * `extra` - The fields to send besides the kind's base fields
pub async fn raw_create(
    host: &str,
    sender: &TestUser,
    kind: &ProvenanceKind,
    group: &str,
    extra: &[(&str, String)],
) -> Result<(u16, String, Option<Uuid>), Error> {
    // build the name, group, and kind fields followed by the extra fields
    let mut fields = vec![
        ("name", generators::gen_string(16)),
        ("groups[]", group.to_owned()),
    ];
    fields.extend((kind.base_fields)());
    fields.extend(extra.iter().cloned());
    // send the form as this user
    let resp = reqwest::Client::new()
        .post(format!("{host}/api/entities/"))
        .header("authorization", sender.auth_header())
        .multipart(build_form(&fields))
        .send()
        .await?;
    // get the status and body of the response
    let status = resp.status().as_u16();
    let body = resp.text().await?;
    // get the id of a created entity
    let id = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| value["id"].as_str().and_then(|id| id.parse().ok()));
    Ok((status, body, id))
}

/// Send a raw entity update form as a user and get the status and body
///
/// # Arguments
///
/// * `host` - The API to send to
/// * `sender` - The user to send the form as
/// * `id` - The entity to update
/// * `fields` - The fields to send
pub async fn raw_update(
    host: &str,
    sender: &TestUser,
    id: Uuid,
    fields: &[(&str, String)],
) -> Result<(u16, String), Error> {
    // send the form as this user
    let resp = reqwest::Client::new()
        .patch(format!("{host}/api/entities/{id}"))
        .header("authorization", sender.auth_header())
        .multipart(build_form(fields))
        .send()
        .await?;
    // get the status and body of the response
    let status = resp.status().as_u16();
    Ok((status, resp.text().await?))
}

/// Get the raw JSON the API returns for a request as a user
///
/// # Arguments
///
/// * `url` - The url to get
/// * `sender` - The user to get it as
async fn raw_get(url: &str, sender: &TestUser) -> Result<serde_json::Value, Error> {
    // get the url as this user
    let resp = reqwest::Client::new()
        .get(url)
        .header("authorization", sender.auth_header())
        .send()
        .await?
        .error_for_status()?;
    Ok(resp.json().await?)
}

/// Create an entity of a provenance kind through a raw form, failing if it isn't created
///
/// # Arguments
///
/// * `host` - The API to send to
/// * `sender` - The user to create the entity as
/// * `kind` - The kind of entity to create
/// * `group` - The group to create the entity in
/// * `tools` - The tool instances that produced the entity
async fn create_with_tools(
    host: &str,
    sender: &TestUser,
    kind: &ProvenanceKind,
    group: &str,
    tools: &[Uuid],
) -> Result<Uuid, Error> {
    // send each tool instance as a tool id
    let extra = tools
        .iter()
        .map(|tool| ("metadata[tool_ids][]", tool.to_string()))
        .collect::<Vec<_>>();
    // create the entity
    let (status, body, id) = raw_create(host, sender, kind, group, &extra).await?;
    // make sure it was created
    match (status, id) {
        (200, Some(id)) => Ok(id),
        _ => Err(Error::new(format!("create failed with {status}: {body}"))),
    }
}

/// Read the stored kind data of an entity's rows as JSON
///
/// # Arguments
///
/// * `id` - The entity to read
async fn stored_rows(id: Uuid) -> Result<Vec<(String, serde_json::Value)>, Error> {
    // read the entity's rows straight from Scylla
    let rows = test_utilities::entity_rows(id).await?;
    // parse each row's kind data
    rows.into_iter()
        .map(|(group, data)| {
            serde_json::from_str(&data)
                .map(|value| (group, value))
                .map_err(|err| Error::new(format!("bad stored kind data {data}: {err}")))
        })
        .collect()
}

/// An old-format row keeps its legacy tool names through an unrelated update, and the update
/// stores it in the current shape
///
/// # Arguments
///
/// * `client` - An admin client
/// * `kind` - The kind of entity to check
pub async fn check_legacy_survives_update(
    client: &Thorium,
    kind: &ProvenanceKind,
) -> Result<(), Error> {
    // create a group, a sender, and a tool instance
    let group = generators::groups(1, client).await?.remove(0).name;
    let sender = generators::user_with_role(client, UserRole::Admin).await?;
    let tool = generators::tool_instance(&[&group], client).await?;
    // create an entity of this kind
    let id = create_with_tools(&client.host, &sender, kind, &group, &[tool]).await?;
    // overwrite its row with one stored before tool instances
    let old = (kind.stored_row)(&["volatility".to_owned(), tool.to_string()]);
    test_utilities::set_entity_kind_data(id, &old).await?;
    println!(
        "{} {id} stored row before update: {:?}",
        kind.kind,
        stored_rows(id).await?
    );
    // the old row reads with its name in legacy tools
    let entity = client.entities.get(id).await?;
    let read = provenance(&entity)?;
    is!(read, (vec![tool], vec!["volatility".to_owned()]));
    println!(
        "{} {id} read before update: tools={:?} legacy_tools={:?}",
        kind.kind, read.0, read.1
    );
    // update a field that has nothing to do with tools
    let update = EntityUpdate::default().description(generators::gen_string(24));
    client.entities.update(id, update).await?;
    // the stored row is in the current shape and keeps the legacy name
    let rows = stored_rows(id).await?;
    println!("{} {id} stored row after update: {rows:?}", kind.kind);
    for (_, row) in &rows {
        // get this row's kind data
        let data = &row[kind.kind.as_str()];
        is!(data["tools"], serde_json::json!([tool.to_string()]));
        is!(data["legacy_tools"], serde_json::json!(["volatility"]));
    }
    // the updated entity reads the same tool references
    let entity = client.entities.get(id).await?;
    is!(
        provenance(&entity)?,
        (vec![tool], vec!["volatility".to_owned()])
    );
    // adding the entity to another group keeps the legacy name in the rows of both groups
    let added = generators::groups(1, client).await?.remove(0).name;
    client
        .entities
        .update(id, EntityUpdate::default().group(added.clone()))
        .await?;
    let rows = stored_rows(id).await?;
    println!("{} {id} stored rows after group add: {rows:?}", kind.kind);
    let groups = rows
        .iter()
        .map(|(group, _)| group.clone())
        .collect::<std::collections::BTreeSet<String>>();
    let expected = std::collections::BTreeSet::from([group, added]);
    is!(groups, expected);
    for (_, row) in &rows {
        // get this row's kind data
        let data = &row[kind.kind.as_str()];
        is!(data["tools"], serde_json::json!([tool.to_string()]));
        is!(data["legacy_tools"], serde_json::json!(["volatility"]));
    }
    Ok(())
}

/// Tool names are rejected in every create and update field, tool instance ids are accepted,
/// and legacy names can only be removed
///
/// # Arguments
///
/// * `client` - An admin client
/// * `kind` - The kind of entity to check
pub async fn check_legacy_form_names(client: &Thorium, kind: &ProvenanceKind) -> Result<(), Error> {
    // create a group, a sender, and two tool instances
    let group = generators::groups(1, client).await?.remove(0).name;
    let sender = generators::user_with_role(client, UserRole::Admin).await?;
    let first = generators::tool_instance(&[&group], client).await?;
    let second = generators::tool_instance(&[&group], client).await?;
    // a create that sends tool names or legacy names is rejected naming the field
    for name in ["tools", "legacy_tools"] {
        // send a create with this field
        let field = format!("metadata[{name}][]");
        let extra = [(field.as_str(), "volatility".to_owned())];
        let (status, body, _) = raw_create(&client.host, &sender, kind, &group, &extra).await?;
        // it is rejected and the error names the field
        is!(status, 400, format!("create with {field}: {body}"));
        is!(body.contains(&format!("'{name}'")), true, body);
    }
    // create an entity and give it an old row with two legacy names
    let id = create_with_tools(&client.host, &sender, kind, &group, &[first]).await?;
    let old = (kind.stored_row)(&[
        "volatility".to_owned(),
        "binwalk".to_owned(),
        first.to_string(),
    ]);
    test_utilities::set_entity_kind_data(id, &old).await?;
    // an update that adds or removes tool names or adds legacy names is rejected naming the field
    for name in ["add_tools", "remove_tools", "add_legacy_tools"] {
        // send an update with this field
        let field = format!("metadata[{name}][]");
        let (status, body) = raw_update(
            &client.host,
            &sender,
            id,
            &[(field.as_str(), "volatility".to_owned())],
        )
        .await?;
        // it is rejected and the error names the field
        is!(status, 400, format!("update with {field}: {body}"));
        is!(body.contains(&format!("'{name}'")), true, body);
    }
    // the rejected updates changed nothing
    let entity = client.entities.get(id).await?;
    is!(
        provenance(&entity)?,
        (
            vec![first],
            vec!["volatility".to_owned(), "binwalk".to_owned()]
        )
    );
    // the same edit with tool instance ids succeeds
    let fields = [
        ("metadata[add_tool_ids][]", second.to_string()),
        ("metadata[remove_tool_ids][]", first.to_string()),
    ];
    let (status, body) = raw_update(&client.host, &sender, id, &fields).await?;
    is!(status, 204, body);
    // removing one legacy name leaves the other
    let fields = [("metadata[remove_legacy_tools][]", "volatility".to_owned())];
    let (status, body) = raw_update(&client.host, &sender, id, &fields).await?;
    is!(status, 204, body);
    let entity = client.entities.get(id).await?;
    is!(
        provenance(&entity)?,
        (vec![second], vec!["binwalk".to_owned()])
    );
    Ok(())
}

/// Tool instance ids must be tool instances the submitter can see when they're added, and are
/// never checked when they're removed
///
/// # Arguments
///
/// * `client` - An admin client
/// * `kind` - The kind of entity to check
pub async fn check_tool_id_validation(
    client: &Thorium,
    kind: &ProvenanceKind,
) -> Result<(), Error> {
    // run the checks in a boxed future so callers' futures stay small
    Box::pin(check_tool_id_validation_inner(client, kind)).await
}

/// Run the checks behind [`check_tool_id_validation`]
///
/// # Arguments
///
/// * `client` - An admin client
/// * `kind` - The kind of entity to check
async fn check_tool_id_validation_inner(
    client: &Thorium,
    kind: &ProvenanceKind,
) -> Result<(), Error> {
    // create a user who is a member of one group but not another
    let user = generators::user_with_role(client, UserRole::User).await?;
    let mine = generators::group_with_roles(
        &GroupRoles::default().user(&user.username),
        GroupAllowed::default(),
        client,
    )
    .await?;
    let other = generators::groups(1, client).await?.remove(0).name;
    // create a tool in the user's group and one only in the other group
    let (tool, visible) =
        generators::tool(&[&mine], "1.0.0", &generators::gen_image_spec(), client).await?;
    let hidden = generators::tool_instance(&[&other], client).await?;
    // a deprecated tool instance in the user's group
    let deprecated = generators::tool_instance(&[&mine], client).await?;
    client
        .entities
        .update(deprecated, generators::gen_tool_deprecate(true))
        .await?;
    // ids that aren't visible tool instances are rejected on create
    let rejected = [
        Uuid::new_v4().to_string(),
        hidden.to_string(),
        tool.id.to_string(),
        "not-a-uuid".to_owned(),
    ];
    let mut refusals = Vec::with_capacity(rejected.len());
    for value in rejected {
        // send a create with this tool id as the user
        let extra = [("metadata[tool_ids][]", value.clone())];
        let (status, body, _) = raw_create(&client.host, &user, kind, &mine, &extra).await?;
        // it is rejected
        is!(status, 400, format!("create with tool id {value}: {body}"));
        refusals.push(body.replace(&value, "<id>"));
    }
    // a missing entity, a hidden tool instance, and a tool get the same refusal
    is!(refusals[0], refusals[1]);
    is!(refusals[0], refusals[2]);
    // a tool instance only in another of the user's groups is accepted in this one
    let second = generators::group_with_roles(
        &GroupRoles::default().user(&user.username),
        GroupAllowed::default(),
        client,
    )
    .await?;
    let elsewhere = generators::tool_instance(&[&second], client).await?;
    let id = create_with_tools(&client.host, &user, kind, &mine, &[elsewhere]).await?;
    let entity = client.entities.get(id).await?;
    is!(provenance(&entity)?.0, vec![elsewhere]);
    // a visible tool instance sent twice is accepted
    create_with_tools(&client.host, &user, kind, &mine, &[visible.id, visible.id]).await?;
    // a visible tool instance and a deprecated one are accepted
    let id = create_with_tools(&client.host, &user, kind, &mine, &[visible.id, deprecated]).await?;
    let entity = client.entities.get(id).await?;
    is!(provenance(&entity)?.0, vec![visible.id, deprecated]);
    // adding ids that aren't visible tool instances is rejected
    for value in [Uuid::new_v4(), hidden, tool.id] {
        // send an update adding this tool id as the user
        let fields = [("metadata[add_tool_ids][]", value.to_string())];
        let (status, body) = raw_update(&client.host, &user, id, &fields).await?;
        // it is rejected
        is!(
            status,
            400,
            format!("update adding tool id {value}: {body}")
        );
    }
    // a tool instance that is deleted after it was recorded
    let deleted = generators::tool_instance(&[&mine], client).await?;
    let fields = [("metadata[add_tool_ids][]", deleted.to_string())];
    let (status, body) = raw_update(&client.host, &user, id, &fields).await?;
    is!(status, 204, body);
    test_utilities::delete_entity_rows(deleted).await?;
    // the entity still reads with the deleted tool instance
    let entity = client.entities.get(id).await?;
    is!(
        provenance(&entity)?.0,
        vec![visible.id, deprecated, deleted]
    );
    // removing the deleted tool instance isn't checked
    let fields = [("metadata[remove_tool_ids][]", deleted.to_string())];
    let (status, body) = raw_update(&client.host, &user, id, &fields).await?;
    is!(status, 204, body);
    let entity = client.entities.get(id).await?;
    is!(provenance(&entity)?.0, vec![visible.id, deprecated]);
    Ok(())
}

/// The client's provenance update adds and removes tool instances
///
/// # Arguments
///
/// * `client` - An admin client
/// * `kind` - The kind of entity to check
pub async fn check_tool_id_updates(client: &Thorium, kind: &ProvenanceKind) -> Result<(), Error> {
    // create a group, a sender, and three tool instances
    let group = generators::groups(1, client).await?.remove(0).name;
    let sender = generators::user_with_role(client, UserRole::Admin).await?;
    let first = generators::tool_instance(&[&group], client).await?;
    let second = generators::tool_instance(&[&group], client).await?;
    let third = generators::tool_instance(&[&group], client).await?;
    // create an entity found by the first two tool instances
    let id = create_with_tools(&client.host, &sender, kind, &group, &[first, second]).await?;
    // add the third and remove the first through the client
    let update =
        EntityUpdate::default().metadata((kind.update)(vec![third], vec![first], Vec::new()));
    client.entities.update(id, update).await?;
    // the entity has the second and third tool instances
    let entity = client.entities.get(id).await?;
    is!(
        provenance(&entity)?,
        (vec![second, third], Vec::<String>::new())
    );
    Ok(())
}

/// A create with many tool instance ids succeeds
///
/// # Arguments
///
/// * `client` - An admin client
/// * `kind` - The kind of entity to check
pub async fn check_many_tool_ids(client: &Thorium, kind: &ProvenanceKind) -> Result<(), Error> {
    // create a group, a sender, and a tool
    let group = generators::groups(1, client).await?.remove(0).name;
    let sender = generators::user_with_role(client, UserRole::Admin).await?;
    let (tool, first) =
        generators::tool(&[&group], "1.0.0", &generators::gen_image_spec(), client).await?;
    // publish 49 more versions of the tool
    let mut tools = vec![first.id];
    for minor in 1..50 {
        // publish this version
        let publish =
            generators::gen_tool_publish(&format!("1.{minor}.0"), &generators::gen_image_spec());
        client.entities.update(tool.id, publish).await?;
    }
    // get the ids of every instance of the tool
    let instances = client.entities.tool_instances(tool.id).await?;
    tools.extend(
        instances
            .iter()
            .map(|instance| instance.id)
            .filter(|id| *id != first.id),
    );
    is!(tools.len(), 50);
    // create an entity found by all 50 tool instances
    let id = create_with_tools(&client.host, &sender, kind, &group, &tools).await?;
    // the entity records every one of them
    let entity = client.entities.get(id).await?;
    is!(provenance(&entity)?.0, tools);
    Ok(())
}

/// Reading and listing an entity returns its tool instances as uuid strings and leaves out an
/// empty `legacy_tools`
///
/// # Arguments
///
/// * `client` - An admin client
/// * `kind` - The kind of entity to check
pub async fn check_response_shape(client: &Thorium, kind: &ProvenanceKind) -> Result<(), Error> {
    // create a group, a sender, and a tool instance
    let group = generators::groups(1, client).await?.remove(0).name;
    let sender = generators::user_with_role(client, UserRole::Admin).await?;
    let tool = generators::tool_instance(&[&group], client).await?;
    // create an entity found by the tool instance
    let id = create_with_tools(&client.host, &sender, kind, &group, &[tool]).await?;
    // get the entity and list the group's entities of this kind as raw JSON
    let host = &client.host;
    let got = raw_get(&format!("{host}/api/entities/{id}"), &sender).await?;
    let listed = raw_get(
        &format!(
            "{host}/api/entities/details/?groups[]={group}&kinds[]={}",
            kind.kind.as_str()
        ),
        &sender,
    )
    .await?;
    println!("{} {id} GET metadata: {}", kind.kind, got["metadata"]);
    // find the entity in the listing
    let line = listed["data"]
        .as_array()
        .and_then(|data| data.iter().find(|line| line["id"] == id.to_string()))
        .ok_or_else(|| Error::new(format!("{id} not listed: {listed}")))?;
    // both have the tool instance as a uuid string and no legacy tools
    for value in [&got, line] {
        // get this response's kind data
        let data = &value["metadata"][kind.kind.as_str()];
        is!(data["tools"], serde_json::json!([tool.to_string()]));
        is!(data.get("legacy_tools").is_none(), true, data.to_string());
    }
    Ok(())
}

/// A tool instance is recorded once however often a create or update sends it
///
/// # Arguments
///
/// * `client` - An admin client
/// * `kind` - The kind of entity to check
pub async fn check_no_duplicate_tool_ids(
    client: &Thorium,
    kind: &ProvenanceKind,
) -> Result<(), Error> {
    // create a group, a sender, and two tool instances
    let group = generators::groups(1, client).await?.remove(0).name;
    let sender = generators::user_with_role(client, UserRole::Admin).await?;
    let first = generators::tool_instance(&[&group], client).await?;
    let second = generators::tool_instance(&[&group], client).await?;
    // a create that sends the first tool instance twice records it once
    let id = create_with_tools(&client.host, &sender, kind, &group, &[first, first]).await?;
    let entity = client.entities.get(id).await?;
    is!(provenance(&entity)?.0, vec![first]);
    // adding the recorded tool instance again and the second one twice records each once
    let fields = [
        ("metadata[add_tool_ids][]", first.to_string()),
        ("metadata[add_tool_ids][]", second.to_string()),
        ("metadata[add_tool_ids][]", second.to_string()),
    ];
    let (status, body) = raw_update(&client.host, &sender, id, &fields).await?;
    is!(status, 204, body);
    let entity = client.entities.get(id).await?;
    is!(provenance(&entity)?.0, vec![first, second]);
    Ok(())
}

/// A create with more tool instance ids than one lookup batch holds checks every one of them
///
/// # Arguments
///
/// * `client` - An admin client
/// * `kind` - The kind of entity to check
pub async fn check_tool_ids_past_one_batch(
    client: &Thorium,
    kind: &ProvenanceKind,
) -> Result<(), Error> {
    // create a user who is a member of one group but not another
    let user = generators::user_with_role(client, UserRole::User).await?;
    let mine = generators::group_with_roles(
        &GroupRoles::default().user(&user.username),
        GroupAllowed::default(),
        client,
    )
    .await?;
    let other = generators::groups(1, client).await?.remove(0).name;
    // create a tool in the user's group and publish 100 more versions of it
    let (tool, first) =
        generators::tool(&[&mine], "1.0.0", &generators::gen_image_spec(), client).await?;
    for minor in 1..=100 {
        // publish this version
        let publish =
            generators::gen_tool_publish(&format!("1.{minor}.0"), &generators::gen_image_spec());
        client.entities.update(tool.id, publish).await?;
    }
    // get the ids of all 101 instances of the tool
    let mut tools = vec![first.id];
    let instances = client.entities.tool_instances(tool.id).await?;
    tools.extend(
        instances
            .iter()
            .map(|instance| instance.id)
            .filter(|id| *id != first.id),
    );
    is!(tools.len(), 101);
    // a create with all 101 visible tool instances records every one of them
    let id = create_with_tools(&client.host, &user, kind, &mine, &tools).await?;
    let entity = client.entities.get(id).await?;
    is!(provenance(&entity)?.0, tools.clone());
    // 100 visible tool instances followed by a hidden one are refused
    let hidden = generators::tool_instance(&[&other], client).await?;
    let mut extra = tools[..100]
        .iter()
        .map(|tool| ("metadata[tool_ids][]", tool.to_string()))
        .collect::<Vec<_>>();
    extra.push(("metadata[tool_ids][]", hidden.to_string()));
    let (status, body, _) = raw_create(&client.host, &user, kind, &mine, &extra).await?;
    is!(status, 400, body);
    is!(body.contains(&hidden.to_string()), true, body);
    Ok(())
}

/// Create an agentic tool and get it and its first instance
///
/// # Arguments
///
/// * `groups` - The groups to create the tool in
/// * `client` - The client to create the tool with
pub async fn agentic_tool(groups: &[&str], client: &Thorium) -> Result<(Entity, Entity), Error> {
    // publish a random agentic tool at 1.0.0
    generators::tool(groups, "1.0.0", &generators::gen_agentic_spec(), client).await
}

/// Create a Harness tool and get it and its first instance
///
/// # Arguments
///
/// * `groups` - The groups to create the tool in
/// * `client` - The client to create the tool with
pub async fn harness_tool(groups: &[&str], client: &Thorium) -> Result<(Entity, Entity), Error> {
    // publish a random harness at 1.0.0
    generators::tool(groups, "1.0.0", &generators::gen_harness_spec(), client).await
}

/// Get the sorted ids of a list of entities
///
/// # Arguments
///
/// * `entities` - The entities to get the ids of
pub fn ids(entities: &[Entity]) -> Vec<Uuid> {
    // collect and sort the ids
    let mut ids = entities
        .iter()
        .map(|entity| entity.id)
        .collect::<Vec<Uuid>>();
    ids.sort();
    ids
}

/// Make a low and a high id, which sort before and after almost every random id
///
/// Tests give them to created entities so they know how those entities sort against the rest.
pub fn ordered_ids() -> (Uuid, Uuid) {
    // keep only the low 48 bits of a random id so ids from repeated runs stay distinct
    let tail = u128::from(Uuid::new_v4().as_u64_pair().1 >> 16);
    // put the random tail under all-zero or all-one high bits
    (Uuid::from_u128(tail), Uuid::from_u128(u128::MAX - tail))
}

/// Sort a list of ids
///
/// # Arguments
///
/// * `ids` - The ids to sort
pub fn sorted(ids: &[Uuid]) -> Vec<Uuid> {
    // copy and sort the ids
    let mut ids = ids.to_vec();
    ids.sort();
    ids
}

/// Get the other end of an association that links two entities
///
/// # Arguments
///
/// * `association` - The association to read
pub fn other_id(association: &Association) -> Option<Uuid> {
    // only entity ends have an id
    match &association.other {
        AssociationTarget::Entity { id, .. } => Some(*id),
        _ => None,
    }
}

/// Get the ids an entity's edges of one kind point to or come from, read with the admin route
///
/// # Arguments
///
/// * `admin` - The admin client
/// * `id` - The entity to list the edges of
/// * `kind` - The kind of edge
/// * `direction` - Whether to list outgoing (`To`) or incoming (`From`) edges
pub async fn edges(
    admin: &Thorium,
    id: Uuid,
    kind: AssociationKind,
    direction: Directionality,
) -> Result<Vec<Uuid>, Error> {
    // list every association of the entity and keep this kind and direction
    let mut found = admin
        .entities
        .associations(id)
        .await?
        .iter()
        .filter(|assoc| assoc.kind == kind && assoc.direction == direction)
        .filter_map(other_id)
        .collect::<Vec<Uuid>>();
    found.sort();
    found.dedup();
    Ok(found)
}

/// Send an entity metadata update
///
/// # Arguments
///
/// * `client` - The client to send it with
/// * `id` - The entity to update
/// * `update` - The metadata update
pub async fn update_metadata(
    client: &Thorium,
    id: Uuid,
    update: EntityMetadataUpdate,
) -> Result<(), Error> {
    // send the update as an entity update
    client
        .entities
        .update(id, EntityUpdate::default().metadata(update))
        .await?;
    Ok(())
}

/// Release a lock held by the test after a delay long enough to measure a wait on it
///
/// # Arguments
///
/// * `root` - The id of the root entity whose lock is held
/// * `delay` - How long to hold the lock
pub async fn release_later(root: Uuid, delay: Duration) -> Result<(), Error> {
    // wait, then release the lock
    tokio::time::sleep(delay).await;
    test_utilities::release_instance_lock(root).await
}

/// Run a request and measure how long it took to finish
///
/// # Arguments
///
/// * `request` - The request to run
pub async fn timed<T>(
    request: impl std::future::Future<Output = Result<T, Error>>,
) -> Result<Duration, Error> {
    // time the request from when it's first polled
    let start = Instant::now();
    request.await?;
    Ok(start.elapsed())
}

/// Run a request that must be refused and measure how long the refusal took
///
/// Returns the time taken and the refusal's status code.
///
/// # Arguments
///
/// * `request` - The request to run
pub async fn timed_refusal<T: std::fmt::Debug>(
    request: impl std::future::Future<Output = Result<T, Error>>,
) -> (Duration, u16) {
    // time the request from when it's first polled
    let start = Instant::now();
    let result = request.await;
    let elapsed = start.elapsed();
    // the request must have been refused with a status
    let status = match result {
        Ok(value) => panic!("expected a refusal, got {value:?}"),
        Err(err) => err.status().map_or(0, |status| status.as_u16()),
    };
    (elapsed, status)
}
