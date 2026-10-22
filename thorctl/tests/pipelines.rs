//! Tests the thorctl pipelines commands against the test API

mod common;

use common::{TestUser, assert_success, unique};
use thorium::Error;
use thorium::models::{
    EntityMetadata, GroupRequest, GroupUsersRequest, PipelineRequest, ReactionRequest,
};
use thorium::test_utilities::generators;

/// `pipelines stage-tool` sets a stage's tool to follow its latest version or pins a version,
/// prints the API's refusals, and converts a pipeline so reactions can run on it
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn pipelines_stage_tool() -> Result<(), Error> {
    // a pipeline whose stages have no tools and a tool in the user's group
    let user = TestUser::new("pipelines-stage-tool").await?;
    let (tool, instance) = generators::tool(
        &[&user.group],
        "1.0.0",
        &generators::gen_image_spec(),
        &user.admin,
    )
    .await?;
    let (_, pinned) = generators::tool(
        &[&user.group],
        "1.0.0",
        &generators::gen_image_spec(),
        &user.admin,
    )
    .await?;
    let name = unique("pipe")[..20].to_owned();
    let req = PipelineRequest::new(
        &user.group,
        &name,
        serde_json::json!([["plant"], ["harvest"]]),
    )
    .stage_tool("plant", instance.id)
    .stage_tool("harvest", instance.id);
    user.client.pipelines.create(&req).await?;
    // setting a stage to the tool follows its latest version
    let args = [
        "pipelines",
        "stage-tool",
        &name,
        "plant",
        &tool.name,
        "-g",
        &user.group,
    ];
    assert_success(&user.run("pipelines-stage-tool-latest", &args).await);
    let read = user.client.pipelines.get(&user.group, &name).await?;
    assert_eq!(read.stage_tools.get("plant"), Some(&tool.id));
    // pinning a stage sets it to the tool instance
    let EntityMetadata::ToolInstance(pinned_meta) = &pinned.metadata else {
        panic!("expected a tool instance");
    };
    let pinned_tool = pinned_meta.tool.to_string();
    let pinned_id = pinned.id.to_string();
    let args = [
        "pipelines",
        "stage-tool",
        &name,
        "harvest",
        &pinned_tool,
        "--pin",
        &pinned_id,
        "-g",
        &user.group,
    ];
    assert_success(&user.run("pipelines-stage-tool-pin", &args).await);
    let read = user.client.pipelines.get(&user.group, &name).await?;
    assert_eq!(read.stage_tools.get("harvest"), Some(&pinned.id));
    // a stage that isn't in the pipeline prints the API's refusal
    let args = [
        "pipelines",
        "stage-tool",
        &name,
        "missing",
        &tool.name,
        "-g",
        &user.group,
    ];
    let out = user.run("pipelines-stage-tool-missing-stage", &args).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("aren't in the order: `missing`"),
        "{stderr}"
    );
    // a tool the user can see outside the pipeline's group prints the API's refusal
    let elsewhere = unique("ctlother");
    user.admin
        .groups
        .create(
            &GroupRequest::new(&elsewhere).owners(
                GroupUsersRequest::default()
                    .direct("thorium")
                    .direct(&user.username),
            ),
        )
        .await?;
    let (outside, _) = generators::tool(
        &[&elsewhere],
        "1.0.0",
        &generators::gen_image_spec(),
        &user.admin,
    )
    .await?;
    let outside_id = outside.id.to_string();
    let args = [
        "pipelines",
        "stage-tool",
        &name,
        "plant",
        &outside_id,
        "-g",
        &user.group,
    ];
    let out = user.run("pipelines-stage-tool-outside", &args).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("is not in the pipeline's group"),
        "{stderr}"
    );
    // with every stage set a reaction runs on the pipeline
    user.client
        .reactions
        .create(&ReactionRequest::new(&user.group, &name))
        .await?;
    Ok(())
}
