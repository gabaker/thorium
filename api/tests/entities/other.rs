//! Integration tests for `Other` entity CRUD

mod common;

use test_utilities::generators;
use thorium::test_utilities;

/// Verify a other entity can be created and matches its request
#[tokio::test]
async fn create() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // build an other entity request and verify creation
    let req = generators::gen_entity(&group, generators::gen_other_meta());
    common::check_create(&client, &group, req).await
}

/// Verify a other entity is updated as requested
#[tokio::test]
async fn update() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // build an other entity request and verify updates apply
    let req = generators::gen_entity(&group, generators::gen_other_meta());
    common::check_update(&client, req).await
}

/// Verify a other entity can be deleted
#[tokio::test]
async fn delete() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // build an other entity request and verify deletion
    let req = generators::gen_entity(&group, generators::gen_other_meta());
    common::check_delete(&client, req).await
}

// Sync tests

#[cfg(all(feature = "sync", not(feature = "python")))]
/// Verify other entity create and delete via the blocking client
#[test]
fn create_delete_blocking() -> Result<(), thorium::Error> {
    // get a blocking admin client
    let client = test_utilities::admin_client_blocking()?;
    // create a group for our entity
    let group = generators::groups_blocking(1, &client)?.remove(0).name;
    // build and create an other entity
    let req = generators::gen_entity(&group, generators::gen_other_meta());
    let resp = client.entities.create(req)?;
    // delete the entity we just created
    client.entities.delete(resp.id)?;
    Ok(())
}

/// List the ids of every entity in a group listing, paging through the whole cursor
///
/// # Arguments
///
/// * `client` - The client to list entities with
/// * `opts` - The options to list entities with
async fn list_ids(
    client: &thorium::Thorium,
    opts: &thorium::models::EntityListOpts,
) -> Result<Vec<uuid::Uuid>, thorium::Error> {
    // get the first page of entities
    let mut cursor = client.entities.list_details(opts).await?;
    // collect the ids on every page until the cursor is exhausted
    let mut ids: Vec<uuid::Uuid> = cursor.data.drain(..).map(|entity| entity.id).collect();
    while !cursor.exhausted() {
        cursor.refill().await?;
        ids.extend(cursor.data.drain(..).map(|entity| entity.id));
    }
    Ok(ids)
}

/// Listing with one kind repeated many times returns the same entities, once each, as listing
/// with that kind once
#[tokio::test]
async fn list_repeated_kinds() -> Result<(), thorium::Error> {
    // set up an admin client and a group holding two other entities
    let (client, group) = common::setup().await?;
    for _ in 0..2 {
        let req = generators::gen_entity(&group, generators::gen_other_meta());
        client.entities.create(req).await?;
    }
    // list the group's other entities with the kind given once
    let base = thorium::models::EntityListOpts::default()
        .groups(vec![group.clone()])
        .limit(1000);
    let once = base.clone().kinds([thorium::models::EntityKinds::Other]);
    let expected = list_ids(&client, &once).await?;
    assert_eq!(expected.len(), 2, "listing with the kind once");
    // list them again with the kind repeated 99 times
    let repeated = base.kinds(vec![thorium::models::EntityKinds::Other; 99]);
    let listed = list_ids(&client, &repeated).await?;
    assert_eq!(listed, expected, "listing with the kind repeated");
    Ok(())
}

/// Verify a one page list saves no resumable cursor while a truncated list does
///
/// Every id hand-off gates on cursor exhaustion, so an exhausted first page's
/// cursor id can never be presented again; saving it would just orphan a TTL'd
/// redis key per bounded read (the hunt census pays this twice per poll tick).
#[tokio::test]
async fn one_page_list_saves_no_cursor() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a fresh member so their cursor key space scans clean
    let member = common::member(&client, &[&group]).await?;
    let username = member.users.info().await?.username;
    // create three entities for the lists to walk
    for _ in 0..3 {
        let req = generators::gen_entity(&group, generators::gen_other_meta());
        member.entities.create(req).await?;
    }
    // a list that fits one page must leave no cursor key behind
    let opts = thorium::models::EntityListOpts::default()
        .groups(vec![group.clone()])
        .limit(10);
    let page = member.entities.list_details(&opts).await?;
    assert!(
        page.data.len() >= 3,
        "the one page list must return every entity"
    );
    thorium::is!(test_utilities::cursor_keys(&username).await?.len(), 0);
    // a truncated list must still save its resumable cursor
    let opts = thorium::models::EntityListOpts::default()
        .groups(vec![group.clone()])
        .page_size(2);
    let mut cursor = member.entities.list_details(&opts).await?;
    thorium::is!(test_utilities::cursor_keys(&username).await?.len(), 1);
    // and paging it to exhaustion must still return everything
    let mut all = std::mem::take(&mut cursor.data);
    while !cursor.exhausted() {
        cursor.refill().await?;
        all.append(&mut cursor.data);
    }
    assert!(
        all.len() >= 3,
        "paging a truncated list must still reach every entity"
    );
    Ok(())
}
