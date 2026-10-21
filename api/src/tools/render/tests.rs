//! Tests for rendering tools, tool choices, results, and arguments for each model API

use serde_json::{Map, Value, json};
use std::path::PathBuf;
use uuid::Uuid;

use super::{
    ANTHROPIC_MAX_STRICT_OPTIONAL_PARAMETERS, ANTHROPIC_MAX_STRICT_TOOLS,
    ANTHROPIC_MAX_STRICT_UNION_TYPES, RenderError, RenderOptions, ToolCallOutput, ToolCallResult,
    decode_arguments, encode_result, render_structured_output, render_tool, render_tool_choice,
    render_tools,
};
use crate::models::{
    AgenticTool, AgenticToolBacking, GeminiApiVersion, HarnessToolEntry, ModelApi,
    ModelCapabilities, ToolAnnotations, ToolChoice, ToolIcon, ToolIconTheme, ToolInvocation,
    ToolLoading, ToolResultFormat, ToolStrictness,
};

/// Every model API, in a fixed order
const APIS: [ModelApi; 8] = [
    ModelApi::AnthropicMessages,
    ModelApi::VertexRawPredict,
    ModelApi::BedrockInvoke,
    ModelApi::BedrockConverse,
    ModelApi::OpenAiChat,
    ModelApi::OpenAiResponses,
    ModelApi::GeminiGenerateContent,
    ModelApi::GeminiInteractions,
];

/// The Claude model id the Bedrock `InvokeModel` renders use
const CLAUDE_ON_BEDROCK: &str = "anthropic.claude-sonnet-4-5-20250929-v1:0";

/// Compare a value with its committed golden, or rewrite the golden when
/// `THORIUM_UPDATE_GOLDENS` is set
///
/// # Arguments
///
/// * `name` - The golden's path under `src/tools/goldens`
/// * `actual` - The rendered value
fn golden_json(name: &str, actual: &Value) {
    // goldens live beside the renderers
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src/tools/goldens")
        .join(name);
    // rewrite the golden when asked to
    if std::env::var_os("THORIUM_UPDATE_GOLDENS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut raw = serde_json::to_string_pretty(actual).unwrap();
        raw.push('\n');
        std::fs::write(&path, raw).unwrap();
        return;
    }
    // compare with the committed golden
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("missing golden {}: {error}", path.display()));
    let mut expected: Value = serde_json::from_str(&raw).unwrap();
    let mut actual = actual.clone();
    sort_required(&mut expected);
    sort_required(&mut actual);
    assert_eq!(expected, actual, "golden {name} differs");
}

/// Sort every `required` list in a rendered value
///
/// Strict renders list every property as required in the properties map's iteration order,
/// which depends on whether another crate in the build enables `serde_json`'s
/// `preserve_order`; `required` is a set, so goldens compare it sorted
///
/// # Arguments
///
/// * `value` - The value to normalize
fn sort_required(value: &mut Value) {
    match value {
        Value::Object(map) => {
            // sort this object's required names, then walk its children
            if let Some(Value::Array(names)) = map.get_mut("required") {
                names.sort_by(|left, right| left.as_str().cmp(&right.as_str()));
            }
            map.values_mut().for_each(sort_required);
        }
        // walk each array element
        Value::Array(items) => items.iter_mut().for_each(sort_required),
        _ => {}
    }
}

/// Build a `tools.json` entry for a tool with an input schema
///
/// # Arguments
///
/// * `name` - The tool's function name
/// * `schema` - The tool's input schema
fn entry(name: &str, schema: &Value) -> HarnessToolEntry {
    // the agentic tool the entry is written from, for its annotations and portability report
    let tool = AgenticTool {
        function_name: name.to_owned(),
        model_description: format!("Run {name} on a sample"),
        input_schema: schema.as_object().unwrap().clone(),
        backing: AgenticToolBacking::ThoriumMcp {
            tool: name.to_owned(),
        },
        timeout: Some(60),
        ..AgenticTool::default()
    };
    HarnessToolEntry {
        function_name: tool.function_name.clone(),
        title: None,
        model_description: tool.model_description.clone(),
        input_schema: tool.input_schema.clone(),
        output_schema: None,
        input_examples: Vec::new(),
        strict: ToolStrictness::Auto,
        loading: ToolLoading::Eager,
        icons: Vec::new(),
        capabilities: tool.capabilities.clone(),
        annotations: ToolAnnotations::from(&tool),
        portability: tool.portability(),
        result_format: ToolResultFormat::Json,
        requires_approval: false,
        max_result_bytes: 0,
        timeout: Some(60),
        invocation: ToolInvocation::ThoriumApi {
            tool_instance: Uuid::nil(),
            backing: tool.backing.clone(),
        },
    }
}

/// A flat schema of string and integer properties, all required
fn flat_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"sha256": {"type": "string"}, "depth": {"type": "integer"}},
        "required": ["sha256", "depth"],
    })
}

/// A schema with one required and two optional properties
fn optional_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "sha256": {"type": "string"},
            "depth": {"type": "integer"},
            "verbose": {"type": "boolean"},
        },
        "required": ["sha256"],
    })
}

/// A schema with a nested object property
fn nested_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "target": {
                "type": "object",
                "properties": {"sha256": {"type": "string"}, "path": {"type": "string"}},
                "required": ["sha256"],
            },
        },
        "required": ["target"],
    })
}

/// A schema with an enum property
fn enum_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"mode": {"type": "string", "enum": ["fast", "full"]}},
        "required": ["mode"],
    })
}

/// A schema with numeric and length bounds
fn bounds_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "depth": {"type": "integer", "minimum": 1, "maximum": 10},
            "name": {"type": "string", "minLength": 1, "maxLength": 64},
        },
        "required": ["depth", "name"],
    })
}

/// The schema of the portability report tests with one of each lossy Gemini rewrite
fn lossy_schema() -> Value {
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

/// The fixture entries the golden tool definitions are rendered from, by fixture name
fn fixtures() -> Vec<(&'static str, HarnessToolEntry)> {
    // a plain entry for each schema shape
    let mut examples = entry("yara_scan", &flat_schema());
    examples.input_examples = vec![json!({"sha256": "abc123", "depth": 2})];
    let mut deferred = entry("strings", &flat_schema());
    deferred.loading = ToolLoading::Deferred;
    let mut icons = entry("capa", &flat_schema());
    icons.icons = vec![ToolIcon {
        src: "https://thorium.example/capa.png".to_owned(),
        mime_type: Some("image/png".to_owned()),
        sizes: vec!["48x48".to_owned()],
        theme: Some(ToolIconTheme::Light),
    }];
    icons.title = Some("Capa".to_owned());
    vec![
        ("flat", entry("get_sample", &flat_schema())),
        ("optional", entry("get_results", &optional_schema())),
        ("nested", entry("carve", &nested_schema())),
        ("enum", entry("scan_mode", &enum_schema())),
        ("bounds", entry("bounded", &bounds_schema())),
        ("examples", examples),
        ("deferred", deferred),
        ("icons", icons),
    ]
}

/// Render options for a model with every tool capability
///
/// # Arguments
///
/// * `model_id` - The provider's model id
fn capable(model_id: &str) -> RenderOptions {
    RenderOptions {
        capabilities: ModelCapabilities {
            tool_calling: true,
            strict_tools: true,
            parallel_tool_calls: true,
            forced_tool_choice: true,
            ..ModelCapabilities::default()
        },
        gemini_api_version: None,
        model_id: model_id.to_owned(),
        history_has_tool_calls: false,
    }
}

/// Render options for a model with no tool capabilities beyond calling tools
fn plain() -> RenderOptions {
    RenderOptions {
        capabilities: ModelCapabilities {
            tool_calling: true,
            ..ModelCapabilities::default()
        },
        model_id: CLAUDE_ON_BEDROCK.to_owned(),
        ..RenderOptions::default()
    }
}

/// Get the input schema a rendered definition carries
///
/// # Arguments
///
/// * `api` - The model API it was rendered for
/// * `definition` - The rendered definition
fn rendered_schema(api: ModelApi, definition: &Value) -> Value {
    // each API puts the schema in its own place
    match api {
        ModelApi::AnthropicMessages | ModelApi::VertexRawPredict | ModelApi::BedrockInvoke => {
            definition["input_schema"].clone()
        }
        ModelApi::BedrockConverse => definition["toolSpec"]["inputSchema"]["json"].clone(),
        ModelApi::OpenAiChat => definition["function"]["parameters"].clone(),
        ModelApi::OpenAiResponses | ModelApi::GeminiInteractions => {
            definition["parameters"].clone()
        }
        ModelApi::GeminiGenerateContent => definition
            .get("parameters")
            .or_else(|| definition.get("parametersJsonSchema"))
            .cloned()
            .unwrap(),
    }
}

/// Tool definitions for each model API match their goldens, and Bedrock `InvokeModel` refuses
/// non-Claude models
#[test]
fn tool_definition_goldens() {
    // render every fixture for every API with a strict-capable model
    for api in APIS {
        let mut rendered = Map::new();
        for (name, fixture) in fixtures() {
            let tool = render_tool(&fixture, api, &capable(CLAUDE_ON_BEDROCK))
                .unwrap_or_else(|error| panic!("{api:?} {name}: {error}"));
            // every definition carries the name, the description, and a properties object
            let definition = &tool.definition;
            let schema = rendered_schema(api, definition);
            assert!(
                schema["properties"].is_object(),
                "{api:?} {name}: {definition}"
            );
            rendered.insert(name.to_owned(), serde_json::to_value(&tool).unwrap());
        }
        golden_json(&format!("render/{api:?}.json"), &Value::Object(rendered));
    }
    // Bedrock InvokeModel only renders tools for Claude models
    let error = render_tool(
        &entry("get_sample", &flat_schema()),
        ModelApi::BedrockInvoke,
        &capable("meta.llama3-70b-instruct-v1:0"),
    )
    .unwrap_err();
    assert!(
        matches!(
            error,
            RenderError::Unsupported {
                api: ModelApi::BedrockInvoke,
                ..
            }
        ),
        "{error:?}"
    );
}

/// Collect every object subschema of a schema
///
/// # Arguments
///
/// * `schema` - The schema to walk
/// * `found` - The object subschemas found so far
fn objects<'a>(schema: &'a Value, found: &mut Vec<&'a Map<String, Value>>) {
    // an object with properties is an object schema
    if let Some(map) = schema.as_object() {
        if map.contains_key("properties") {
            found.push(map);
        }
        // walk every nested value
        for value in map.values() {
            objects(value, found);
        }
    } else if let Some(items) = schema.as_array() {
        for item in items {
            objects(item, found);
        }
    }
}

/// Strict variants close objects, require properties, and drop bounds per API; strictness
/// follows the tool's setting, the model, and the schema
#[test]
#[allow(clippy::too_many_lines)]
fn strict_variants() {
    // a schema with an optional property, a nested object, and bounds
    let schema = json!({
        "type": "object",
        "properties": {
            "sha256": {"type": "string", "minLength": 64, "maxLength": 64},
            "depth": {"type": "integer", "minimum": 1},
            "target": {"type": "object", "properties": {"path": {"type": "string"}}},
        },
        "required": ["sha256"],
    });
    let fixture = entry("carve", &schema);
    // the OpenAI APIs and Vertex close every object, require every property, and make optional
    // ones nullable
    for api in [
        ModelApi::OpenAiChat,
        ModelApi::OpenAiResponses,
        ModelApi::VertexRawPredict,
    ] {
        let tool = render_tool(&fixture, api, &capable(CLAUDE_ON_BEDROCK)).unwrap();
        assert!(tool.strict, "{api:?}");
        let rendered = rendered_schema(api, &tool.definition);
        let mut found = Vec::new();
        objects(&rendered, &mut found);
        assert_eq!(found.len(), 2, "{api:?}: {rendered}");
        for object in found {
            assert_eq!(object["additionalProperties"], json!(false), "{api:?}");
            let mut required: Vec<&str> = object["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|name| name.as_str().unwrap())
                .collect();
            required.sort_unstable();
            let mut names: Vec<&str> = object["properties"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            names.sort_unstable();
            assert_eq!(required, names, "{api:?}");
        }
        assert_eq!(
            rendered["properties"]["depth"]["type"],
            json!(["integer", "null"]),
            "{api:?}"
        );
        assert_eq!(rendered["properties"]["sha256"]["type"], json!("string"));
    }
    // the OpenAI APIs keep bounds
    let chat = render_tool(&fixture, ModelApi::OpenAiChat, &capable("gpt")).unwrap();
    let chat_schema = rendered_schema(ModelApi::OpenAiChat, &chat.definition);
    assert_eq!(chat_schema["properties"]["depth"]["minimum"], json!(1));
    // Anthropic and Bedrock close every object and drop numeric and length bounds
    for api in [
        ModelApi::AnthropicMessages,
        ModelApi::BedrockInvoke,
        ModelApi::BedrockConverse,
        ModelApi::VertexRawPredict,
    ] {
        let tool = render_tool(&fixture, api, &capable(CLAUDE_ON_BEDROCK)).unwrap();
        assert!(tool.strict, "{api:?}");
        let rendered = rendered_schema(api, &tool.definition);
        assert_eq!(rendered["additionalProperties"], json!(false), "{api:?}");
        assert_eq!(
            rendered["properties"]["target"]["additionalProperties"],
            json!(false),
            "{api:?}"
        );
        let raw = rendered.to_string();
        for bound in ["minimum", "minLength", "maxLength"] {
            assert!(!raw.contains(bound), "{api:?} kept {bound}: {raw}");
        }
        // only Vertex requires every property
        if api != ModelApi::VertexRawPredict {
            assert_eq!(rendered["required"], json!(["sha256"]), "{api:?}");
        }
    }
    // Auto on a model without strict_tools renders non-strict with no warning
    for api in APIS {
        let tool = render_tool(&fixture, api, &plain()).unwrap();
        assert!(!tool.strict, "{api:?}");
        assert!(tool.warnings.is_empty(), "{api:?}: {:?}", tool.warnings);
    }
    // Auto with a strict-incompatible schema renders non-strict with no warning
    let open = entry(
        "open",
        &json!({"type": "object", "properties": {}, "additionalProperties": {"type": "string"}}),
    );
    assert!(!open.portability.strict_compatible);
    for api in APIS {
        let tool = render_tool(&open, api, &capable(CLAUDE_ON_BEDROCK)).unwrap();
        assert!(!tool.strict, "{api:?}");
        assert!(tool.warnings.is_empty(), "{api:?}: {:?}", tool.warnings);
    }
    // On on a model without strict_tools renders non-strict with a warning naming the tool
    let mut on = fixture.clone();
    on.strict = ToolStrictness::On;
    for api in [
        ModelApi::AnthropicMessages,
        ModelApi::OpenAiChat,
        ModelApi::BedrockConverse,
    ] {
        let tool = render_tool(&on, api, &plain()).unwrap();
        assert!(!tool.strict, "{api:?}");
        assert_eq!(tool.warnings.len(), 1, "{api:?}");
        assert!(tool.warnings[0].contains("carve"), "{:?}", tool.warnings);
    }
    // Off never renders strict
    let mut off = fixture.clone();
    off.strict = ToolStrictness::Off;
    for api in APIS {
        let tool = render_tool(&off, api, &capable(CLAUDE_ON_BEDROCK)).unwrap();
        assert!(!tool.strict, "{api:?}");
        assert_eq!(
            rendered_schema(api, &tool.definition),
            Value::Object(off.input_schema.clone()),
            "{api:?}"
        );
    }
    // Responses sends strict: false when not strict, and the other APIs send no strict flag
    let responses = render_tool(&off, ModelApi::OpenAiResponses, &capable("gpt")).unwrap();
    assert_eq!(responses.definition["strict"], json!(false));
    let chat = render_tool(&off, ModelApi::OpenAiChat, &capable("gpt")).unwrap();
    assert!(chat.definition["function"].get("strict").is_none());
    let anthropic = render_tool(&off, ModelApi::AnthropicMessages, &capable("claude")).unwrap();
    assert!(anthropic.definition.get("strict").is_none());
}

/// Gemini's `v1` rewrites schemas into its `OpenAPI` subset, `v1beta` takes them unchanged, and the
/// first input example becomes the schema's example
#[test]
fn gemini_transforms() {
    // a v1 endpoint
    let v1 = RenderOptions {
        gemini_api_version: Some(GeminiApiVersion::V1),
        ..capable("gemini-2.5-pro")
    };
    // the portability report's lossy schema gets each listed rewrite
    let lossy = entry("lossy", &lossy_schema());
    let tool = render_tool(&lossy, ModelApi::GeminiGenerateContent, &v1).unwrap();
    assert!(tool.definition.get("parametersJsonSchema").is_none());
    assert_eq!(
        tool.definition["parameters"],
        json!({
            "type": "object",
            "properties": {
                "meta": {"type": "object", "properties": {}},
                "target": {"type": "string"},
                "mode": {"anyOf": [{"type": "string"}, {"type": "integer"}]},
                "kind": {"type": "string", "enum": ["scan"]},
            },
        })
    );
    // allOf merges, type arrays become nullable, and enums become strings
    let more = entry(
        "more",
        &json!({
            "type": "object",
            "properties": {
                "both": {"allOf": [
                    {"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]},
                    {"properties": {"b": {"type": "integer"}}},
                ]},
                "label": {"type": ["string", "null"]},
                "level": {"type": "integer", "enum": [1, 2]},
            },
        }),
    );
    let tool = render_tool(&more, ModelApi::GeminiGenerateContent, &v1).unwrap();
    assert_eq!(
        tool.definition["parameters"]["properties"],
        json!({
            "both": {
                "type": "object",
                "properties": {"a": {"type": "string"}, "b": {"type": "integer"}},
                "required": ["a"],
            },
            "label": {"type": "string", "nullable": true},
            "level": {"type": "string", "enum": ["1", "2"]},
        })
    );
    // on v1beta the schema goes unchanged as parametersJsonSchema and the output schema as
    // responseJsonSchema
    let mut with_output = lossy.clone();
    with_output.output_schema = Some(
        json!({"type": "object", "properties": {"verdict": {"const": "clean"}}})
            .as_object()
            .unwrap()
            .clone(),
    );
    let v1beta = RenderOptions {
        gemini_api_version: Some(GeminiApiVersion::V1Beta),
        ..capable("gemini-2.5-pro")
    };
    let tool = render_tool(&with_output, ModelApi::GeminiGenerateContent, &v1beta).unwrap();
    assert!(tool.definition.get("parameters").is_none());
    assert_eq!(tool.definition["parametersJsonSchema"], lossy_schema());
    assert_eq!(
        tool.definition["responseJsonSchema"],
        Value::Object(with_output.output_schema.clone().unwrap())
    );
    // on v1 the output schema goes as response, rewritten
    let tool = render_tool(&with_output, ModelApi::GeminiGenerateContent, &v1).unwrap();
    assert_eq!(
        tool.definition["response"],
        json!({"type": "object", "properties": {"verdict": {"type": "string", "enum": ["clean"]}}})
    );
    // the first input example becomes the schema's example on both versions
    let mut examples = entry("yara_scan", &flat_schema());
    examples.input_examples = vec![
        json!({"sha256": "abc", "depth": 1}),
        json!({"sha256": "def", "depth": 2}),
    ];
    let tool = render_tool(&examples, ModelApi::GeminiGenerateContent, &v1).unwrap();
    assert_eq!(
        tool.definition["parameters"]["example"],
        json!({"sha256": "abc", "depth": 1})
    );
    let tool = render_tool(&examples, ModelApi::GeminiGenerateContent, &v1beta).unwrap();
    assert_eq!(
        tool.definition["parametersJsonSchema"]["example"],
        json!({"sha256": "abc", "depth": 1})
    );
}

/// Build a schema with optional properties and union-typed properties
///
/// # Arguments
///
/// * `optional` - How many optional properties to add
/// * `unions` - How many required union-typed properties to add
fn counted_schema(optional: usize, unions: usize) -> Value {
    // one property per optional and per union
    let mut properties = Map::new();
    let mut required = Vec::new();
    for index in 0..optional {
        properties.insert(format!("opt_{index}"), json!({"type": "string"}));
    }
    for index in 0..unions {
        let name = format!("union_{index}");
        properties.insert(
            name.clone(),
            json!({"anyOf": [{"type": "string"}, {"type": "integer"}]}),
        );
        required.push(json!(name));
    }
    json!({"type": "object", "properties": properties, "required": required})
}

/// Anthropic's per-request strict limits send the tools past each limit non-strict with a
/// warning, and Converse takes its input schema as an object and structured output as a string
#[test]
fn request_limits_and_converse() {
    let opts = capable(CLAUDE_ON_BEDROCK);
    // one more strict-eligible tool than the limit sends the last one non-strict
    let many: Vec<HarnessToolEntry> = (0..=ANTHROPIC_MAX_STRICT_TOOLS)
        .map(|index| entry(&format!("tool_{index}"), &flat_schema()))
        .collect();
    let refs: Vec<&HarnessToolEntry> = many.iter().collect();
    let rendered = render_tools(&refs, ModelApi::AnthropicMessages, &opts).unwrap();
    assert_eq!(rendered.len(), ANTHROPIC_MAX_STRICT_TOOLS + 1);
    let (last, first) = rendered.split_last().unwrap();
    assert!(
        first
            .iter()
            .all(|tool| tool.strict && tool.warnings.is_empty())
    );
    assert!(!last.strict);
    assert_eq!(last.warnings.len(), 1, "{:?}", last.warnings);
    assert!(
        last.warnings[0].contains(&format!("tool_{ANTHROPIC_MAX_STRICT_TOOLS}")),
        "{:?}",
        last.warnings
    );
    // the optional-parameter limit
    let full = entry(
        "full",
        &counted_schema(ANTHROPIC_MAX_STRICT_OPTIONAL_PARAMETERS, 0),
    );
    let one_more = entry("one_more", &counted_schema(1, 0));
    let rendered = render_tools(&[&full, &one_more], ModelApi::AnthropicMessages, &opts).unwrap();
    assert!(rendered[0].strict && rendered[0].warnings.is_empty());
    assert!(!rendered[1].strict);
    assert!(rendered[1].warnings[0].contains("one_more"));
    // the union-type limit
    let full = entry("full", &counted_schema(0, ANTHROPIC_MAX_STRICT_UNION_TYPES));
    let one_more = entry("one_more", &counted_schema(0, 1));
    let rendered = render_tools(&[&full, &one_more], ModelApi::AnthropicMessages, &opts).unwrap();
    assert!(rendered[0].strict && rendered[0].warnings.is_empty());
    assert!(!rendered[1].strict);
    assert!(rendered[1].warnings[0].contains("one_more"));
    // the limits are Anthropic's, so OpenAI keeps every tool strict
    let rendered = render_tools(&refs, ModelApi::OpenAiChat, &opts).unwrap();
    assert!(rendered.iter().all(|tool| tool.strict));
    // Converse takes the input schema as an object and structured output as a JSON string
    let tool = render_tool(
        &entry("get_sample", &flat_schema()),
        ModelApi::BedrockConverse,
        &opts,
    )
    .unwrap();
    assert!(tool.definition["toolSpec"]["inputSchema"]["json"].is_object());
    let schema = flat_schema().as_object().unwrap().clone();
    let structured = render_structured_output(&schema, ModelApi::BedrockConverse);
    let parsed: Value = serde_json::from_str(structured.as_str().unwrap()).unwrap();
    assert_eq!(parsed, flat_schema());
    assert_eq!(
        render_structured_output(&schema, ModelApi::OpenAiChat),
        flat_schema()
    );
}

/// The tool choice table for one model API: the fields for `Auto`, `Required`, `None`, and
/// `Function("yara_scan")`, the parallel-off fields, and whether the harness serializes calls
/// instead
///
/// # Arguments
///
/// * `api` - The model API
fn choice_table(api: ModelApi) -> ([Value; 4], Value, bool) {
    // the expected fields of each choice for this API
    match api {
        ModelApi::AnthropicMessages | ModelApi::VertexRawPredict | ModelApi::BedrockInvoke => (
            [
                json!({"tool_choice": {"type": "auto"}}),
                json!({"tool_choice": {"type": "any"}}),
                json!({"tool_choice": {"type": "none"}}),
                json!({"tool_choice": {"type": "tool", "name": "yara_scan"}}),
            ],
            json!({"tool_choice": {"type": "auto", "disable_parallel_tool_use": true}}),
            false,
        ),
        ModelApi::BedrockConverse => (
            [
                json!({"toolChoice": {"auto": {}}}),
                json!({"toolChoice": {"any": {}}}),
                json!({"toolConfig": null}),
                json!({"toolChoice": {"tool": {"name": "yara_scan"}}}),
            ],
            json!({"toolChoice": {"auto": {}}}),
            true,
        ),
        ModelApi::OpenAiChat => (
            [
                json!({"tool_choice": "auto"}),
                json!({"tool_choice": "required"}),
                json!({"tool_choice": "none"}),
                json!({"tool_choice": {"type": "function", "function": {"name": "yara_scan"}}}),
            ],
            json!({"tool_choice": "auto", "parallel_tool_calls": false}),
            false,
        ),
        ModelApi::OpenAiResponses => (
            [
                json!({"tool_choice": "auto"}),
                json!({"tool_choice": "required"}),
                json!({"tool_choice": "none"}),
                json!({"tool_choice": {"type": "function", "name": "yara_scan"}}),
            ],
            json!({"tool_choice": "auto", "parallel_tool_calls": false}),
            false,
        ),
        ModelApi::GeminiGenerateContent => (
            [
                json!({"toolConfig": {"functionCallingConfig": {"mode": "AUTO"}}}),
                json!({"toolConfig": {"functionCallingConfig": {"mode": "ANY"}}}),
                json!({"toolConfig": {"functionCallingConfig": {"mode": "NONE"}}}),
                json!({"toolConfig": {"functionCallingConfig": {
                    "mode": "ANY",
                    "allowedFunctionNames": ["yara_scan"],
                }}}),
            ],
            json!({"toolConfig": {"functionCallingConfig": {"mode": "AUTO"}}}),
            true,
        ),
        ModelApi::GeminiInteractions => (
            [
                json!({"tool_choice": "auto"}),
                json!({"tool_choice": "any"}),
                json!({"tool_choice": "none"}),
                json!({"tool_choice": {"allowed_tools": {"mode": "any", "tools": ["yara_scan"]}}}),
            ],
            json!({"tool_choice": "auto"}),
            true,
        ),
    }
}

/// Tool choices and the parallel-call setting follow the tool choice table for each model API
#[test]
fn tool_choice_mapping() {
    let choices = [
        ToolChoice::Auto,
        ToolChoice::Required,
        ToolChoice::None,
        ToolChoice::Function("yara_scan".to_owned()),
    ];
    for api in APIS {
        let (expected, parallel_off, serializes) = choice_table(api);
        let opts = capable(CLAUDE_ON_BEDROCK);
        // each choice with parallel calls left to the model
        for (choice, expected) in choices.iter().zip(expected) {
            let rendered = render_tool_choice(choice, None, api, &opts);
            assert_eq!(
                Value::Object(rendered.fields),
                expected,
                "{api:?} {choice:?}"
            );
            assert!(!rendered.serialize_calls, "{api:?} {choice:?}");
            assert!(rendered.warnings.is_empty(), "{api:?} {choice:?}");
        }
        // parallel calls turned off with Auto
        let rendered = render_tool_choice(&ToolChoice::Auto, Some(false), api, &opts);
        assert_eq!(Value::Object(rendered.fields), parallel_off, "{api:?}");
        assert_eq!(rendered.serialize_calls, serializes, "{api:?}");
        // parallel calls turned on changes nothing
        let rendered = render_tool_choice(&ToolChoice::Auto, Some(true), api, &opts);
        assert!(!rendered.serialize_calls, "{api:?}");
        // a forced choice on a model without forced_tool_choice is sent as Auto with a warning
        let unforced = plain();
        let rendered = render_tool_choice(&ToolChoice::Required, None, api, &unforced);
        assert_eq!(
            Value::Object(rendered.fields),
            choice_table(api).0[0],
            "{api:?}"
        );
        assert_eq!(rendered.warnings.len(), 1, "{api:?}");
        assert!(rendered.warnings[0].contains("Required"), "{api:?}");
    }
    // Converse with tools off but tool blocks in the history keeps a tool config and says calls
    // must be answered with errors
    let history = RenderOptions {
        history_has_tool_calls: true,
        ..capable(CLAUDE_ON_BEDROCK)
    };
    let rendered = render_tool_choice(&ToolChoice::None, None, ModelApi::BedrockConverse, &history);
    assert_eq!(
        Value::Object(rendered.fields),
        json!({"toolChoice": {"auto": {}}})
    );
    assert_eq!(rendered.warnings.len(), 1);
    assert!(rendered.warnings[0].contains("error"));
    // parallel off on a model whose API can't turn it off for it makes the harness serialize
    let unparallel = plain();
    let rendered = render_tool_choice(
        &ToolChoice::Auto,
        Some(false),
        ModelApi::OpenAiChat,
        &unparallel,
    );
    assert_eq!(
        Value::Object(rendered.fields),
        json!({"tool_choice": "auto"})
    );
    assert!(rendered.serialize_calls);
}

/// The success and failure encodings of one model API
///
/// # Arguments
///
/// * `api` - The model API
fn result_table(api: ModelApi) -> (Value, Value) {
    // a JSON success of {"matches": 2} and a text failure "timed out"
    let text = r#"{"matches":2}"#;
    match api {
        ModelApi::AnthropicMessages | ModelApi::VertexRawPredict | ModelApi::BedrockInvoke => (
            json!({"type": "tool_result", "tool_use_id": "call-1", "content": text}),
            json!({"type": "tool_result", "tool_use_id": "call-1", "content": "timed out", "is_error": true}),
        ),
        ModelApi::BedrockConverse => (
            json!({"toolResult": {"toolUseId": "call-1", "content": [{"json": {"matches": 2}}, {"text": text}], "status": "success"}}),
            json!({"toolResult": {"toolUseId": "call-1", "content": [{"text": "timed out"}], "status": "error"}}),
        ),
        ModelApi::OpenAiChat => (
            json!({"role": "tool", "tool_call_id": "call-1", "content": text}),
            json!({"role": "tool", "tool_call_id": "call-1", "content": "Error: timed out"}),
        ),
        ModelApi::OpenAiResponses => (
            json!({"type": "function_call_output", "call_id": "call-1", "output": text}),
            json!({"type": "function_call_output", "call_id": "call-1", "output": "Error: timed out"}),
        ),
        ModelApi::GeminiGenerateContent => (
            json!({"functionResponse": {"id": "call-1", "name": "yara_scan", "response": {"output": {"matches": 2}}}}),
            json!({"functionResponse": {"id": "call-1", "name": "yara_scan", "response": {"error": "timed out"}}}),
        ),
        ModelApi::GeminiInteractions => (
            json!({"type": "function_result", "call_id": "call-1", "name": "yara_scan", "result": {"matches": 2}}),
            json!({"type": "function_result", "call_id": "call-1", "name": "yara_scan", "result": "timed out", "is_error": true}),
        ),
    }
}

/// Build a tool call result
///
/// # Arguments
///
/// * `output` - What the tool returned
/// * `is_error` - Whether the call failed
fn call(output: ToolCallOutput, is_error: bool) -> ToolCallResult {
    ToolCallResult {
        call_id: "call-1".to_owned(),
        function_name: "yara_scan".to_owned(),
        output,
        is_error,
    }
}

/// Results follow the calls-and-results table, files become their paths, results are cut to the
/// entry's limit, and a JSON result that breaks the output schema becomes an error
#[test]
fn result_encoding() {
    let tool = entry("yara_scan", &flat_schema());
    for api in APIS {
        let (success, failure) = result_table(api);
        // a JSON success and a text failure
        let encoded = encode_result(
            &call(ToolCallOutput::Json(json!({"matches": 2})), false),
            &tool,
            api,
        );
        assert_eq!(encoded, success, "{api:?}");
        let encoded = encode_result(
            &call(ToolCallOutput::Text("timed out".to_owned()), true),
            &tool,
            api,
        );
        assert_eq!(encoded, failure, "{api:?}");
    }
    // a files result is its paths in text
    let files = call(
        ToolCallOutput::Files(vec![
            "/tmp/thorium/tool-results/a.txt".to_owned(),
            "/tmp/thorium/tool-results/b.txt".to_owned(),
        ]),
        false,
    );
    let encoded = encode_result(&files, &tool, ModelApi::OpenAiChat);
    assert_eq!(
        encoded["content"],
        json!("/tmp/thorium/tool-results/a.txt\n/tmp/thorium/tool-results/b.txt")
    );
    // a 2000-byte result is cut to the entry's 500-byte limit
    let mut limited = tool.clone();
    limited.max_result_bytes = 500;
    let long = call(ToolCallOutput::Text("a".repeat(2000)), false);
    for api in [ModelApi::AnthropicMessages, ModelApi::OpenAiResponses] {
        let encoded = encode_result(&long, &limited, api);
        let text = encoded
            .get("content")
            .or_else(|| encoded.get("output"))
            .and_then(Value::as_str)
            .unwrap();
        assert_eq!(text.len(), 500, "{api:?}");
    }
    // a JSON result that breaks the output schema becomes an error naming the mismatch
    let mut schemed = tool.clone();
    schemed.output_schema = Some(
        json!({"type": "object", "properties": {"matches": {"type": "integer"}}, "required": ["matches"]})
            .as_object()
            .unwrap()
            .clone(),
    );
    let bad = call(ToolCallOutput::Json(json!({"matches": "two"})), false);
    let encoded = encode_result(&bad, &schemed, ModelApi::AnthropicMessages);
    assert_eq!(encoded["is_error"], json!(true));
    let message = encoded["content"].as_str().unwrap();
    assert!(
        message.contains("output_schema") && message.contains("matches"),
        "{message}"
    );
    // a conforming result is unchanged
    let good = call(ToolCallOutput::Json(json!({"matches": 2})), false);
    let encoded = encode_result(&good, &schemed, ModelApi::AnthropicMessages);
    assert!(encoded.get("is_error").is_none());
}

/// JSON-string arguments parse, bad JSON names the tool, and non-strict arguments are checked
/// against the schema while strict ones are not
#[test]
fn argument_decoding() {
    let tool = entry("yara_scan", &flat_schema());
    // the OpenAI APIs send arguments as a JSON string
    let decoded =
        decode_arguments(&json!(r#"{"sha256": "abc", "depth": 1}"#), &tool, false).unwrap();
    assert_eq!(decoded, json!({"sha256": "abc", "depth": 1}));
    // the other APIs send an object
    let decoded = decode_arguments(&json!({"sha256": "abc", "depth": 1}), &tool, false).unwrap();
    assert_eq!(decoded, json!({"sha256": "abc", "depth": 1}));
    // a string that isn't JSON is an error naming the tool
    let error = decode_arguments(&json!("{sha256: abc"), &tool, false).unwrap_err();
    assert_eq!(error.function_name, "yara_scan");
    assert!(error.message.contains("yara_scan"), "{error}");
    // without strict, a missing required property is an error naming it
    let error = decode_arguments(&json!({"sha256": "abc"}), &tool, false).unwrap_err();
    assert!(
        error.message.contains("yara_scan") && error.message.contains("depth"),
        "{error}"
    );
    // with strict applied the same arguments are parsed but not checked
    let decoded = decode_arguments(&json!(r#"{"sha256": "abc"}"#), &tool, true).unwrap();
    assert_eq!(decoded, json!({"sha256": "abc"}));
}
