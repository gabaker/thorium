//! Integration tests for `Device` entity CRUD
//!
//! Device entities reference vendor entities by id, so each test creates a vendor
//! entity first to satisfy that dependency.

mod common;

use test_utilities::{events, generators};
use thorium::models::{
    Association, AssociationKind, AssociationRequest, AssociationTarget, Directionality, Entity,
    EntityKinds, EntityListLine, EntityListOpts, EntityRequest, EventType,
};
use thorium::test_utilities;
use thorium::{Error, Thorium, fail, is};
use uuid::Uuid;

/// Build a device entity request backed by a freshly created vendor
///
/// # Arguments
///
/// * `client` - The client to create the vendor dependency with
/// * `group` - The group the vendor and device should be in
async fn request(client: &Thorium, group: &str) -> Result<EntityRequest, Error> {
    // create a vendor entity to associate with this device
    let vendor = generators::vendor_entity(group, client).await?;
    // build a device entity request that references our vendor
    Ok(generators::gen_entity(
        group,
        generators::gen_device_meta(vec![vendor.id]),
    ))
}

/// Verify a device entity can be created and matches its request
#[tokio::test]
async fn create() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // build a device entity request and verify creation
    let req = request(&client, &group).await?;
    common::check_create(&client, &group, req).await
}

/// Verify a device entity is updated as requested
#[tokio::test]
async fn update() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // build a device entity request and verify updates apply
    let req = request(&client, &group).await?;
    common::check_update_meta(&client, req, generators::gen_device_update).await
}

/// Verify a device entity can be deleted
#[tokio::test]
async fn delete() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // build a device entity request and verify deletion
    let req = request(&client, &group).await?;
    common::check_delete(&client, req).await
}

/// Find whether an association list holds an edge of a kind and direction to an entity
///
/// # Arguments
///
/// * `associations` - The associations to search
/// * `kind` - The kind of association to look for
/// * `other` - The id of the entity on the other side
/// * `direction` - The direction of the association from the listed entity
fn has_edge(
    associations: &[Association],
    kind: AssociationKind,
    other: Uuid,
    direction: Directionality,
) -> bool {
    // look for an association matching every field we care about
    associations.iter().any(|assoc| {
        assoc.kind == kind
            && assoc.direction == direction
            && matches!(&assoc.other, AssociationTarget::Entity { id, .. } if *id == other)
    })
}

/// Build an entity association target
///
/// # Arguments
///
/// * `entity` - The entity to target
fn target(entity: &Entity) -> AssociationTarget {
    AssociationTarget::Entity {
        id: entity.id,
        name: entity.name.clone(),
    }
}

/// An admin lists both directions of every association kind an entity has
/// without changing anything
#[tokio::test]
async fn associations() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a device that is developed by a vendor
    let vendor = generators::vendor_entity(&group, &client).await?.id;
    let req = generators::gen_entity(&group, generators::gen_device_meta(vec![vendor]));
    let (_, device) = generators::entity(req, &client).await?;
    // create two more vendors to link to the device
    let other = generators::vendor_entity(&group, &client).await?;
    let third = generators::vendor_entity(&group, &client).await?;
    // link both vendors to the device so it holds both directions of two kinds
    let edges = [
        (AssociationKind::AssociatedWith, &other, &device),
        (AssociationKind::AssociatedWith, &device, &third),
        (AssociationKind::DevelopedBy, &third, &device),
    ];
    for (kind, source, dest) in edges {
        let assoc = AssociationRequest::new(kind, target(source)).target(target(dest));
        client.associations.create(&assoc).await?;
    }
    // capture the event queues before listing associations
    let triggers = events::queue_state(EventType::ReactionTrigger).await?;
    let sigma = events::queue_state(EventType::SigmaScannableResults).await?;
    // list the device's associations
    let listed = client.entities.associations(device.id).await?;
    // the device lists each kind in both directions
    let expected = [
        (AssociationKind::DevelopedBy, vendor, Directionality::To),
        (AssociationKind::DevelopedBy, third.id, Directionality::From),
        (
            AssociationKind::AssociatedWith,
            other.id,
            Directionality::From,
        ),
        (
            AssociationKind::AssociatedWith,
            third.id,
            Directionality::To,
        ),
    ];
    for (kind, other_id, direction) in expected {
        is!(has_edge(&listed, kind, other_id, direction), true);
    }
    is!(listed.len(), expected.len());
    // each other entity lists its edges to the device in the opposite direction
    let mirrored = [
        (vendor, AssociationKind::DevelopedBy, Directionality::From),
        (third.id, AssociationKind::DevelopedBy, Directionality::To),
        (
            other.id,
            AssociationKind::AssociatedWith,
            Directionality::To,
        ),
        (
            third.id,
            AssociationKind::AssociatedWith,
            Directionality::From,
        ),
    ];
    for (id, kind, direction) in mirrored {
        let other_listed = client.entities.associations(id).await?;
        is!(has_edge(&other_listed, kind, device.id, direction), true);
    }
    // listing again returns the same associations, so the read changed nothing
    let again = client.entities.associations(device.id).await?;
    is!(again.len(), listed.len());
    // the event queues are unchanged by the reads
    is!(
        events::queue_state(EventType::ReactionTrigger).await?,
        triggers
    );
    is!(
        events::queue_state(EventType::SigmaScannableResults).await?,
        sigma
    );
    Ok(())
}

/// Listing an entity's associations needs an admin with a full token
/// and a known entity
#[tokio::test]
async fn associations_errors() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (admin, group) = common::setup().await?;
    // an unknown entity is not found
    fail!(admin.entities.associations(Uuid::new_v4()).await, 404);
    // create a vendor whose associations others try to list
    let vendor = generators::vendor_entity(&group, &admin).await?;
    // a non-admin full token is refused
    let user = generators::client(&admin).await?;
    fail!(user.entities.associations(vendor.id).await, 401);
    // a scoped token of an admin is refused
    let scoped = generators::scoped_client(&admin).await?;
    fail!(scoped.entities.associations(vendor.id).await, 401);
    // a request without a token is refused
    let url = format!("{}/api/entities/{}/associations", admin.host, vendor.id);
    let resp = reqwest::Client::new().get(&url).send().await?;
    is!(resp.status().as_u16(), 401);
    Ok(())
}

/// List the details of every entity in a group with a tag and of some kinds, paging through the cursor
///
/// # Arguments
///
/// * `client` - The client to list entities with
/// * `group` - The group to list entities from
/// * `tag` - The tag key and value to filter on
/// * `kinds` - The kinds of entities to list
async fn list_tagged(
    client: &Thorium,
    group: &str,
    tag: (&str, &str),
    kinds: Vec<EntityKinds>,
) -> Result<Vec<Entity>, Error> {
    // build the options to list our tagged entities of the requested kinds
    let opts = EntityListOpts::default()
        .groups(vec![group.to_owned()])
        .tag(tag.0, tag.1)
        .kinds(kinds)
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

/// An entity list filtered by a tag and a kind returns only entities of that kind
#[tokio::test]
async fn list_tag_and_kind() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // pick a tag that the vendor and device both carry
    let key = generators::gen_string(12);
    let value = generators::gen_string(12);
    // create a vendor with the shared tag
    let vendor_req =
        generators::gen_entity(&group, generators::gen_vendor_meta()).tag(&key, &value);
    let (_, vendor) = generators::entity(vendor_req, &client).await?;
    // create a device developed by that vendor with the shared tag
    let device_req = generators::gen_entity(&group, generators::gen_device_meta(vec![vendor.id]))
        .tag(&key, &value);
    let (_, device) = generators::entity(device_req, &client).await?;
    // listing devices by the tag returns only the device
    let devices = list_tagged(&client, &group, (&key, &value), vec![EntityKinds::Device]).await?;
    is!(
        devices.iter().map(|entity| entity.id).collect::<Vec<_>>(),
        vec![device.id]
    );
    is!(
        devices
            .iter()
            .all(|entity| entity.kind == EntityKinds::Device),
        true
    );
    // listing vendors by the tag returns only the vendor
    let vendors = list_tagged(&client, &group, (&key, &value), vec![EntityKinds::Vendor]).await?;
    is!(
        vendors.iter().map(|entity| entity.id).collect::<Vec<_>>(),
        vec![vendor.id]
    );
    // listing both kinds by the tag returns both entities
    let mut both = list_tagged(
        &client,
        &group,
        (&key, &value),
        vec![EntityKinds::Device, EntityKinds::Vendor],
    )
    .await?
    .into_iter()
    .map(|entity| entity.id)
    .collect::<Vec<_>>();
    both.sort_unstable();
    let mut expected = vec![device.id, vendor.id];
    expected.sort_unstable();
    is!(both, expected);
    // listing a kind no tagged entity has returns nothing
    let none = list_tagged(&client, &group, (&key, &value), vec![EntityKinds::Flag]).await?;
    is!(none.is_empty(), true);
    Ok(())
}

/// List the lines of every entity in a group with a tag and of some kinds, one entity per page
///
/// # Arguments
///
/// * `client` - The client to list entities with
/// * `group` - The group to list entities from
/// * `tag` - The tag key and value to filter on
/// * `kinds` - The kinds of entities to list
async fn list_tagged_lines(
    client: &Thorium,
    group: &str,
    tag: (&str, &str),
    kinds: Vec<EntityKinds>,
) -> Result<Vec<EntityListLine>, Error> {
    // build the options to list our tagged entities a page of one at a time
    let opts = EntityListOpts::default()
        .groups(vec![group.to_owned()])
        .tag(tag.0, tag.1)
        .kinds(kinds)
        .page_size(1)
        .limit(10_000);
    // get the first page of entity lines
    let mut cursor = client.entities.list(&opts).await?;
    // start collecting the lines from the first page
    let mut all = std::mem::take(&mut cursor.data);
    // page through the rest of the cursor until it is exhausted
    while !cursor.exhausted() {
        cursor.refill().await?;
        all.append(&mut cursor.data);
    }
    // sort the lines by id so they compare in a fixed order
    all.sort_unstable_by_key(|line| line.id);
    Ok(all)
}

/// `Entities::list` decodes the lines `/api/entities/` returns: each entity's id,
/// name, kind, and creation time, across pages and filtered by kind
#[tokio::test]
async fn list_lines() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // pick a tag that the vendor and device both carry
    let key = generators::gen_string(12);
    let value = generators::gen_string(12);
    // create a vendor and a device developed by it, both with the shared tag
    let vendor_req =
        generators::gen_entity(&group, generators::gen_vendor_meta()).tag(&key, &value);
    let (_, vendor) = generators::entity(vendor_req, &client).await?;
    let device_req = generators::gen_entity(&group, generators::gen_device_meta(vec![vendor.id]))
        .tag(&key, &value);
    let (_, device) = generators::entity(device_req, &client).await?;
    // build the line we expect for each entity, sorted by id
    let mut expected = [&vendor, &device]
        .iter()
        .map(|entity| (entity.id, entity.name.clone(), entity.kind, entity.created))
        .collect::<Vec<_>>();
    expected.sort_unstable_by_key(|line| line.0);
    // listing both kinds returns a line for each entity across two pages
    let kinds = vec![EntityKinds::Device, EntityKinds::Vendor];
    let lines = list_tagged_lines(&client, &group, (&key, &value), kinds).await?;
    let found = lines
        .iter()
        .map(|line| (line.id, line.name.clone(), line.kind, line.created))
        .collect::<Vec<_>>();
    is!(found, expected);
    // listing only devices returns only the device's line
    let kinds = vec![EntityKinds::Device];
    let lines = list_tagged_lines(&client, &group, (&key, &value), kinds).await?;
    is!(
        lines.iter().map(|line| line.id).collect::<Vec<_>>(),
        vec![device.id]
    );
    Ok(())
}
