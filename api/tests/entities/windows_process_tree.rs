//! Integration tests for `WindowsProcessTree` entity CRUD

mod common;

use common::ProvenanceKind;
use test_utilities::generators;
use thorium::models::{EntityKinds, EntityMetadataUpdate};
use thorium::test_utilities;

/// How the shared provenance checks build windows process trees
const PROCESS_TREE: ProvenanceKind = ProvenanceKind {
    kind: EntityKinds::WindowsProcessTree,
    base_fields: || vec![("kind", "WindowsProcessTree".to_owned())],
    stored_row: |tools| serde_json::json!({"WindowsProcessTree": {"tools": tools}}).to_string(),
    update: |add_tools, remove_tools, remove_legacy_tools| {
        EntityMetadataUpdate::WindowsProcessTree {
            add_tools,
            remove_tools,
            remove_legacy_tools,
        }
    },
};

/// Verify a windows process tree entity can be created and matches its request
#[tokio::test]
async fn create() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // build a windows process tree entity request and verify creation
    let req = generators::gen_entity(&group, generators::gen_windows_process_tree_meta());
    common::check_create(&client, &group, req).await
}

/// Verify a windows process tree entity is updated as requested
#[tokio::test]
async fn update() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create the tool instance to add
    let added = generators::tool_instance(&[&group], &client).await?;
    // build a windows process tree entity request and verify updates apply
    let req = generators::gen_entity(&group, generators::gen_windows_process_tree_meta());
    common::check_update_meta(&client, req, |entity| {
        generators::gen_windows_process_tree_update(entity, vec![added])
    })
    .await
}

/// Verify a windows process tree entity can be deleted
#[tokio::test]
async fn delete() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // build a windows process tree entity request and verify deletion
    let req = generators::gen_entity(&group, generators::gen_windows_process_tree_meta());
    common::check_delete(&client, req).await
}

/// A process tree stored with tool names keeps them as legacy names through an update
#[tokio::test]
async fn legacy_tools_survive_update() -> Result<(), thorium::Error> {
    // get an admin client and check process trees
    let client = test_utilities::admin_client().await?;
    common::check_legacy_survives_update(&client, &PROCESS_TREE).await
}

/// Process tree forms reject tool names and only remove legacy names
#[tokio::test]
async fn legacy_form_names() -> Result<(), thorium::Error> {
    // get an admin client and check process trees
    let client = test_utilities::admin_client().await?;
    common::check_legacy_form_names(&client, &PROCESS_TREE).await
}

/// Process tree tool ids must be visible tool instances when they're added
#[tokio::test]
async fn tool_id_validation() -> Result<(), thorium::Error> {
    // get an admin client and check process trees
    let client = test_utilities::admin_client().await?;
    common::check_tool_id_validation(&client, &PROCESS_TREE).await
}

/// The client adds and removes a process tree's tool instances
#[tokio::test]
async fn tool_id_updates() -> Result<(), thorium::Error> {
    // get an admin client and check process trees
    let client = test_utilities::admin_client().await?;
    common::check_tool_id_updates(&client, &PROCESS_TREE).await
}

/// A process tree can be created with many tool instances at once
#[tokio::test]
async fn many_tool_ids() -> Result<(), thorium::Error> {
    // get an admin client and check process trees
    let client = test_utilities::admin_client().await?;
    common::check_many_tool_ids(&client, &PROCESS_TREE).await
}

/// Process trees return tool instances as uuids and leave out empty legacy names
#[tokio::test]
async fn response_shape() -> Result<(), thorium::Error> {
    // get an admin client and check process trees
    let client = test_utilities::admin_client().await?;
    common::check_response_shape(&client, &PROCESS_TREE).await
}

/// A process tree records each tool instance once however often it's sent
#[tokio::test]
async fn no_duplicate_tool_ids() -> Result<(), thorium::Error> {
    // get an admin client and check process trees
    let client = test_utilities::admin_client().await?;
    common::check_no_duplicate_tool_ids(&client, &PROCESS_TREE).await
}

/// A process tree create checks every tool instance id past one lookup batch
#[tokio::test]
async fn tool_ids_past_one_batch() -> Result<(), thorium::Error> {
    // get an admin client and check process trees
    let client = test_utilities::admin_client().await?;
    common::check_tool_ids_past_one_batch(&client, &PROCESS_TREE).await
}
