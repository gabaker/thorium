//! Integration tests for `FileSystem` entity CRUD

mod common;

use common::ProvenanceKind;
use test_utilities::generators;
use thorium::models::{EntityKinds, EntityMetadataUpdate};
use thorium::test_utilities;

/// How the shared provenance checks build filesystems
const FILESYSTEM: ProvenanceKind = ProvenanceKind {
    kind: EntityKinds::FileSystem,
    base_fields: || {
        vec![
            ("kind", "FileSystem".to_owned()),
            ("metadata[sha256]", generators::gen_string(64)),
        ]
    },
    stored_row: |tools| {
        serde_json::json!({"FileSystem": {"sha256": generators::gen_string(64), "tools": tools}})
            .to_string()
    },
    update: |add_tools, remove_tools, remove_legacy_tools| EntityMetadataUpdate::FileSystem {
        add_tools,
        remove_tools,
        remove_legacy_tools,
    },
};

/// Verify a filesystem entity can be created and matches its request
#[tokio::test]
async fn create() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a tool instance that found this filesystem
    let tool = generators::tool_instance(&[&group], &client).await?;
    // build a filesystem entity request and verify creation
    let req = generators::gen_entity(&group, generators::gen_filesystem_meta(vec![tool]));
    common::check_create(&client, &group, req).await
}

/// Verify a filesystem entity is updated as requested
#[tokio::test]
async fn update() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create the tool instances to remove and add
    let found = generators::tool_instance(&[&group], &client).await?;
    let added = generators::tool_instance(&[&group], &client).await?;
    // build a filesystem entity request and verify updates apply
    let req = generators::gen_entity(&group, generators::gen_filesystem_meta(vec![found]));
    common::check_update_meta(&client, req, |entity| {
        generators::gen_filesystem_update(entity, vec![added])
    })
    .await
}

/// Verify a filesystem entity can be deleted
#[tokio::test]
async fn delete() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // build a filesystem entity request and verify deletion
    let req = generators::gen_entity(&group, generators::gen_filesystem_meta(Vec::new()));
    common::check_delete(&client, req).await
}

/// A filesystem stored with tool names keeps them as legacy names through an update
#[tokio::test]
async fn legacy_tools_survive_update() -> Result<(), thorium::Error> {
    // get an admin client and check filesystems
    let client = test_utilities::admin_client().await?;
    common::check_legacy_survives_update(&client, &FILESYSTEM).await
}

/// Filesystem forms reject tool names and only remove legacy names
#[tokio::test]
async fn legacy_form_names() -> Result<(), thorium::Error> {
    // get an admin client and check filesystems
    let client = test_utilities::admin_client().await?;
    common::check_legacy_form_names(&client, &FILESYSTEM).await
}

/// Filesystem tool ids must be visible tool instances when they're added
#[tokio::test]
async fn tool_id_validation() -> Result<(), thorium::Error> {
    // get an admin client and check filesystems
    let client = test_utilities::admin_client().await?;
    common::check_tool_id_validation(&client, &FILESYSTEM).await
}

/// The client adds and removes a filesystem's tool instances
#[tokio::test]
async fn tool_id_updates() -> Result<(), thorium::Error> {
    // get an admin client and check filesystems
    let client = test_utilities::admin_client().await?;
    common::check_tool_id_updates(&client, &FILESYSTEM).await
}

/// A filesystem can be created with many tool instances at once
#[tokio::test]
async fn many_tool_ids() -> Result<(), thorium::Error> {
    // get an admin client and check filesystems
    let client = test_utilities::admin_client().await?;
    common::check_many_tool_ids(&client, &FILESYSTEM).await
}

/// Filesystems return tool instances as uuids and leave out empty legacy names
#[tokio::test]
async fn response_shape() -> Result<(), thorium::Error> {
    // get an admin client and check filesystems
    let client = test_utilities::admin_client().await?;
    common::check_response_shape(&client, &FILESYSTEM).await
}

/// A filesystem records each tool instance once however often it's sent
#[tokio::test]
async fn no_duplicate_tool_ids() -> Result<(), thorium::Error> {
    // get an admin client and check filesystems
    let client = test_utilities::admin_client().await?;
    common::check_no_duplicate_tool_ids(&client, &FILESYSTEM).await
}

/// A filesystem create checks every tool instance id past one lookup batch
#[tokio::test]
async fn tool_ids_past_one_batch() -> Result<(), thorium::Error> {
    // get an admin client and check filesystems
    let client = test_utilities::admin_client().await?;
    common::check_tool_ids_past_one_batch(&client, &FILESYSTEM).await
}
