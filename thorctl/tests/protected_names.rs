//! Tests that the create commands show the API's refusal of a protected name as one clean line

mod common;

use common::{TestUser, assert_success, unique};
use thorium::Error;
use thorium::test_utilities::generators;
use thorium::utils::protected_names::refusal;

/// The one line of stderr that holds a command's refusal, asserting only that one line mentions it
///
/// # Arguments
///
/// * `output` - The failed command's output
/// * `name` - The refused name
fn refusal_line(output: &std::process::Output, name: &str) -> String {
    // the command failed
    assert!(!output.status.success(), "{name} was accepted");
    // exactly one line carries the refusal, with the name and the protected list
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let lines: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains(&refusal(name)))
        .collect();
    assert_eq!(lines.len(), 1, "{stderr}");
    lines[0].to_owned()
}

/// `pipelines`, `tools`, `toolsets`, and `workflows create` refuse a protected name with the
/// API's reason on one line and create nothing, while a neighboring name is created
#[tokio::test]
async fn create_commands_show_the_protected_name_refusal() -> Result<(), Error> {
    // a user, a group, and a tool for a pipeline stage and a tool spec file
    let user = TestUser::new("protected-names").await?;
    let (scan, _) = generators::tool(
        &[&user.group],
        "1.0.0",
        &generators::gen_image_spec(),
        &user.admin,
    )
    .await?;
    let spec = std::env::temp_dir().join(format!("{}.json", unique("spec")));
    std::fs::write(&spec, serde_json::to_string(&generators::gen_image_spec())?)?;
    let spec = spec.to_string_lossy().into_owned();
    let stage_tool = format!("only={}", scan.name);
    // each command refuses the word with the API's line
    let args = [
        "pipelines",
        "create",
        "pipeline",
        "-g",
        &user.group,
        "--stage",
        "only",
        "--stage-tool",
        &stage_tool,
    ];
    refusal_line(&user.run("pnames-pipeline", &args).await, "pipeline");
    let args = [
        "tools",
        "create",
        "--spec",
        &spec,
        "-n",
        "tools",
        "-g",
        &user.group,
        "-v",
        "1.0.0",
    ];
    refusal_line(&user.run("pnames-tool", &args).await, "tools");
    let args = ["toolsets", "create", "toolset", "-g", &user.group];
    refusal_line(&user.run("pnames-toolset", &args).await, "toolset");
    let args = ["workflows", "create", "workflow", "-g", &user.group];
    refusal_line(&user.run("pnames-workflow", &args).await, "workflow");
    // a name that only contains the word is created by each command
    let args = [
        "pipelines",
        "create",
        "tool-pipeline",
        "-g",
        &user.group,
        "--stage",
        "only",
        "--stage-tool",
        &stage_tool,
    ];
    assert_success(&user.run("pnames-pipeline-ok", &args).await);
    let args = [
        "tools",
        "create",
        "--spec",
        &spec,
        "-n",
        "tool-runner",
        "-g",
        &user.group,
        "-v",
        "1.0.0",
    ];
    assert_success(&user.run("pnames-tool-ok", &args).await);
    let args = ["toolsets", "create", "my-toolset", "-g", &user.group];
    assert_success(&user.run("pnames-toolset-ok", &args).await);
    let args = ["workflows", "create", "my-workflow", "-g", &user.group];
    assert_success(&user.run("pnames-workflow-ok", &args).await);
    Ok(())
}
