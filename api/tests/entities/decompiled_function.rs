//! Integration tests for `DecompiledFunction` entity CRUD

mod common;

use common::ProvenanceKind;
use test_utilities::generators;
use thorium::models::{EntityKinds, EntityMetadataUpdate};
use thorium::test_utilities;

/// How the shared provenance checks build decompiled functions
const DECOMPILED_FUNCTION: ProvenanceKind = ProvenanceKind {
    kind: EntityKinds::DecompiledFunction,
    base_fields: || {
        vec![
            ("kind", "DecompiledFunction".to_owned()),
            ("metadata[function_address]", "4096".to_owned()),
            (
                "metadata[decompilation_content]",
                generators::gen_string(32),
            ),
        ]
    },
    stored_row: |tools| {
        serde_json::json!({
            "DecompiledFunction": {"address": 4096, "tools": tools, "content": "int main() {}"}
        })
        .to_string()
    },
    update: |add_tools, remove_tools, remove_legacy_tools| {
        EntityMetadataUpdate::DecompiledFunction {
            function_address: None,
            decompilation_content: None,
            add_tools,
            remove_tools,
            remove_legacy_tools,
        }
    },
};

/// Verify a decompiled function entity can be created and matches its request
#[tokio::test]
async fn create() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a tool instance that decompiled this function
    let tool = generators::tool_instance(&[&group], &client).await?;
    // build a decompiled function entity request and verify creation
    let req = generators::gen_entity(&group, generators::gen_decompiled_function_meta(vec![tool]));
    common::check_create(&client, &group, req).await
}

/// Verify a decompiled function entity is updated as requested
#[tokio::test]
async fn update() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create the tool instances to remove and add
    let found = generators::tool_instance(&[&group], &client).await?;
    let added = generators::tool_instance(&[&group], &client).await?;
    // build a decompiled function entity request and verify updates apply
    let req = generators::gen_entity(
        &group,
        generators::gen_decompiled_function_meta(vec![found]),
    );
    common::check_update_meta(&client, req, |entity| {
        generators::gen_decompiled_function_update(entity, vec![added])
    })
    .await
}

/// Verify a decompiled function entity can be deleted
#[tokio::test]
async fn delete() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // build a decompiled function entity request and verify deletion
    let req = generators::gen_entity(&group, generators::gen_decompiled_function_meta(Vec::new()));
    common::check_delete(&client, req).await
}

/// A decompiled function stored with tool names keeps them as legacy names through an update
#[tokio::test]
async fn legacy_tools_survive_update() -> Result<(), thorium::Error> {
    // get an admin client and check decompiled functions
    let client = test_utilities::admin_client().await?;
    common::check_legacy_survives_update(&client, &DECOMPILED_FUNCTION).await
}

/// Decompiled function forms reject tool names and only remove legacy names
#[tokio::test]
async fn legacy_form_names() -> Result<(), thorium::Error> {
    // get an admin client and check decompiled functions
    let client = test_utilities::admin_client().await?;
    common::check_legacy_form_names(&client, &DECOMPILED_FUNCTION).await
}

/// Decompiled function tool ids must be visible tool instances when they're added
#[tokio::test]
async fn tool_id_validation() -> Result<(), thorium::Error> {
    // get an admin client and check decompiled functions
    let client = test_utilities::admin_client().await?;
    common::check_tool_id_validation(&client, &DECOMPILED_FUNCTION).await
}

/// The client adds and removes a decompiled function's tool instances
#[tokio::test]
async fn tool_id_updates() -> Result<(), thorium::Error> {
    // get an admin client and check decompiled functions
    let client = test_utilities::admin_client().await?;
    common::check_tool_id_updates(&client, &DECOMPILED_FUNCTION).await
}

/// A decompiled function can be created with many tool instances at once
#[tokio::test]
async fn many_tool_ids() -> Result<(), thorium::Error> {
    // get an admin client and check decompiled functions
    let client = test_utilities::admin_client().await?;
    common::check_many_tool_ids(&client, &DECOMPILED_FUNCTION).await
}

/// Decompiled functions return tool instances as uuids and leave out empty legacy names
#[tokio::test]
async fn response_shape() -> Result<(), thorium::Error> {
    // get an admin client and check decompiled functions
    let client = test_utilities::admin_client().await?;
    common::check_response_shape(&client, &DECOMPILED_FUNCTION).await
}

/// A decompiled function records each tool instance once however often it's sent
#[tokio::test]
async fn no_duplicate_tool_ids() -> Result<(), thorium::Error> {
    // get an admin client and check decompiled functions
    let client = test_utilities::admin_client().await?;
    common::check_no_duplicate_tool_ids(&client, &DECOMPILED_FUNCTION).await
}

/// A decompiled function create checks every tool instance id past one lookup batch
#[tokio::test]
async fn tool_ids_past_one_batch() -> Result<(), thorium::Error> {
    // get an admin client and check decompiled functions
    let client = test_utilities::admin_client().await?;
    common::check_tool_ids_past_one_batch(&client, &DECOMPILED_FUNCTION).await
}
