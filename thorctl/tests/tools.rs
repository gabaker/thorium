//! Tests the thorctl tools commands against the test API

mod common;

use common::{TestUser, assert_success, stdout, unique};
use serde_json::Value;
use std::process::Stdio;
use thorium::Error;
use thorium::models::{
    EntityKinds, EntityListOpts, EntityMetadata, GroupRequest, GroupUsersRequest, ToolSpec,
};
use thorium::test_utilities::generators;
use tokio::io::AsyncWriteExt;

/// Write a spec to a file in a format
///
/// # Arguments
///
/// * `spec` - The spec to write
/// * `yaml` - Whether to write YAML instead of JSON
fn write_spec(spec: &ToolSpec, yaml: bool) -> String {
    // pick a unique path in the temp dir
    let ext = if yaml { "yaml" } else { "json" };
    let path = std::env::temp_dir().join(format!("{}.{ext}", unique("spec")));
    // write the spec in the requested format
    let raw = if yaml {
        // write the kind as the top-level key, the way people write specs
        let value = serde_json::to_value(spec).expect("failed to convert the spec");
        serde_norway::to_string(&value).expect("failed to write yaml")
    } else {
        serde_json::to_string_pretty(spec).expect("failed to write json")
    };
    std::fs::write(&path, raw).expect("failed to write the spec");
    path.to_string_lossy().into_owned()
}

/// Parse a command's stdout as JSON
///
/// # Arguments
///
/// * `output` - The command's output
fn json(output: &std::process::Output) -> Value {
    serde_json::from_str(&stdout(output)).expect("stdout was not JSON")
}

/// Change an agentic spec's description so it publishes as a new version
///
/// # Arguments
///
/// * `spec` - The spec to change
/// * `description` - The new description
fn described(spec: &ToolSpec, description: &str) -> ToolSpec {
    // copy the spec and change its description
    let mut changed = spec.clone();
    if let ToolSpec::Agentic(agentic) = &mut changed {
        description.clone_into(&mut agentic.model_description);
    }
    changed
}

/// Create a tool from a spec file with `tools create`, asserting it worked
///
/// # Arguments
///
/// * `user` - The user to run thorctl as
/// * `transcript` - The name of the run's transcript
/// * `spec` - The path to the spec file
/// * `name` - The name of the tool
/// * `group` - The group to create the tool in
async fn create(user: &TestUser, transcript: &str, spec: &str, name: &str, group: &str) {
    // create the tool's first version as 1.0.0
    let args = [
        "tools", "create", "--spec", spec, "-n", name, "-g", group, "-v", "1.0.0",
    ];
    assert_success(&user.run(transcript, &args).await);
}

/// `tools publish --provides` gives a new harness version its native tools, `tools harness-env`
/// sets and clears the harness's env, and `tools harness-models` disables and enables model ids,
/// neither of which publishes a version
#[tokio::test]
// one scenario walks every harness command against shared fixtures
#[allow(clippy::too_many_lines)]
async fn tools_harness_commands() -> Result<(), Error> {
    // a native tool and a harness created with thorctl
    let user = TestUser::new("tools-harness").await?;
    let (_, native) = generators::tool(
        &[&user.group],
        "1.0.0",
        &generators::gen_native_spec(),
        &user.client,
    )
    .await?;
    let name = unique("harness");
    let spec = generators::gen_harness_spec();
    create(
        &user,
        "tools-harness-create",
        &write_spec(&spec, true),
        &name,
        &user.group,
    )
    .await;
    // publishing a new version with --provides links it to the native tool
    let ToolSpec::Harness(mut next) = spec else {
        unreachable!()
    };
    next.interface.inputs.models = "/work/models.json".into();
    let next_path = write_spec(&ToolSpec::Harness(next), true);
    let native_id = native.id.to_string();
    let args = [
        "tools",
        "publish",
        &name,
        "-v",
        "1.1.0",
        "--spec",
        &next_path,
        "--provides",
        &native_id,
    ];
    assert_success(&user.run("tools-harness-publish-provides", &args).await);
    let described = json(
        &user
            .run(
                "tools-harness-describe",
                &["tools", "describe", &name, "-o", "json"],
            )
            .await,
    );
    let latest = described["metadata"]["Tool"]["latest"]
        .as_str()
        .expect("the harness has a latest version")
        .parse()
        .expect("the latest version is an id");
    let instance = user.client.entities.get(latest).await?;
    let EntityMetadata::ToolInstance(meta) = &instance.metadata else {
        panic!("expected a tool instance");
    };
    assert_eq!(meta.provides, vec![native.id]);
    let versions = user
        .client
        .entities
        .tool_instances(instance_tool(meta))
        .await?
        .len();
    // harness-env exports values under their standard or new names without publishing
    let args = [
        "tools",
        "harness-env",
        &name,
        "--set",
        "OpenAiApiKey=LITELLM_API_KEY",
        "--set",
        "ModelId",
    ];
    let out = user.run("tools-harness-env-set", &args).await;
    assert_success(&out);
    assert!(
        stdout(&out).contains("OpenAiApiKey=LITELLM_API_KEY"),
        "{}",
        stdout(&out)
    );
    let described = json(
        &user
            .run(
                "tools-harness-env-read",
                &["tools", "describe", &name, "-o", "json"],
            )
            .await,
    );
    let env = &described["metadata"]["Tool"]["harness_env"];
    assert_eq!(
        env["names"]["OpenAiApiKey"], "LITELLM_API_KEY",
        "{described}"
    );
    assert_eq!(
        env["export"],
        serde_json::json!(["ModelId", "OpenAiApiKey"]),
        "{described}"
    );
    // a reserved name prints the API's 400
    let args = [
        "tools",
        "harness-env",
        &name,
        "--set",
        "OpenAiApiKey=THORIUM_TOKEN",
    ];
    let out = user.run("tools-harness-env-reserved", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("400"));
    // --clear removes the env
    assert_success(
        &user
            .run(
                "tools-harness-env-clear",
                &["tools", "harness-env", &name, "--clear"],
            )
            .await,
    );
    let described = json(
        &user
            .run(
                "tools-harness-env-cleared",
                &["tools", "describe", &name, "-o", "json"],
            )
            .await,
    );
    assert!(
        described["metadata"]["Tool"].get("harness_env").is_none(),
        "{described}"
    );
    // harness-models disables and enables model ids, read back sorted
    let args = [
        "tools",
        "harness-models",
        &name,
        "--disable-model",
        "zeta-1",
        "--disable-model",
        "alpha-2",
    ];
    assert_success(&user.run("tools-harness-models-disable", &args).await);
    let args = ["tools", "harness-models", &name, "--enable-model", "zeta-1"];
    assert_success(&user.run("tools-harness-models-enable", &args).await);
    let described = json(
        &user
            .run(
                "tools-harness-models-read",
                &["tools", "describe", &name, "-o", "json"],
            )
            .await,
    );
    assert_eq!(
        described["metadata"]["Tool"]["disabled_models"],
        serde_json::json!(["alpha-2"]),
        "{described}"
    );
    // neither command published a version
    let after = user
        .client
        .entities
        .tool_instances(instance_tool(meta))
        .await?
        .len();
    assert_eq!(after, versions);
    Ok(())
}

/// Get the tool a tool instance belongs to
///
/// # Arguments
///
/// * `meta` - The tool instance's metadata
fn instance_tool(meta: &thorium::models::ToolInstanceEntity) -> uuid::Uuid {
    // an instance names its tool
    meta.tool
}

/// `tools create` reads YAML and JSON specs, `publish` adds versions that `versions` lists, and
/// unchanged specs or used labels print the API's 409
#[tokio::test]
async fn tools_create_and_publish() -> Result<(), Error> {
    // create one tool from a YAML spec and one from a JSON spec
    let user = TestUser::new("tools-create-publish").await?;
    let spec = generators::gen_agentic_spec();
    let name = unique("tool");
    let yaml_path = write_spec(&spec, true);
    create(&user, "tools-create-yaml", &yaml_path, &name, &user.group).await;
    let json_path = write_spec(&generators::gen_image_spec(), false);
    create(
        &user,
        "tools-create-json",
        &json_path,
        &unique("tool"),
        &user.group,
    )
    .await;
    // both tools have their first instance
    let args = ["tools", "get", "-g", &user.group, "-o", "json"];
    let tools = json(&user.run("tools-get-json", &args).await);
    let tools = tools.as_array().expect("expected a list of tools");
    assert_eq!(tools.len(), 2);
    for tool in tools {
        assert!(tool["metadata"]["Tool"]["latest"].is_string(), "{tool}");
    }
    // publishing a new version creates the next instance
    let next_path = write_spec(&described(&spec, "the next version"), true);
    let args = [
        "tools", "publish", &name, "-v", "1.1.0", "--spec", &next_path,
    ];
    assert_success(&user.run("tools-publish", &args).await);
    // versions lists both, newest first, with the new one marked latest
    let out = user
        .run("tools-versions", &["tools", "versions", &name])
        .await;
    assert_success(&out);
    let table = stdout(&out);
    let rows = table
        .lines()
        .filter(|line| line.contains("1.0.0") || line.contains("1.1.0"))
        .collect::<Vec<&str>>();
    assert_eq!(rows.len(), 2, "{table}");
    assert!(
        rows[0].contains("1.1.0") && rows[0].contains("latest"),
        "{table}"
    );
    assert!(!rows[1].contains("latest"), "{table}");
    // publishing an unchanged spec prints the API's 409 and fails
    let args = [
        "tools", "publish", &name, "-v", "1.2.0", "--spec", &next_path,
    ];
    let out = user.run("tools-publish-noop", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("409"));
    // publishing a used version label prints the API's 409 and fails
    let args = [
        "tools", "publish", &name, "-v", "1.0.0", "--spec", &yaml_path,
    ];
    let out = user.run("tools-publish-used-label", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("409"));
    Ok(())
}

/// `deprecate` moves latest back, `describe` shows a tool's details, and `get` lists one group
#[tokio::test]
async fn tools_deprecate_describe_get() -> Result<(), Error> {
    // create a tool with two versions
    let user = TestUser::new("tools-deprecate-describe-get").await?;
    let spec = generators::gen_agentic_spec();
    let name = unique("tool");
    let yaml_path = write_spec(&spec, true);
    create(&user, "tools-create", &yaml_path, &name, &user.group).await;
    let next_path = write_spec(&described(&spec, "the next version"), true);
    let args = [
        "tools", "publish", &name, "-v", "1.1.0", "--spec", &next_path,
    ];
    assert_success(&user.run("tools-publish", &args).await);
    // deprecating the latest instance moves latest back
    let args = ["tools", "versions", &name, "-o", "json"];
    let versions = json(&user.run("tools-versions-json", &args).await);
    let newest = versions[0]["id"].as_str().unwrap().to_owned();
    let oldest = versions[1]["id"].as_str().unwrap().to_owned();
    assert_success(
        &user
            .run("tools-deprecate", &["tools", "deprecate", &newest])
            .await,
    );
    let args = ["tools", "describe", &name, "-o", "json"];
    let tool = json(&user.run("tools-describe-json", &args).await);
    assert_eq!(
        tool["metadata"]["Tool"]["latest"],
        Value::String(oldest.clone())
    );
    // describe shows the kind, groups, latest, and instance count
    let out = user
        .run("tools-describe", &["tools", "describe", &name])
        .await;
    assert_success(&out);
    let text = stdout(&out);
    assert!(text.contains("Kind: Agentic"), "{text}");
    assert!(text.contains(&user.group), "{text}");
    assert!(
        text.contains(&format!("Latest: 1.0.0 ({oldest})")),
        "{text}"
    );
    assert!(text.contains("Instances: 2"), "{text}");
    // a tool in another group isn't listed with our group
    let other = unique("ctlgroup");
    let owners = GroupUsersRequest::default()
        .direct("thorium")
        .direct(&user.username);
    let req = GroupRequest::new(&other).owners(owners);
    user.admin.groups.create(&req).await?;
    let other_name = unique("tool");
    create(&user, "tools-create-other", &yaml_path, &other_name, &other).await;
    let args = ["tools", "get", "-g", &other, "-o", "json"];
    let listed = json(&user.run("tools-get-group", &args).await);
    let names = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect::<Vec<String>>();
    assert_eq!(names, vec![other_name]);
    // the tool read back through the client agrees with the commands
    let tool_id = tool["id"].as_str().unwrap().parse()?;
    let EntityMetadata::Tool(meta) = user.client.entities.get(tool_id).await?.metadata else {
        panic!("expected a tool");
    };
    assert_eq!(meta.latest.map(|id| id.to_string()), Some(oldest));
    Ok(())
}

/// `tools get` lists past the first page up to its limit
#[tokio::test]
async fn tools_get_pages() -> Result<(), Error> {
    // 51 tools, more than one page holds
    let user = TestUser::new("tools-get-pages").await?;
    for _ in 0..51 {
        let spec = generators::gen_agentic_spec();
        generators::tool(&[&user.group], "1.0.0", &spec, &user.client).await?;
    }
    // a limit above a page lists all of them
    let args = [
        "tools",
        "get",
        "-g",
        &user.group,
        "--limit",
        "60",
        "-o",
        "json",
    ];
    let tools = json(&user.run("tools-get-pages", &args).await);
    assert_eq!(tools.as_array().map(Vec::len), Some(51));
    // a limit below the count lists only that many
    let args = [
        "tools",
        "get",
        "-g",
        &user.group,
        "--limit",
        "7",
        "-o",
        "json",
    ];
    let tools = json(&user.run("tools-get-limited", &args).await);
    assert_eq!(tools.as_array().map(Vec::len), Some(7));
    Ok(())
}

/// Tool commands given the ID of an entity that isn't a tool refuse it
#[tokio::test]
async fn tools_reject_other_kinds() -> Result<(), Error> {
    // an entity of another kind
    let user = TestUser::new("tools-reject-other-kinds").await?;
    let req = generators::gen_entity(&user.group, generators::gen_device_meta(Vec::new()));
    let (_, device) = generators::entity(req, &user.client).await?;
    let id = device.id.to_string();
    // describing it as a tool fails and names its kind
    let out = user
        .run("tools-describe-device", &["tools", "describe", &id])
        .await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("is a Device, not a tool"), "{stderr}");
    // publishing to it fails without changing it
    let spec = write_spec(&generators::gen_agentic_spec(), true);
    let args = ["tools", "publish", &id, "-v", "2.0.0", "--spec", &spec];
    let out = user.run("tools-publish-device", &args).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("is a Device, not a tool"), "{stderr}");
    // the device is unchanged
    let read = user.client.entities.get(device.id).await?;
    assert_eq!(read.name, device.name);
    assert_eq!(read.description, device.description);
    assert_eq!(
        serde_json::to_value(&read.metadata)?,
        serde_json::to_value(&device.metadata)?
    );
    // and no tool instance was created in its group
    let opts = EntityListOpts::default()
        .groups(vec![user.group.clone()])
        .kinds([EntityKinds::ToolInstance])
        .limit(10);
    assert_eq!(user.client.entities.list(&opts).await?.data.len(), 0);
    Ok(())
}

/// A YAML tool spec that sets every model-API tool field and has a lossy schema
const MODEL_API_SPEC: &str = r#"Agentic:
  function_name: lossy_scan
  title: Lossy scan
  model_description: Scan a sample and keep free-form metadata
  input_schema:
    type: object
    properties:
      sha256: {type: string}
      meta: {type: object, properties: {}, additionalProperties: {type: string}}
    required: [sha256]
  backing:
    ThoriumMcp:
      tool: get_sample
  timeout: 60
  capabilities:
    open_world: true
  input_examples:
    - sha256: "0000"
  strict: Auto
  loading: Deferred
  icons:
    - src: https://example.com/scan.png
      mime_type: image/png
      sizes: ["48x48"]
"#;

/// Write raw spec text to a file
///
/// # Arguments
///
/// * `raw` - The spec text
fn write_raw_spec(raw: &str) -> String {
    // pick a unique path in the temp dir and write the text
    let path = std::env::temp_dir().join(format!("{}.yaml", unique("spec")));
    std::fs::write(&path, raw).expect("failed to write the spec");
    path.to_string_lossy().into_owned()
}

/// `tools create` takes the model-API fields from YAML and prints the portability warnings,
/// `publish` and `describe` print the report, and an unknown spec key is refused before any request
#[tokio::test]
async fn tools_model_api_fields() -> Result<(), Error> {
    // a developer and a group named for this test
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "tools-model-api-fields",
        &generators::readable_name("europa", &suffix),
        &generators::readable_name("minerva", &suffix),
    )
    .await?;
    let name = generators::readable_name("lossy-scan", &suffix);
    // create prints the warnings
    let spec = write_raw_spec(MODEL_API_SPEC);
    let args = [
        "tools",
        "create",
        "--spec",
        &spec,
        "-n",
        &name,
        "-g",
        &user.group,
        "-v",
        "1.0.0",
    ];
    let out = user.run("tools-create-model-api", &args).await;
    assert_success(&out);
    let text = stdout(&out);
    println!("{text}");
    assert!(text.contains("Gemini"), "{text}");
    assert!(text.contains("additionalProperties"), "{text}");
    // describe shows the fields and the report
    let out = user
        .run("tools-describe-model-api", &["tools", "describe", &name])
        .await;
    assert_success(&out);
    let text = stdout(&out);
    println!("{text}");
    for expected in [
        "Function: lossy_scan",
        "Strict: Auto",
        "Loading: Deferred",
        "Open world: true",
        "Input examples: 1",
        "Icons: 1",
        "Strict compatible: no",
        "Gemini compatible: no",
    ] {
        assert!(text.contains(expected), "missing `{expected}` in:\n{text}");
    }
    // the JSON view carries the latest instance and its report
    let args = ["tools", "describe", &name, "-o", "json"];
    let described = json(&user.run("tools-describe-model-api-json", &args).await);
    let instance = &described["latest_instance"]["metadata"]["ToolInstance"];
    assert_eq!(instance["spec"]["Agentic"]["loading"], "Deferred");
    assert_eq!(instance["portability"]["gemini_compatible"], false);
    // publish prints the new version's report
    let next = write_raw_spec(&MODEL_API_SPEC.replace("keep free-form", "keep any"));
    let args = ["tools", "publish", &name, "-v", "1.1.0", "--spec", &next];
    let out = user.run("tools-publish-model-api", &args).await;
    assert_success(&out);
    let text = stdout(&out);
    assert!(text.contains("Gemini"), "{text}");
    // a spec with an unknown key is refused by thorctl, naming the key
    let unknown = write_raw_spec(&MODEL_API_SPEC.replace("strict: Auto", "strictness: On"));
    let other = generators::readable_name("unknown-key", &suffix);
    let args = [
        "tools",
        "create",
        "--spec",
        &unknown,
        "-n",
        &other,
        "-g",
        &user.group,
        "-v",
        "1.0.0",
    ];
    let out = user.run("tools-create-unknown-key", &args).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    println!("{stderr}");
    assert!(stderr.contains("strictness"), "{stderr}");
    // and nothing was created
    let opts = EntityListOpts::default()
        .groups(vec![user.group.clone()])
        .kinds([EntityKinds::Tool])
        .limit(10);
    let names = user
        .client
        .entities
        .list(&opts)
        .await?
        .data
        .into_iter()
        .map(|line| line.name)
        .collect::<Vec<String>>();
    assert_eq!(names, vec![name]);
    Ok(())
}

/// Run `tools create` with the spec piped to stdin
///
/// # Arguments
///
/// * `user` - The user to run thorctl as
/// * `args` - The `tools create` arguments after the global flags
/// * `spec` - The spec to write to stdin
async fn create_from_stdin(
    user: &TestUser,
    args: &[&str],
    spec: &ToolSpec,
) -> std::process::Output {
    // start thorctl with a pipe for stdin and write the spec to it
    let mut child = user
        .command(args)
        .stdin(Stdio::piped())
        .spawn()
        .expect("failed to start thorctl");
    let value = serde_json::to_value(spec).expect("failed to convert the spec");
    let raw = serde_norway::to_string(&value).expect("failed to write yaml");
    child
        .stdin
        .take()
        .expect("no stdin")
        .write_all(raw.as_bytes())
        .await
        .expect("failed to write stdin");
    child.wait_with_output().await.expect("thorctl failed")
}

/// The `key=value` tags of a tool, without the one Thorium sets, sorted
///
/// # Arguments
///
/// * `tool` - The tool as JSON
fn user_tags(tool: &Value) -> Vec<String> {
    // each tag key has its values as keys of an object
    let mut tags = Vec::new();
    for (key, values) in tool["tags"].as_object().into_iter().flatten() {
        if key == "submitter" {
            continue;
        }
        for value in values
            .as_object()
            .into_iter()
            .flatten()
            .map(|(value, _)| value)
        {
            tags.push(format!("{key}={value}"));
        }
    }
    tags.sort();
    tags
}

/// `tools create` puts tags on the tool, reads a spec from stdin, checks the spec with a dry run
/// without creating anything, sets a harness's env, disabled models and native tools, and refuses
/// harness settings on another kind of tool
#[tokio::test]
// one scenario walks every create flag against shared fixtures
#[allow(clippy::too_many_lines)]
async fn tools_create_extras() -> Result<(), Error> {
    // a tool created from stdin with tags
    let user = TestUser::new("tools-create-extras").await?;
    let name = unique("tool");
    let spec = generators::gen_agentic_spec();
    let args = [
        "tools",
        "create",
        "--spec",
        "-",
        "-n",
        &name,
        "-g",
        &user.group,
        "-v",
        "1.0.0",
        "--tag",
        "env=prod",
        "--tag",
        "env=dev",
        "--tag",
        "team=a",
    ];
    let out = create_from_stdin(&user, &args, &spec).await;
    assert_success(&out);
    let args = ["tools", "describe", &name, "-o", "json"];
    let tool = json(&user.run("tools-create-extras-describe", &args).await);
    assert_eq!(user_tags(&tool), vec!["env=dev", "env=prod", "team=a"]);
    // a dry run creates nothing
    let dry = unique("tool");
    let path = write_spec(&spec, true);
    let args = [
        "tools",
        "create",
        "--spec",
        &path,
        "-n",
        &dry,
        "-g",
        &user.group,
        "-v",
        "1.0.0",
        "--dry-run",
    ];
    let out = user.run("tools-create-dry-run", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("Would create"), "{}", stdout(&out));
    let out = user
        .run("tools-create-dry-run-gone", &["tools", "describe", &dry])
        .await;
    assert!(!out.status.success());
    // a dry run still refuses a spec that isn't one
    let bad = std::env::temp_dir().join(format!("{}.yaml", unique("bad")));
    std::fs::write(&bad, "Agentic:\n  colour: red\n").expect("failed to write the spec");
    let bad = bad.to_string_lossy().into_owned();
    let args = [
        "tools",
        "create",
        "--spec",
        &bad,
        "-n",
        &dry,
        "-g",
        &user.group,
        "-v",
        "1.0.0",
        "--dry-run",
    ];
    assert!(
        !user
            .run("tools-create-dry-run-bad", &args)
            .await
            .status
            .success()
    );
    // harness settings on an agentic tool are refused and nothing is created
    let args = [
        "tools",
        "create",
        "--spec",
        &path,
        "-n",
        &dry,
        "-g",
        &user.group,
        "-v",
        "1.0.0",
        "--disable-model",
        "gpt-5",
    ];
    let out = user.run("tools-create-harness-only", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("harness"));
    // a harness gets its env, disabled models and native tool at creation
    let (_, native) = generators::tool(
        &[&user.group],
        "1.0.0",
        &generators::gen_native_spec(),
        &user.client,
    )
    .await?;
    let harness = unique("harness");
    let harness_path = write_spec(&generators::gen_harness_spec(), true);
    let native_id = native.id.to_string();
    let args = [
        "tools",
        "create",
        "--spec",
        &harness_path,
        "-n",
        &harness,
        "-g",
        &user.group,
        "-v",
        "1.0.0",
        "--harness-env",
        "ModelId",
        "--harness-env",
        "OpenAiApiKey=LITELLM_API_KEY",
        "--disable-model",
        "gpt-5",
        "--provides",
        &native_id,
    ];
    assert_success(&user.run("tools-create-harness", &args).await);
    let args = ["tools", "describe", &harness, "-o", "json"];
    let described = json(&user.run("tools-create-harness-describe", &args).await);
    let meta = &described["metadata"]["Tool"];
    assert_eq!(
        meta["harness_env"]["export"],
        serde_json::json!(["ModelId", "OpenAiApiKey"])
    );
    assert_eq!(
        meta["harness_env"]["names"]["OpenAiApiKey"],
        "LITELLM_API_KEY"
    );
    assert_eq!(meta["disabled_models"], serde_json::json!(["gpt-5"]));
    let latest = meta["latest"].as_str().expect("a latest version").parse()?;
    let instance = user.client.entities.get(latest).await?;
    let EntityMetadata::ToolInstance(instance) = &instance.metadata else {
        panic!("expected a tool instance");
    };
    assert_eq!(instance.provides, vec![native.id]);
    // a tag needs a value
    let args = [
        "tools",
        "create",
        "--spec",
        &path,
        "-n",
        &dry,
        "-g",
        &user.group,
        "-v",
        "1.0.0",
        "--tag",
        "novalue",
    ];
    assert!(
        !user
            .run("tools-create-bad-tag", &args)
            .await
            .status
            .success()
    );
    Ok(())
}

/// `tools get` filters on kind and tags and lists past its limit with `--no-limit`
#[tokio::test]
// one scenario walks every filter against shared fixtures
#[allow(clippy::too_many_lines)]
async fn tools_get_filters() -> Result<(), Error> {
    // tools of two kinds, one with tags
    let user = TestUser::new("tools-get-filters").await?;
    let agentic = unique("agentic");
    let image = unique("image");
    let agentic_path = write_spec(&generators::gen_agentic_spec(), true);
    let args = [
        "tools",
        "create",
        "--spec",
        &agentic_path,
        "-n",
        &agentic,
        "-g",
        &user.group,
        "-v",
        "1.0.0",
        "--tag",
        "env=prod",
        "--tag",
        "team=a",
    ];
    assert_success(&user.run("tools-get-filters-agentic", &args).await);
    create(
        &user,
        "tools-get-filters-image",
        &write_spec(&generators::gen_image_spec(), true),
        &image,
        &user.group,
    )
    .await;
    // the kind filter keeps one kind
    let args = [
        "tools",
        "get",
        "-g",
        &user.group,
        "--kind",
        "image",
        "-o",
        "json",
    ];
    let listed = json(&user.run("tools-get-kind", &args).await);
    let names = |listed: &Value| {
        listed
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect::<Vec<String>>()
    };
    assert_eq!(names(&listed), vec![image.clone()]);
    // the tag filter keeps tools with every tag given
    let args = [
        "tools",
        "get",
        "-g",
        &user.group,
        "--tag",
        "env=prod",
        "-o",
        "json",
    ];
    let listed = json(&user.run("tools-get-tag", &args).await);
    assert_eq!(names(&listed), vec![agentic.clone()]);
    let args = [
        "tools",
        "get",
        "-g",
        &user.group,
        "--tag",
        "env=prod",
        "--tag",
        "team=b",
        "-o",
        "json",
    ];
    let listed = json(&user.run("tools-get-tags-unmatched", &args).await);
    assert_eq!(names(&listed), Vec::<String>::new());
    // the filters combine
    let args = [
        "tools",
        "get",
        "-g",
        &user.group,
        "--kind",
        "image",
        "--tag",
        "env=prod",
        "-o",
        "json",
    ];
    let listed = json(&user.run("tools-get-kind-and-tag", &args).await);
    assert_eq!(names(&listed), Vec::<String>::new());
    // without a limit every tool is listed, and with the default limit only one page's worth
    for _ in 0..50 {
        let spec = generators::gen_agentic_spec();
        generators::tool(&[&user.group], "1.0.0", &spec, &user.client).await?;
    }
    let args = [
        "tools",
        "get",
        "-g",
        &user.group,
        "--no-limit",
        "-o",
        "json",
    ];
    let listed = json(&user.run("tools-get-no-limit", &args).await);
    assert_eq!(names(&listed).len(), 52);
    let args = ["tools", "get", "-g", &user.group, "-o", "json"];
    let listed = json(&user.run("tools-get-default-limit", &args).await);
    assert_eq!(names(&listed).len(), 50);
    // a limit and no limit conflict
    let args = ["tools", "get", "--limit", "3", "--no-limit"];
    assert!(
        !user
            .run("tools-get-limit-conflict", &args)
            .await
            .status
            .success()
    );
    Ok(())
}

/// `tools deprecate --undo` restores a deprecated version as the latest
#[tokio::test]
async fn tools_deprecate_undo() -> Result<(), Error> {
    // a tool with two versions
    let user = TestUser::new("tools-deprecate-undo").await?;
    let spec = generators::gen_agentic_spec();
    let name = unique("tool");
    create(
        &user,
        "tools-undo-create",
        &write_spec(&spec, true),
        &name,
        &user.group,
    )
    .await;
    let next = write_spec(&described(&spec, "the next version"), true);
    let args = ["tools", "publish", &name, "-v", "1.1.0", "--spec", &next];
    assert_success(&user.run("tools-undo-publish", &args).await);
    let versions = json(
        &user
            .run(
                "tools-undo-versions",
                &["tools", "versions", &name, "-o", "json"],
            )
            .await,
    );
    let newest = versions[0]["id"].as_str().unwrap().to_owned();
    // deprecating the newest moves latest back, and --undo moves it forward again
    assert_success(
        &user
            .run("tools-undo-deprecate", &["tools", "deprecate", &newest])
            .await,
    );
    let out = user
        .run(
            "tools-undo-describe",
            &["tools", "describe", &name, "-o", "json"],
        )
        .await;
    assert_ne!(
        json(&out)["metadata"]["Tool"]["latest"],
        Value::String(newest.clone())
    );
    let out = user
        .run("tools-undo", &["tools", "deprecate", &newest, "--undo"])
        .await;
    assert_success(&out);
    assert!(stdout(&out).contains("Restored"), "{}", stdout(&out));
    let out = user
        .run(
            "tools-undo-describe-again",
            &["tools", "describe", &name, "-o", "json"],
        )
        .await;
    assert_eq!(
        json(&out)["metadata"]["Tool"]["latest"],
        Value::String(newest)
    );
    Ok(())
}

/// Convert a stored image the way `tools snapshot` and the migration of stored images do
///
/// # Arguments
///
/// * `image` - The stored image
fn converted(image: &thorium::models::Image) -> thorium::client::migrations::ImageMigration {
    // an image's tool is found by the image's own name
    let map = thorium::client::migrations::ToolIdMap::from_images([image]);
    let ctx = thorium::client::migrations::ImageContext {
        creator_in_group: true,
        last_modified: None,
        map: &map,
    };
    thorium::client::migrations::convert_image(image, &ctx).expect("the image converts")
}

/// Read a tool's latest spec through `tools spec`
///
/// # Arguments
///
/// * `user` - The user to run thorctl as
/// * `name` - The tool's name
async fn spec_of(user: &TestUser, name: &str) -> ToolSpec {
    // print the spec as JSON and parse it
    let out = user
        .run("spec-of", &["tools", "spec", name, "--format", "json"])
        .await;
    assert_success(&out);
    serde_json::from_value(json(&out)).expect("a tool spec")
}

/// Build a request for an image with nothing but a container image to run
///
/// # Arguments
///
/// * `group` - The group the image is in
fn plain_image(group: &str) -> thorium::models::ImageRequest {
    // the conversion of an image with dependency lists needs the tools they name, so use none
    thorium::models::ImageRequest::new(group.to_owned(), generators::gen_string(20))
        .version(thorium::models::ToolVersion::from("1.0.0"))
        .image("registry.local/scan:1")
}

/// `tools snapshot` creates an Image tool whose spec is the migration's conversion of the image,
/// for the images of each environment, and never creates anything on a dry run
#[tokio::test]
async fn tools_snapshot_converts_images() -> Result<(), Error> {
    // an image in each of two environments the conversion supports
    let user = TestUser::new("tools-snapshot").await?;
    thorium::test_utilities::faults::allow_image_writes(&user.group);
    let k8s = plain_image(&user.group);
    user.client.images.create(&k8s).await?;
    let bare = plain_image(&user.group).scaler(thorium::models::ImageScaler::BareMetal);
    user.client.images.create(&bare).await?;
    for request in [&k8s, &bare] {
        let image = user.client.images.get(&user.group, &request.name).await?;
        let target = format!("{}/{}", user.group, request.name);
        // a dry run says what would be created and creates nothing
        let args = ["tools", "snapshot", &target, "--dry-run"];
        let out = user.run("tools-snapshot-dry-run", &args).await;
        assert_success(&out);
        assert!(stdout(&out).contains("Would create"), "{}", stdout(&out));
        let out = user
            .run(
                "tools-snapshot-dry-run-gone",
                &["tools", "describe", &request.name],
            )
            .await;
        assert!(!out.status.success());
        // the snapshot keeps the image's name, group and version, and its spec is the conversion's
        let out = user
            .run("tools-snapshot", &["tools", "snapshot", &target])
            .await;
        assert_success(&out);
        assert!(stdout(&out).contains("Created tool"), "{}", stdout(&out));
        let tool = json(
            &user
                .run(
                    "tools-snapshot-describe",
                    &["tools", "describe", &request.name, "-o", "json"],
                )
                .await,
        );
        assert_eq!(tool["groups"], serde_json::json!([user.group]));
        assert_eq!(
            spec_of(&user, &request.name).await,
            converted(&image).tool.spec
        );
        let versions = json(
            &user
                .run(
                    "tools-snapshot-versions",
                    &["tools", "versions", &request.name, "-o", "json"],
                )
                .await,
        );
        assert_eq!(versions[0]["version"]["SemVer"], "1.0.0");
    }
    Ok(())
}

/// `tools snapshot` never replaces an existing tool, takes another name, version and group, and
/// refuses an image that doesn't exist or can't be converted
#[tokio::test]
async fn tools_snapshot_conflicts_and_options() -> Result<(), Error> {
    // an image and a second group
    let user = TestUser::new("tools-snapshot-options").await?;
    thorium::test_utilities::faults::allow_image_writes(&user.group);
    let request = plain_image(&user.group);
    user.client.images.create(&request).await?;
    let target = format!("{}/{}", user.group, request.name);
    assert_success(
        &user
            .run("tools-snapshot-first", &["tools", "snapshot", &target])
            .await,
    );
    // a second snapshot under the same name is refused and creates nothing
    let out = user
        .run("tools-snapshot-again", &["tools", "snapshot", &target])
        .await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("already exists"));
    let args = ["tools", "get", "-g", &user.group, "-o", "json"];
    let listed = json(&user.run("tools-snapshot-count", &args).await);
    assert_eq!(listed.as_array().map(Vec::len), Some(1));
    // another name and version make a second tool
    let args = ["tools", "snapshot", &target, "-n", "second", "-v", "9.9.9"];
    assert_success(&user.run("tools-snapshot-renamed", &args).await);
    let versions = json(
        &user
            .run(
                "tools-snapshot-renamed-versions",
                &["tools", "versions", "second", "-o", "json"],
            )
            .await,
    );
    assert_eq!(versions[0]["version"]["SemVer"], "9.9.9");
    // an image that doesn't exist is the API's refusal
    let missing = format!("{}/{}", user.group, unique("noimage"));
    let out = user
        .run("tools-snapshot-missing", &["tools", "snapshot", &missing])
        .await;
    assert!(!out.status.success());
    // a target that isn't GROUP/IMAGE is refused
    let out = user
        .run(
            "tools-snapshot-bad-target",
            &["tools", "snapshot", "noslash"],
        )
        .await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("GROUP/IMAGE"));
    // an image whose dependency lists name no tool is skipped by the conversion, and nothing is
    // created
    let empty = generators::gen_image(&user.group);
    user.client.images.create(&empty).await?;
    let target = format!("{}/{}", user.group, empty.name);
    let out = user
        .run(
            "tools-snapshot-unconvertible",
            &["tools", "snapshot", &target],
        )
        .await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("can't become a tool"));
    let out = user
        .run(
            "tools-snapshot-unconvertible-gone",
            &["tools", "describe", &empty.name],
        )
        .await;
    assert!(!out.status.success());
    Ok(())
}
