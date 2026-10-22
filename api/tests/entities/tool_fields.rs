//! Integration tests for the model-API tool fields and the portability rules
//!
//! These cover creating agentic tools that set input examples, strictness, loading, icons, and
//! `open_world`, the refusals the portability rules add, the portability report the API returns
//! without storing it, and stored instances that predate the rules.

use serde_json::{Value, json};
use thorium::models::{
    AgenticTool, Entity, EntityMetadata, EntityResponse, ToolIcon, ToolIconTheme,
    ToolInstanceEntity, ToolLoading, ToolPortabilityReport, ToolResultFormat, ToolSpec,
    ToolStrictness,
};
use thorium::test_utilities::generators::{GroupRoles, god_group};
use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium, fail, is};

/// Build an MCP-backed agentic tool with a function name and an input schema
///
/// # Arguments
///
/// * `function_name` - The name a model calls the tool by
/// * `input_schema` - The tool's input schema
fn agentic(function_name: &str, input_schema: &Value) -> AgenticTool {
    // start from a generated MCP tool and set the name and schema
    let ToolSpec::Agentic(mut tool) = generators::gen_agentic_spec() else {
        unreachable!()
    };
    function_name.clone_into(&mut tool.function_name);
    tool.input_schema
        .clone_from(input_schema.as_object().unwrap());
    tool
}

/// A schema whose sha256 property is required
fn required_schema() -> Value {
    // one required string property
    json!({"type": "object", "properties": {"sha256": {"type": "string"}}, "required": ["sha256"]})
}

/// A schema with a property for each lossy Gemini rewrite and one strict issue
fn lossy_schema() -> Value {
    // additionalProperties, a $ref, a oneOf, and a const
    json!({
        "type": "object",
        "properties": {
            "meta": {"type": "object", "properties": {}, "additionalProperties": {"type": "string"}},
            "target": {"$ref": "#/$defs/target"},
            "mode": {"oneOf": [{"type": "string"}, {"type": "integer"}]},
            "kind": {"const": "scan"},
        },
        "$defs": {"target": {"type": "string"}},
    })
}

/// Create a tool in a group and return the create response
///
/// # Arguments
///
/// * `client` - The client to create the tool with
/// * `group` - The group to create the tool in
/// * `tool` - The agentic tool to create
async fn create(
    client: &Thorium,
    group: &str,
    tool: &AgenticTool,
) -> Result<EntityResponse, Error> {
    // create the tool's first version as 1.0.0
    let req = generators::gen_tool(&[group], "1.0.0", &ToolSpec::Agentic(tool.clone()));
    client.entities.create(req).await
}

/// Assert that creating a tool is refused with a 400 whose message holds some text
///
/// # Arguments
///
/// * `client` - The client to create the tool with
/// * `group` - The group to create the tool in
/// * `tool` - The agentic tool to create
/// * `expected` - Text the refusal must hold
async fn refused(
    client: &Thorium,
    group: &str,
    tool: &AgenticTool,
    expected: &str,
) -> Result<(), Error> {
    // the create is refused
    let resp = create(client, group, tool).await;
    fail!(resp, 400);
    // the refusal names the problem
    let msg = resp.unwrap_err().msg().unwrap_or_default();
    assert!(msg.contains(expected), "expected `{expected}` in: {msg}");
    println!("refused {}: {msg}", tool.function_name);
    Ok(())
}

/// Get a tool instance's metadata from an entity
///
/// # Arguments
///
/// * `entity` - The tool instance entity
fn instance_meta(entity: &Entity) -> &ToolInstanceEntity {
    // only tool instances have tool instance metadata
    match &entity.metadata {
        EntityMetadata::ToolInstance(instance) => instance,
        other => panic!("expected a tool instance, got {other:?}"),
    }
}

/// Get an instance's agentic spec
///
/// # Arguments
///
/// * `entity` - The tool instance entity
fn agentic_spec(entity: &Entity) -> &AgenticTool {
    // only agentic instances have an agentic spec
    match &instance_meta(entity).spec {
        ToolSpec::Agentic(tool) => tool,
        ToolSpec::Image(_) | ToolSpec::Harness(_) => panic!("expected an agentic spec"),
    }
}

/// A tool with input examples, strictness, loading, icons, and `open_world` is created and read back
#[tokio::test]
async fn model_api_fields_round_trip() -> Result<(), Error> {
    // a group named for this test
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let group = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    // a tool setting every model-API tool field
    let mut tool = agentic("scan_sample", &required_schema());
    tool.input_examples = vec![json!({"sha256": "a".repeat(64)})];
    tool.strict = ToolStrictness::On;
    tool.loading = ToolLoading::Deferred;
    tool.icons = vec![ToolIcon {
        src: "https://example.com/scan.png".into(),
        mime_type: Some("image/png".into()),
        sizes: vec!["48x48".into()],
        theme: Some(ToolIconTheme::Dark),
    }];
    tool.capabilities.open_world = true;
    tool.output_schema = Some(
        json!({"type": "object", "properties": {"hits": {"type": "integer"}}})
            .as_object()
            .unwrap()
            .clone(),
    );
    let resp = create(&admin, &group, &tool).await?;
    println!("create response: {}", serde_json::to_string(&resp)?);
    // the create response carries a clean report
    let report = resp.portability.clone().expect("no portability report");
    is!(report.strict_compatible, true);
    is!(report.gemini_compatible, true);
    // the instance reads back with every field
    let instance = admin.entities.get(resp.tool_instance.unwrap()).await?;
    is!(agentic_spec(&instance).clone(), tool.clone());
    is!(
        instance_meta(&instance).portability.clone(),
        Some(report.clone())
    );
    // and the spec hash covers them
    let plain = AgenticTool {
        input_examples: Vec::new(),
        strict: ToolStrictness::Auto,
        loading: ToolLoading::Eager,
        icons: Vec::new(),
        ..tool.clone()
    };
    assert_ne!(
        instance_meta(&instance).spec_hash,
        thorium::models::entities::digests::ThorDigest::tool_spec(&ToolSpec::Agentic(plain), None)?
    );
    // the stored row holds the model-API tool fields but no report
    for (row_group, kind_data) in test_utilities::entity_rows(instance.id).await? {
        println!("scylla entities_by_id group={row_group} kind_data={kind_data}");
        let stored: Value = serde_json::from_str(&kind_data)?;
        let stored = &stored["ToolInstance"];
        assert!(stored.get("portability").is_none(), "{kind_data}");
        let spec = &stored["spec"]["Agentic"];
        is!(spec["strict"].clone(), json!("On"));
        is!(spec["loading"].clone(), json!("Deferred"));
        is!(spec["capabilities"]["open_world"].clone(), json!(true));
    }
    Ok(())
}

/// The tightened function name, description, and property rules are refused at create
#[tokio::test]
async fn name_description_and_schema_refusals() -> Result<(), Error> {
    // a group named for this test
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let group = god_group(&admin, "mars", &suffix, &GroupRoles::default()).await?;
    let plain = json!({"type": "object", "properties": {"sha256": {"type": "string"}}});
    // names that don't start with a letter or underscore are refused
    for name in ["1scan", "-scan", "9"] {
        refused(&admin, &group, &agentic(name, &plain), "function_name").await?;
    }
    // a name starting with an underscore is accepted
    create(&admin, &group, &agentic("_scan", &plain)).await?;
    // a blank description is refused
    let mut blank = agentic("blank_description", &plain);
    blank.model_description = "   ".into();
    refused(&admin, &group, &blank, "model_description").await?;
    // a property name with a hyphen is refused, naming its pointer
    let hyphen = json!({"type": "object", "properties": {"file-path": {"type": "string"}}});
    refused(
        &admin,
        &group,
        &agentic("hyphen_property", &hyphen),
        "#/properties/file-path",
    )
    .await?;
    // an object schema without properties is refused
    let items = json!({"type": "object", "properties": {"list": {"type": "array", "items": {"type": "object"}}}});
    refused(
        &admin,
        &group,
        &agentic("bare_object", &items),
        "#/properties/list/items",
    )
    .await?;
    Ok(())
}

/// Examples, icons, strictness, output schemas, and the schema safety limits are checked
#[tokio::test]
async fn examples_icons_strict_and_output_refusals() -> Result<(), Error> {
    // a group named for this test
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let group = god_group(&admin, "venus", &suffix, &GroupRoles::default()).await?;
    // six examples are refused
    let mut six = agentic("six_examples", &required_schema());
    six.input_examples = (0..6).map(|i| json!({"sha256": format!("{i}")})).collect();
    refused(&admin, &group, &six, "input_examples").await?;
    // an example missing a required property is refused, naming its index
    let mut missing = agentic("missing_property", &required_schema());
    missing.input_examples = vec![json!({"sha256": "a"}), json!({})];
    refused(&admin, &group, &missing, "input_examples[1]").await?;
    // an http icon and an svg icon are refused
    for (name, src) in [
        ("http_icon", "http://example.com/i.png"),
        ("svg_icon", "data:image/svg+xml;base64,PHN2Zz4="),
    ] {
        let mut icon = agentic(name, &required_schema());
        icon.icons = vec![ToolIcon {
            src: src.into(),
            ..ToolIcon::default()
        }];
        refused(&admin, &group, &icon, "icons").await?;
    }
    // strict On with patternProperties is refused, while Auto publishes
    let patterned = json!({
        "type": "object",
        "properties": {"tags": {"type": "object", "properties": {}, "patternProperties": {"^x_": {"type": "string"}}}},
    });
    let mut strict = agentic("strict_patterned", &patterned);
    strict.strict = ToolStrictness::On;
    refused(&admin, &group, &strict, "patternProperties").await?;
    strict.strict = ToolStrictness::Auto;
    let resp = create(&admin, &group, &strict).await?;
    let report = resp.portability.expect("no portability report");
    is!(report.strict_compatible, false);
    // a remote reference, a backtracking pattern, and a 33-level schema are refused
    let remote =
        json!({"type": "object", "properties": {"x": {"$ref": "https://example.com/s.json"}}});
    refused(&admin, &group, &agentic("remote_ref", &remote), "$ref").await?;
    let lookahead =
        json!({"type": "object", "properties": {"x": {"type": "string", "pattern": "^(?=a)a$"}}});
    refused(&admin, &group, &agentic("lookahead", &lookahead), "pattern").await?;
    let mut inner = json!({"type": "string"});
    for _ in 0..32 {
        inner = json!({"type": "array", "items": inner});
    }
    let deep = json!({"type": "object", "properties": {"x": inner}});
    refused(&admin, &group, &agentic("too_deep", &deep), "32").await?;
    // an output schema needs JSON results
    let mut text = agentic("text_output", &required_schema());
    text.output_schema = Some(
        json!({"type": "object", "properties": {}})
            .as_object()
            .unwrap()
            .clone(),
    );
    text.result_format = ToolResultFormat::Text;
    refused(&admin, &group, &text, "output_schema").await?;
    text.result_format = ToolResultFormat::Json;
    create(&admin, &group, &text).await?;
    Ok(())
}

/// The create response, a get of the instance, and the typed report agree, and none is stored
#[tokio::test]
async fn portability_report_is_computed() -> Result<(), Error> {
    // a group named for this test
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let group = god_group(&admin, "apollo", &suffix, &GroupRoles::default()).await?;
    // a lossy tool publishes with warnings
    let tool = agentic("lossy_schema", &lossy_schema());
    let resp = create(&admin, &group, &tool).await?;
    let created = resp.portability.clone().expect("no portability report");
    println!(
        "create portability: {}",
        serde_json::to_string_pretty(&created)?
    );
    is!(created.gemini_compatible, false);
    is!(created.gemini_rewrites.len(), 4);
    is!(created.strict_compatible, false);
    // the instance get and the typed report agree with the create response
    let instance = admin.entities.get(resp.tool_instance.unwrap()).await?;
    let got: Option<ToolPortabilityReport> = instance_meta(&instance).portability.clone();
    is!(got, Some(created.clone()));
    is!(tool.portability(), created.clone());
    // a direct instance create returns the report too
    let next = AgenticTool {
        model_description: "the next version".into(),
        ..tool.clone()
    };
    let meta = thorium::models::ToolInstanceEntityRequest::new(
        resp.id,
        "1.1.0",
        &ToolSpec::Agentic(next),
    )?;
    let req = thorium::models::EntityRequest::new(
        "ignored",
        thorium::models::EntityMetadataRequest::ToolInstance(meta),
        vec![group.clone()],
    );
    let direct = admin.entities.create(req).await?;
    is!(direct.portability, Some(created.clone()));
    // the stored rows hold no report
    for (row_group, kind_data) in test_utilities::entity_rows(instance.id).await? {
        println!("scylla entities_by_id group={row_group} kind_data={kind_data}");
        let stored: Value = serde_json::from_str(&kind_data)?;
        assert!(stored["ToolInstance"].is_object(), "{kind_data}");
        assert!(
            stored["ToolInstance"].get("portability").is_none(),
            "{kind_data}"
        );
    }
    Ok(())
}

/// A stored instance that predates the rules is returned unchanged, while a new version must pass
#[tokio::test]
async fn stored_instances_are_not_revalidated() -> Result<(), Error> {
    // a group named for this test and a tool in it
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let group = god_group(&admin, "diana", &suffix, &GroupRoles::default()).await?;
    let plain = json!({"type": "object", "properties": {"sha256": {"type": "string"}}});
    let resp = create(&admin, &group, &agentic("scan1", &plain)).await?;
    let instance_id = resp.tool_instance.unwrap();
    // rewrite the stored instance with a function name the name rule refuses, as data stored
    // before the rule holds
    let rows = test_utilities::entity_rows(instance_id).await?;
    let (_, kind_data) = rows.first().expect("no stored rows").clone();
    let mut row: Value = serde_json::from_str(&kind_data)?;
    let stored = row["ToolInstance"].clone();
    // a spec that sets none of the model-API tool fields is stored without their keys
    let spec = stored["spec"]["Agentic"].as_object().unwrap();
    for key in ["input_examples", "strict", "loading", "icons"] {
        assert!(!spec.contains_key(key), "{kind_data}");
    }
    assert!(
        !spec["capabilities"]
            .as_object()
            .unwrap()
            .contains_key("open_world")
    );
    // and its stored hash is the hash of its spec
    let parsed: ToolSpec = serde_json::from_value(stored["spec"].clone())?;
    is!(
        thorium::models::entities::digests::ThorDigest::tool_spec(&parsed, None)?,
        stored["spec_hash"].as_str().unwrap().to_owned()
    );
    row["ToolInstance"]["spec"]["Agentic"]["function_name"] = json!("1scan");
    let stored = row["ToolInstance"].clone();
    let old_row = serde_json::to_string(&row)?;
    test_utilities::set_entity_kind_data(instance_id, &old_row).await?;
    println!("scylla kind_data before get: {old_row}");
    // a get returns it with its spec unchanged
    let instance = admin.entities.get(instance_id).await?;
    is!(
        agentic_spec(&instance).function_name.clone(),
        "1scan".to_owned()
    );
    is!(
        serde_json::to_value(&instance_meta(&instance).spec)?,
        stored["spec"].clone()
    );
    is!(
        instance_meta(&instance).spec_hash.clone(),
        stored["spec_hash"].as_str().unwrap().to_owned()
    );
    // the stored row is unchanged by the read
    for (_, kind_data) in test_utilities::entity_rows(instance_id).await? {
        println!("scylla kind_data after get: {kind_data}");
        is!(serde_json::from_str::<Value>(&kind_data)?, row.clone());
    }
    // publishing a new version with the same name is refused
    let mut next = agentic("1scan", &plain);
    next.model_description = "the next version".into();
    let resp = admin
        .entities
        .update(
            resp.id,
            generators::gen_tool_publish("1.1.0", &ToolSpec::Agentic(next)),
        )
        .await;
    fail!(resp, 400);
    let msg = resp.unwrap_err().msg().unwrap_or_default();
    assert!(msg.contains("function_name"), "{msg}");
    println!("refused publish of 1scan: {msg}");
    Ok(())
}
