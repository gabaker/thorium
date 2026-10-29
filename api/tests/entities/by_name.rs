//! Integration tests for looking entities up by their exact name

mod common;

use std::time::Duration;

use test_utilities::generators;
use thorium::models::EntityKinds;
use thorium::test_utilities;
use thorium::{is, is_in};

/// Verify an exact name lookup returns the matching entity with its tags
#[tokio::test]
async fn get_by_name() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // build and create an other entity with a random name
    let req = generators::gen_entity(&group, generators::gen_other_meta());
    let (req, entity) = generators::entity(req, &client).await?;
    // look this entity up by its exact name
    let matches = client.entities.get_by_name(&req.name, &[]).await?;
    // make sure exactly our entity came back with its tags
    is!(matches.len(), 1);
    is!(matches[0].id, entity.id);
    is!(matches[0], req);
    Ok(())
}

/// Verify an unknown name comes back as an empty list instead of an error
#[tokio::test]
async fn get_by_name_unknown_empty() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, _group) = common::setup().await?;
    // look up a random name no entity should have
    let matches = client
        .entities
        .get_by_name(&generators::gen_string(24), &[])
        .await?;
    // make sure nothing matched
    is!(matches.len(), 0);
    Ok(())
}

/// Verify same-named entities all come back ordered oldest-created first
#[tokio::test]
async fn get_by_name_same_name_ordered() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // build two other entity requests sharing one exact name
    let first_req = generators::gen_entity(&group, generators::gen_other_meta());
    let mut second_req = generators::gen_entity(&group, generators::gen_other_meta());
    second_req.name = first_req.name.clone();
    // create the older entity first
    let (_, first) = generators::entity(first_req, &client).await?;
    // wait a moment so the second entity gets a strictly later created time
    tokio::time::sleep(Duration::from_millis(5)).await;
    // create the newer entity with the same name
    let (_, second) = generators::entity(second_req, &client).await?;
    // look both entities up by their shared name
    let matches = client.entities.get_by_name(&first.name, &[]).await?;
    // make sure both entities came back oldest first
    is!(matches.len(), 2);
    is!(matches[0].id, first.id);
    is!(matches[1].id, second.id);
    Ok(())
}

/// Verify the kinds filter excludes other kinds sharing the name
#[tokio::test]
async fn get_by_name_kinds_filter() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // build an other entity and a vendor entity sharing one exact name
    let other_req = generators::gen_entity(&group, generators::gen_other_meta());
    let mut vendor_req = generators::gen_entity(&group, generators::gen_vendor_meta());
    vendor_req.name = other_req.name.clone();
    // create both entities
    generators::entity(other_req, &client).await?;
    let (_, vendor) = generators::entity(vendor_req, &client).await?;
    // look the shared name up restricted to vendor entities
    let matches = client
        .entities
        .get_by_name(&vendor.name, &[EntityKinds::Vendor])
        .await?;
    // make sure only the vendor entity matched
    is!(matches.len(), 1);
    is!(matches[0].id, vendor.id);
    is!(matches[0].kind, EntityKinds::Vendor);
    Ok(())
}

/// Verify an entity in two groups merges to a single match with both groups
#[tokio::test]
async fn get_by_name_merges_groups() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a second group for our entity to also live in
    let extra = generators::groups(1, &client).await?.remove(0).name;
    // build an entity request spanning both groups
    let mut req = generators::gen_entity(&group, generators::gen_other_meta());
    req.groups.push(extra.clone());
    // create the entity in both groups
    let (_, entity) = generators::entity(req, &client).await?;
    // look this entity up by its exact name
    let matches = client.entities.get_by_name(&entity.name, &[]).await?;
    // make sure the group rows merged into one match carrying both groups
    is!(matches.len(), 1);
    is!(matches[0].id, entity.id);
    is!(matches[0].groups.len(), 2);
    is_in!(matches[0].groups, group);
    is_in!(matches[0].groups, extra);
    Ok(())
}

/// Verify a user in neither of the entity's groups sees an empty list
#[tokio::test]
async fn get_by_name_hidden_from_non_member() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create an entity in our group
    let req = generators::gen_entity(&group, generators::gen_other_meta());
    let (_, entity) = generators::entity(req, &client).await?;
    // create another group and a user that is only a member of it
    let outside = generators::groups(1, &client).await?.remove(0).name;
    let member = common::member(&client, &[&outside]).await?;
    // look the entity's name up as the outside member
    let matches = member.entities.get_by_name(&entity.name, &[]).await?;
    // make sure the member cannot see the entity
    is!(matches.len(), 0);
    Ok(())
}

// Sync tests

#[cfg(all(feature = "sync", not(feature = "python")))]
/// Verify an exact name lookup works via the blocking client
#[test]
fn get_by_name_blocking() -> Result<(), thorium::Error> {
    // get a blocking admin client
    let client = test_utilities::admin_client_blocking()?;
    // create a group for our entity
    let group = generators::groups_blocking(1, &client)?.remove(0).name;
    // build and create an other entity
    let req = generators::gen_entity(&group, generators::gen_other_meta());
    let resp = client.entities.create(req.clone())?;
    // look this entity up by its exact name
    let matches = client.entities.get_by_name(&req.name, &[])?;
    // make sure exactly our entity came back
    is!(matches.len(), 1);
    is!(matches[0].id, resp.id);
    Ok(())
}
