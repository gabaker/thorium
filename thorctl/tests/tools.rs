//! Tests the thorctl tools commands against the test API

mod common;

use common::{TestUser, assert_success, stdout, unique};
use serde_json::Value;
use thorium::Error;
use thorium::models::{
    EntityKinds, EntityListOpts, EntityMetadata, GroupRequest, GroupUsersRequest, ToolSpec,
};
use thorium::test_utilities::generators;

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
/// * `transcript` - A label for the run, which names what it checks
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
