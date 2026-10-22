//! Tests that new tools, toolsets, workflows, and pipelines can't take the name of an entity kind
//!
//! The create routes and the entity rename refuse the protected names with a `400` naming the
//! word and the list, names that merely contain a word are allowed, and a rename that keeps a
//! name works.

use serde_json::json;
use thorium::models::{
    EntityRequest, EntityUpdate, PipelineRequest, ToolSetEntityRequest,
    WorkflowCatalogEntityRequest,
};
use thorium::test_utilities::{self, generators};
use thorium::utils::protected_names::{PROTECTED_NAMES, refusal};
use thorium::{Error, fail, is};

/// Build a request to create a tool under a chosen name
///
/// # Arguments
///
/// * `name` - The tool's name
/// * `group` - The group to create the tool in
fn tool_named(name: &str, group: &str) -> EntityRequest {
    // take a generated tool request and set its name
    let mut req = generators::gen_tool(&[group], "1.0.0", &generators::gen_image_spec());
    name.clone_into(&mut req.name);
    req
}

/// Check that a result is a `400` that carries the refusal line for a name
///
/// # Arguments
///
/// * `result` - The result of the refused request
/// * `name` - The refused name
fn refused<T: std::fmt::Debug>(result: &Result<T, Error>, name: &str) -> Result<(), Error> {
    // the request failed with a 400 whose message is the refusal for this name
    fail!(result, 400, &refusal(name));
    Ok(())
}

/// Every create route refuses each protected name with a `400` that names the word and the list,
/// and nothing is created
#[tokio::test]
async fn create_routes_refuse_protected_names() -> Result<(), Error> {
    // a group and a tool a pipeline stage can follow
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let group = generators::god_group(
        &admin,
        "pnames",
        &suffix,
        &generators::GroupRoles::default(),
    )
    .await?;
    let (stage_tool, _) =
        generators::tool(&[&group], "1.0.0", &generators::gen_image_spec(), &admin).await?;
    // every word is refused by every kind of create route
    for word in PROTECTED_NAMES {
        // a pipeline name only takes lowercase letters, digits, and '-'
        let req =
            PipelineRequest::new(&group, word, json!([["only"]])).stage_tool("only", stage_tool.id);
        if word.contains('_') {
            // the pipeline character rule already refuses an underscore
            fail!(
                admin.pipelines.create(&req).await,
                400,
                "lowercase alphanumeric"
            );
        } else {
            refused(&admin.pipelines.create(&req).await, word)?;
        }
        // a tool, toolset, and workflow are refused with the same line
        refused(&admin.entities.create(tool_named(word, &group)).await, word)?;
        let toolset = generators::gen_toolset(word, &[&group], ToolSetEntityRequest::default());
        refused(&admin.entities.create(toolset).await, word)?;
        let workflow =
            generators::gen_workflow(word, &[&group], WorkflowCatalogEntityRequest::default());
        refused(&admin.entities.create(workflow).await, word)?;
        // none of them was created
        fail!(admin.pipelines.get(&group, word).await, 404);
    }
    // upper and mixed case entity names are refused the same way
    for name in ["TOOL", "Workflow", "Tool_Sets", "TOOL-SET"] {
        refused(&admin.entities.create(tool_named(name, &group)).await, name)?;
        let workflow =
            generators::gen_workflow(name, &[&group], WorkflowCatalogEntityRequest::default());
        refused(&admin.entities.create(workflow).await, name)?;
        let toolset = generators::gen_toolset(name, &[&group], ToolSetEntityRequest::default());
        refused(&admin.entities.create(toolset).await, name)?;
    }
    // an uppercase pipeline name keeps the lowercase rule's own refusal
    let req =
        PipelineRequest::new(&group, "Tool", json!([["only"]])).stage_tool("only", stage_tool.id);
    fail!(admin.pipelines.create(&req).await, 400, "lowercase");
    Ok(())
}

/// Names that only contain a protected word are created, and so are other entity words
#[tokio::test]
async fn names_containing_a_word_are_created() -> Result<(), Error> {
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let group = generators::god_group(
        &admin,
        "pnamesok",
        &suffix,
        &generators::GroupRoles::default(),
    )
    .await?;
    let (stage_tool, _) =
        generators::tool(&[&group], "1.0.0", &generators::gen_image_spec(), &admin).await?;
    // a pipeline named by an allowed neighbor
    for name in ["tool-pipeline", "my-pipeline", "workflows2"] {
        let req =
            PipelineRequest::new(&group, name, json!([["only"]])).stage_tool("only", stage_tool.id);
        admin.pipelines.create(&req).await?;
        is!(admin.pipelines.get(&group, name).await?.name, name);
    }
    // a tool, toolset, and workflow named by allowed neighbors
    for name in ["tool-runner", "my-toolset", "workflow-one"] {
        let tool = admin.entities.create(tool_named(name, &group)).await?;
        is!(admin.entities.get(tool.id).await?.name, name);
        let toolset = generators::gen_toolset(name, &[&group], ToolSetEntityRequest::default());
        let toolset = admin.entities.create(toolset).await?;
        is!(admin.entities.get(toolset.id).await?.name, name);
        let workflow =
            generators::gen_workflow(name, &[&group], WorkflowCatalogEntityRequest::default());
        let workflow = admin.entities.create(workflow).await?;
        is!(admin.entities.get(workflow.id).await?.name, name);
    }
    // the words for other entities stay allowed as names
    for name in ["agent", "model", "run", "instance"] {
        let workflow =
            generators::gen_workflow(name, &[&group], WorkflowCatalogEntityRequest::default());
        admin.entities.create(workflow).await?;
    }
    Ok(())
}

/// A rename can't take a protected name, an allowed rename works, and an update that repeats the
/// entity's own name keeps working
#[tokio::test]
async fn rename_refuses_protected_names() -> Result<(), Error> {
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let group = generators::god_group(
        &admin,
        "pnamesrn",
        &suffix,
        &generators::GroupRoles::default(),
    )
    .await?;
    // a tool, a toolset, and a workflow with ordinary names
    let (tool, _) =
        generators::tool(&[&group], "1.0.0", &generators::gen_image_spec(), &admin).await?;
    let toolset = admin
        .entities
        .create(generators::gen_toolset(
            "kitchen",
            &[&group],
            ToolSetEntityRequest::default(),
        ))
        .await?;
    let workflow = admin
        .entities
        .create(generators::gen_workflow(
            "triage",
            &[&group],
            WorkflowCatalogEntityRequest::default(),
        ))
        .await?;
    // renaming any of them to a protected name is refused and changes nothing
    for (id, old) in [
        (tool.id, tool.name.as_str()),
        (toolset.id, "kitchen"),
        (workflow.id, "triage"),
    ] {
        for word in ["tool", "Pipelines", "toolset"] {
            let update = EntityUpdate::default().name(word);
            refused(&admin.entities.update(id, update).await, word)?;
        }
        is!(admin.entities.get(id).await?.name, old);
        // an unprotected rename works, and so does an update that repeats the name
        let renamed = format!("{old}-renamed");
        admin
            .entities
            .update(id, EntityUpdate::default().name(&renamed))
            .await?;
        is!(admin.entities.get(id).await?.name, renamed);
        let again = EntityUpdate::default()
            .name(&renamed)
            .description("same name");
        admin.entities.update(id, again).await?;
    }
    Ok(())
}
