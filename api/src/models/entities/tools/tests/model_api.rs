//! Tests for the model-API tool fields, the portability rules, and the portability report

use serde_json::{Value, json};

use super::{mcp_tool, schema};
use crate::models::entities::digests::ThorDigest;
use crate::models::entities::tools::{
    AgenticTool, DEFAULT_MAX_TOOL_SCHEMA_BYTES, ToolCapabilities, ToolEffect, ToolIcon,
    ToolIconTheme, ToolLoading, ToolPortabilityReport, ToolResultFormat, ToolSpec, ToolStrictness,
    tool_portability_json,
};

/// An MCP-backed agentic tool as it was stored before the model-API tool fields existed
const STORED_MCP_TOOL: &str = r#"{"function_name":"get_sample","title":"Get a sample","model_description":"Get a sample's details","input_schema":{"properties":{"sha256":{"type":"string"}},"type":"object"},"output_schema":null,"capabilities":{"effects":[],"idempotent":false,"file_inputs":false,"file_outputs":false},"backing":{"ThoriumMcp":{"tool":"get_sample"}},"result_format":"Json","requires_approval":false,"timeout":60}"#;

/// The `v1:` spec hash [`STORED_MCP_TOOL`] was stored with
///
/// This is the SHA-256 of its tagged canonical digest value:
/// `{"kind":"Agentic","spec":{"Agentic":{"backing":{"ThoriumMcp":{"tool":"get_sample"}},"function_name":"get_sample","input_schema":{"properties":{"sha256":{"type":"string"}},"type":"object"},"model_description":"Get a sample's details","timeout":60,"title":"Get a sample"}},"t":"tool_spec"}`
const STORED_MCP_TOOL_HASH: &str =
    "v1:ad86a46a9b3cc15764704fd41aa1ff94901ae1766d3bd4eba99b69f0f9192eed";

/// Validate a tool with the default schema cap
///
/// # Arguments
///
/// * `tool` - The tool to validate
fn check(tool: &AgenticTool) -> Result<(), String> {
    // validate with the default cap
    tool.validate(DEFAULT_MAX_TOOL_SCHEMA_BYTES)
}

/// Build an MCP tool with an input schema
///
/// # Arguments
///
/// * `input_schema` - The tool's input schema
fn with_schema(input_schema: &Value) -> AgenticTool {
    // swap in the schema
    AgenticTool {
        input_schema: input_schema.as_object().unwrap().clone(),
        ..mcp_tool()
    }
}

/// Hash an agentic tool as a spec
///
/// # Arguments
///
/// * `tool` - The tool to hash
fn hash(tool: &AgenticTool) -> String {
    // hash it as an agentic spec with no pinned instance
    ThorDigest::tool_spec(&ToolSpec::Agentic(tool.clone()), None).unwrap()
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

/// A flat schema of string, integer, and enum properties
fn flat_schema() -> Value {
    // nothing any model API has to rewrite
    json!({
        "type": "object",
        "properties": {
            "sha256": {"type": "string"},
            "depth": {"type": "integer"},
            "mode": {"type": "string", "enum": ["fast", "full"]},
        },
        "required": ["sha256"],
    })
}

/// Stored specs without the model-API tool fields parse with those fields at their defaults and
/// keep their spec hash
#[test]
fn stored_specs_parse_unchanged() {
    // a stored spec parses with every model-API tool field at its default
    let stored: AgenticTool = serde_json::from_str(STORED_MCP_TOOL).unwrap();
    assert_eq!(stored.input_examples, Vec::<Value>::new());
    assert_eq!(stored.icons, Vec::<ToolIcon>::new());
    assert_eq!(stored.strict, ToolStrictness::Auto);
    assert_eq!(stored.loading, ToolLoading::Eager);
    assert!(!stored.capabilities.open_world);
    assert_eq!(stored, mcp_tool());
    // it hashes as it was stored
    assert_eq!(hash(&stored), STORED_MCP_TOOL_HASH);
    // a spec with the model-API tool fields unset serializes to exactly the stored JSON
    assert_eq!(serde_json::to_string(&mcp_tool()).unwrap(), STORED_MCP_TOOL);
    // each model-API tool field, set, round-trips and changes the hash
    let mutations: Vec<fn(&mut AgenticTool)> = vec![
        |tool| tool.input_examples = vec![json!({"sha256": "abc"})],
        |tool| tool.strict = ToolStrictness::On,
        |tool| tool.strict = ToolStrictness::Off,
        |tool| tool.loading = ToolLoading::Deferred,
        |tool| {
            tool.icons = vec![ToolIcon {
                src: "https://example.com/icon.png".into(),
                mime_type: Some("image/png".into()),
                sizes: vec!["48x48".into()],
                theme: Some(ToolIconTheme::Dark),
            }];
        },
        |tool| tool.capabilities.open_world = true,
    ];
    for mutate in mutations {
        // set one model-API tool field
        let mut tool = mcp_tool();
        mutate(&mut tool);
        // it round-trips through JSON
        let raw = serde_json::to_string(&tool).unwrap();
        let back: AgenticTool = serde_json::from_str(&raw).unwrap();
        assert_eq!(back, tool, "{raw}");
        // and changes the hash
        assert_ne!(hash(&tool), STORED_MCP_TOOL_HASH, "{raw}");
    }
    // an icon with an unknown key doesn't parse
    let icon = json!({"src": "https://example.com/icon.png", "colour": "red"});
    assert!(serde_json::from_value::<ToolIcon>(icon).is_err());
}

/// An agentic tool as a client or API binary built without the model-API tool fields parses it
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct OlderAgenticTool {
    /// The name a model calls the tool by
    function_name: String,
    /// The tool's title
    title: Option<String>,
    /// The tool's description for the model
    model_description: String,
    /// The tool's input schema
    input_schema: serde_json::Map<String, Value>,
    /// The tool's output schema
    output_schema: Option<serde_json::Map<String, Value>>,
    /// The tool's capabilities
    #[serde(default)]
    capabilities: Value,
    /// How the tool is invoked
    backing: Value,
    /// The format of the tool's result
    #[serde(default)]
    result_format: Value,
    /// Whether calls need approval
    #[serde(default)]
    requires_approval: bool,
    /// The max seconds a call may run for
    timeout: Option<u64>,
}

/// A binary built without the model-API tool fields parses specs that set none of them and
/// refuses ones that set one
#[test]
fn version_skew() {
    // a spec that sets none of the model-API tool fields parses as the type without them
    let raw = serde_json::to_string(&mcp_tool()).unwrap();
    assert!(serde_json::from_str::<OlderAgenticTool>(&raw).is_ok());
    // a spec that sets strict fails with an unknown field error naming it
    let strict = AgenticTool {
        strict: ToolStrictness::On,
        ..mcp_tool()
    };
    let raw = serde_json::to_string(&strict).unwrap();
    let err = serde_json::from_str::<OlderAgenticTool>(&raw)
        .err()
        .unwrap()
        .to_string();
    assert!(err.contains("unknown field `strict`"), "{err}");
}

/// Function names start with a letter or underscore
#[test]
fn function_name_rule() {
    // these names are accepted
    let long = format!("S{}", "a_-9".repeat(16))[..64].to_owned();
    for name in ["_scan", "a", "Scan_2", long.as_str()] {
        let tool = AgenticTool {
            function_name: name.to_owned(),
            ..mcp_tool()
        };
        assert!(check(&tool).is_ok(), "{name}");
    }
    // these names are refused, naming the field
    for name in ["1scan", "-scan", "9"] {
        let tool = AgenticTool {
            function_name: name.to_owned(),
            ..mcp_tool()
        };
        let err = check(&tool).unwrap_err();
        assert!(err.contains("function_name"), "{name}: {err}");
    }
}

/// Descriptions can't be blank, property names are portable, and object schemas list properties
#[test]
fn description_and_property_rules() {
    // an empty or whitespace-only description is refused
    for description in ["", "  \n\t"] {
        let tool = AgenticTool {
            model_description: description.to_owned(),
            ..mcp_tool()
        };
        let err = check(&tool).unwrap_err();
        assert!(err.contains("model_description"), "{err}");
    }
    // bad property names are refused wherever they are, naming the pointer
    let long = "a".repeat(65);
    for name in ["file-path", "a.b", "1x", long.as_str()] {
        let escaped = name.replace('~', "~0").replace('/', "~1");
        let placements = [
            (
                json!({"type": "object", "properties": {name: {"type": "string"}}}),
                format!("#/properties/{escaped}"),
            ),
            (
                json!({"type": "object", "properties": {"outer": {"type": "object", "properties": {name: {"type": "string"}}}}}),
                format!("#/properties/outer/properties/{escaped}"),
            ),
            (
                json!({"type": "object", "properties": {"list": {"type": "array", "items": {"type": "object", "properties": {name: {"type": "string"}}}}}}),
                format!("#/properties/list/items/properties/{escaped}"),
            ),
            (
                json!({"type": "object", "properties": {"choice": {"anyOf": [{"type": "string"}, {"type": "object", "properties": {name: {"type": "string"}}}]}}}),
                format!("#/properties/choice/anyOf/1/properties/{escaped}"),
            ),
        ];
        for (input_schema, pointer) in placements {
            let err = check(&with_schema(&input_schema)).unwrap_err();
            assert!(err.contains(&pointer), "{pointer}: {err}");
        }
    }
    // portable names are accepted
    let accepted = json!({"type": "object", "properties": {"_x": {"type": "string"}, "sha256": {"type": "string"}}});
    assert!(check(&with_schema(&accepted)).is_ok());
    // an object schema without properties is refused at the top level or as items
    let err = check(&with_schema(&json!({"type": "object"}))).unwrap_err();
    assert!(err.contains("properties") && err.contains("at #"), "{err}");
    let items = json!({"type": "object", "properties": {"list": {"type": "array", "items": {"type": "object"}}}});
    let err = check(&with_schema(&items)).unwrap_err();
    assert!(err.contains("#/properties/list/items"), "{err}");
    // an empty properties object is enough
    let empty = json!({"type": "object", "properties": {}});
    assert!(check(&with_schema(&empty)).is_ok());
}

/// Input examples, icons, strictness, and the schema safety limits are checked
#[test]
// one scenario walks every example, icon, strictness, and safety limit in turn
#[allow(clippy::too_many_lines)]
fn examples_icons_and_strictness() {
    // a schema that requires a sha256
    let required = json!({"type": "object", "properties": {"sha256": {"type": "string"}}, "required": ["sha256"]});
    let examples = |count: usize| {
        (0..count)
            .map(|i| json!({"sha256": format!("{i:064}")}))
            .collect::<Vec<Value>>()
    };
    // 5 valid examples are accepted and 6 are refused
    let five = AgenticTool {
        input_examples: examples(5),
        ..with_schema(&required)
    };
    assert!(check(&five).is_ok());
    let six = AgenticTool {
        input_examples: examples(6),
        ..with_schema(&required)
    };
    let err = check(&six).unwrap_err();
    assert!(err.contains("input_examples"), "{err}");
    // an example missing a required property is refused, naming its index and the error
    let missing = AgenticTool {
        input_examples: vec![json!({"sha256": "a"}), json!({})],
        ..with_schema(&required)
    };
    let err = check(&missing).unwrap_err();
    assert!(
        err.contains("input_examples[1]") && err.contains("sha256"),
        "{err}"
    );
    // an example over 4 KiB is refused
    let big = AgenticTool {
        input_examples: vec![json!({"sha256": "a".repeat(4097)})],
        ..with_schema(&required)
    };
    assert!(check(&big).unwrap_err().contains("input_examples[0]"));
    // examples over 16 KiB in total are refused even when each is under 4 KiB
    let total = AgenticTool {
        input_examples: (0..5)
            .map(|_| json!({"sha256": "a".repeat(3500)}))
            .collect(),
        ..with_schema(&required)
    };
    assert!(check(&total).unwrap_err().contains("input_examples"));
    // icon urls must be https or a raster data URI
    let icon = |src: &str| ToolIcon {
        src: src.to_owned(),
        ..ToolIcon::default()
    };
    for src in ["http://example.com/i.png", "file:///etc/i.png"] {
        let tool = AgenticTool {
            icons: vec![icon(src)],
            ..mcp_tool()
        };
        assert!(check(&tool).unwrap_err().contains("icons"), "{src}");
    }
    for src in [
        "https://example.com/i.png",
        "data:image/png;base64,iVBORw0KGgo=",
    ] {
        let tool = AgenticTool {
            icons: vec![icon(src)],
            ..mcp_tool()
        };
        assert!(check(&tool).is_ok(), "{src}");
    }
    // a fifth icon, an svg data URI, and a data URI over 32 KiB are refused
    let five_icons = AgenticTool {
        icons: vec![icon("https://example.com/i.png"); 5],
        ..mcp_tool()
    };
    assert!(check(&five_icons).unwrap_err().contains("icons"));
    let svg = AgenticTool {
        icons: vec![icon("data:image/svg+xml;base64,PHN2Zz4=")],
        ..mcp_tool()
    };
    assert!(check(&svg).unwrap_err().contains("icons"));
    let huge = format!("data:image/png;base64,{}", "A".repeat(32 * 1024));
    let huge_icon = AgenticTool {
        icons: vec![icon(&huge)],
        ..mcp_tool()
    };
    assert!(check(&huge_icon).unwrap_err().contains("icons"));
    // strict On refuses a recursive $ref or patternProperties, while Auto and Off publish
    let recursive = json!({
        "type": "object",
        "properties": {"root": {"$ref": "#/$defs/node"}},
        "$defs": {"node": {"type": "object", "properties": {"next": {"$ref": "#/$defs/node"}}}},
    });
    let patterned = json!({
        "type": "object",
        "properties": {"tags": {"type": "object", "properties": {}, "patternProperties": {"^x_": {"type": "string"}}}},
    });
    for (input_schema, issue) in [(recursive, "recursive"), (patterned, "patternProperties")] {
        let on = AgenticTool {
            strict: ToolStrictness::On,
            ..with_schema(&input_schema)
        };
        let err = check(&on).unwrap_err();
        assert!(err.contains("strict") && err.contains(issue), "{err}");
        for strict in [ToolStrictness::Auto, ToolStrictness::Off] {
            let tool = AgenticTool {
                strict,
                ..with_schema(&input_schema)
            };
            assert!(check(&tool).is_ok(), "{strict:?}: {input_schema}");
        }
    }
    // remote and file references are refused
    for reference in ["https://example.com/schema.json", "file:///etc/passwd"] {
        let remote = json!({"type": "object", "properties": {"x": {"$ref": reference}}});
        let err = check(&with_schema(&remote)).unwrap_err();
        assert!(err.contains("$ref"), "{err}");
    }
    // a pattern the linear-time engine can't compile is refused
    let lookahead =
        json!({"type": "object", "properties": {"x": {"type": "string", "pattern": "^(?=a)a$"}}});
    let err = check(&with_schema(&lookahead)).unwrap_err();
    assert!(err.contains("pattern"), "{err}");
    // a schema nested 32 levels deep is accepted and one nested 33 levels deep is refused
    let nested = |levels: usize| {
        let mut inner = json!({"type": "string"});
        for _ in 0..levels {
            inner = json!({"type": "array", "items": inner});
        }
        json!({"type": "object", "properties": {"x": inner}})
    };
    assert!(check(&with_schema(&nested(31))).is_ok());
    let err = check(&with_schema(&nested(32))).unwrap_err();
    assert!(err.contains("32"), "{err}");
}

/// An output schema needs JSON results
#[test]
fn output_schema_rule() {
    // an output schema with text or file results is refused
    for format in [ToolResultFormat::Text, ToolResultFormat::Files] {
        let tool = AgenticTool {
            output_schema: Some(schema(&json!({"count": {"type": "integer"}}))),
            result_format: format,
            ..mcp_tool()
        };
        let err = check(&tool).unwrap_err();
        assert!(err.contains("output_schema"), "{err}");
    }
    // with JSON results it publishes
    let json_tool = AgenticTool {
        output_schema: Some(schema(&json!({"count": {"type": "integer"}}))),
        result_format: ToolResultFormat::Json,
        ..mcp_tool()
    };
    assert!(check(&json_tool).is_ok());
}

/// The portability report lists strict issues and Gemini rewrites
#[test]
fn portability_reports() {
    // a flat schema is strict- and Gemini-compatible
    let flat = with_schema(&flat_schema()).portability();
    assert_eq!(
        flat,
        ToolPortabilityReport {
            strict_compatible: true,
            strict_issues: Vec::new(),
            gemini_compatible: true,
            gemini_rewrites: Vec::new(),
        }
    );
    // a lossy schema lists four rewrites and one strict issue
    let lossy = with_schema(&lossy_schema()).portability();
    assert!(!lossy.gemini_compatible);
    assert_eq!(lossy.gemini_rewrites.len(), 4, "{lossy:?}");
    for (rewrite, keyword) in
        lossy
            .gemini_rewrites
            .iter()
            .zip(["additionalProperties", "$ref", "oneOf", "const"])
    {
        assert!(rewrite.contains(keyword), "{rewrite}");
    }
    assert!(!lossy.strict_compatible);
    assert_eq!(lossy.strict_issues.len(), 1, "{lossy:?}");
    assert!(lossy.strict_issues[0].contains("additionalProperties"));
    // a lossy schema still publishes, since the report only warns
    assert!(check(&with_schema(&lossy_schema())).is_ok());
    // the report never serializes into a stored spec
    let raw = serde_json::to_value(with_schema(&lossy_schema())).unwrap();
    assert!(raw.get("portability").is_none());
    // allOf and type arrays are listed too, in a fixed order
    let more = json!({
        "type": "object",
        "properties": {
            "both": {"allOf": [{"type": "string"}, {"minLength": 1}]},
            "maybe": {"type": ["string", "null"]},
        },
    });
    let report = with_schema(&more).portability();
    assert_eq!(report.gemini_rewrites.len(), 2, "{report:?}");
    assert!(report.gemini_rewrites[0].contains("allOf"));
    assert!(report.gemini_rewrites[1].contains("nullable"));
    assert!(report.strict_compatible);
}

/// The JSON-in report function used by the Python bindings agrees with the typed report
#[test]
fn portability_from_json() {
    // a full spec and a bare agentic tool give the typed report
    for input_schema in [flat_schema(), lossy_schema()] {
        let tool = with_schema(&input_schema);
        let expected = serde_json::to_value(tool.portability()).unwrap();
        let spec = serde_json::to_string(&ToolSpec::Agentic(tool.clone())).unwrap();
        let bare = serde_json::to_string(&tool).unwrap();
        for raw in [spec, bare] {
            let report: Value =
                serde_json::from_str(&tool_portability_json(&raw).unwrap()).unwrap();
            assert_eq!(report, expected);
        }
    }
    // an image spec or invalid JSON is refused
    assert!(tool_portability_json(r#"{"Image": {"environment": "External"}}"#).is_err());
    assert!(tool_portability_json("{not json").is_err());
}

/// MCP annotations follow effects and `open_world`, and the definition carries title and icons
#[cfg(feature = "api")]
#[test]
fn mcp_annotation_mapping() {
    // build a tool with some effects and an open-world flag
    let tool = |effects: &[ToolEffect], open_world: bool| AgenticTool {
        capabilities: ToolCapabilities {
            effects: effects.iter().copied().collect(),
            open_world,
            ..ToolCapabilities::default()
        },
        ..mcp_tool()
    };
    // no effects in a closed world is read-only and closed
    let closed = tool(&[], false).mcp_annotations();
    assert_eq!(closed.read_only_hint, Some(true));
    assert_eq!(closed.open_world_hint, Some(false));
    // no effects in an open world is read-only and open
    let open = tool(&[], true).mcp_annotations();
    assert_eq!(open.read_only_hint, Some(true));
    assert_eq!(open.open_world_hint, Some(true));
    // writing outside Thorium is open either way
    for open_world in [false, true] {
        let external = tool(&[ToolEffect::WritesExternal], open_world).mcp_annotations();
        assert_eq!(external.open_world_hint, Some(true));
    }
    // writing only to Thorium is closed
    let thorium = tool(&[ToolEffect::WritesThorium], false).mcp_annotations();
    assert_eq!(thorium.open_world_hint, Some(false));
    // the title becomes the annotations' title
    assert_eq!(closed.title.as_deref(), Some("Get a sample"));
    // the MCP tool carries the title and icons
    let with_icons = AgenticTool {
        icons: vec![ToolIcon {
            src: "https://example.com/icon.png".into(),
            mime_type: Some("image/png".into()),
            sizes: vec!["48x48".into()],
            theme: Some(ToolIconTheme::Light),
        }],
        ..mcp_tool()
    };
    let converted = with_icons.to_mcp_tool();
    assert_eq!(converted.title.as_deref(), Some("Get a sample"));
    let icons = converted.icons.unwrap();
    assert_eq!(icons.len(), 1);
    assert_eq!(icons[0].src, "https://example.com/icon.png");
    assert_eq!(icons[0].mime_type.as_deref(), Some("image/png"));
    assert_eq!(icons[0].sizes, Some(vec!["48x48".to_owned()]));
    // a tool without icons has none
    assert!(mcp_tool().to_mcp_tool().icons.is_none());
}

/// Build an object schema whose properties form a chain of `count` references
///
/// Each property `p{i}` refers to `p{i + 1}`, and the last refers to `last`.
///
/// # Arguments
///
/// * `count` - The number of properties in the chain
/// * `last` - The schema of the last property
fn ref_chain(count: usize, last: &Value) -> Value {
    // point each property at the next one
    let mut properties = serde_json::Map::new();
    for index in 0..count - 1 {
        properties.insert(
            format!("p{index}"),
            json!({"$ref": format!("#/properties/p{}", index + 1)}),
        );
    }
    properties.insert(format!("p{}", count - 1), last.clone());
    json!({"type": "object", "properties": properties})
}

/// The portability report and checks stay fast on a long chain of references
#[test]
fn reference_chains_are_fast() {
    // a chain of 1600 references fits under the schema cap
    let chain = ref_chain(1600, &json!({"type": "string"}));
    assert!(serde_json::to_vec(&chain).unwrap().len() < DEFAULT_MAX_TOOL_SCHEMA_BYTES);
    let tool = with_schema(&chain);
    // validating and reporting on it take well under a second
    let start = std::time::Instant::now();
    assert!(check(&tool).is_ok());
    let report = tool.portability();
    let elapsed = start.elapsed();
    assert!(report.strict_compatible, "{report:?}");
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "took {elapsed:?}"
    );
    // closing the chain back to its start makes every reference in it recursive
    let looped = with_schema(&ref_chain(1600, &json!({"$ref": "#/properties/p0"})));
    let start = std::time::Instant::now();
    let report = looped.portability();
    let elapsed = start.elapsed();
    assert_eq!(
        report.strict_issues.len(),
        1600,
        "{:?}",
        &report.strict_issues[..2]
    );
    assert!(
        report
            .strict_issues
            .iter()
            .all(|issue| issue.contains("recursive")),
        "{report:?}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "took {elapsed:?}"
    );
}

/// Patterns that are slow to compile, or too many patterns, are refused quickly
#[test]
fn pattern_compile_cost_is_bounded() {
    // 60 distinct patterns with large compiled forms are refused within a couple of seconds
    let properties = (0..60)
        .map(|index| {
            (
                format!("p{index}"),
                json!({"type": "string", "pattern": format!("\\pL{{200}}{index}")}),
            )
        })
        .collect::<serde_json::Map<String, Value>>();
    let large = json!({"type": "object", "properties": properties});
    let start = std::time::Instant::now();
    let err = check(&with_schema(&large)).unwrap_err();
    let elapsed = start.elapsed();
    assert!(err.contains("pattern"), "{err}");
    assert!(err.contains("1048576 byte limit"), "{err}");
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "took {elapsed:?}"
    );
    // a pattern refused for its size in patternProperties also names the limit
    let keyed = json!({"type": "object", "properties": {}, "patternProperties": {"^\\pL{200}$": {"type": "string"}}});
    let err = check(&with_schema(&keyed)).unwrap_err();
    assert!(err.contains("1048576 byte limit"), "{err}");
    // ordinary patterns still compile, including length-bounded Unicode-wide classes
    let ordinary = json!({"type": "object", "properties": {
        "sha256": {"type": "string", "pattern": "^[a-f0-9]{64}$"},
        "email": {"type": "string", "pattern": "^[^@\\s]+@[^@\\s]+\\.[a-z]{2,}$"},
        "word": {"type": "string", "pattern": "^\\w+$"},
        "line": {"type": "string", "pattern": "^.{1,256}$"},
        "segment": {"type": "string", "pattern": "^[^/]{1,255}$"},
    }});
    let result = check(&with_schema(&ordinary));
    assert!(result.is_ok(), "{result:?}");
    // a pattern that isn't a regular expression is refused without blaming its size
    let broken =
        json!({"type": "object", "properties": {"p": {"type": "string", "pattern": "(unclosed"}}});
    let err = check(&with_schema(&broken)).unwrap_err();
    assert!(!err.contains("byte limit"), "{err}");
    // a schema with more distinct patterns than the limit is refused, naming patterns
    let many = |count: usize| {
        let properties = (0..count)
            .map(|index| {
                (
                    format!("p{index}"),
                    json!({"type": "string", "pattern": format!("^a{index}$")}),
                )
            })
            .collect::<serde_json::Map<String, Value>>();
        with_schema(&json!({"type": "object", "properties": properties}))
    };
    assert!(check(&many(64)).is_ok());
    let err = check(&many(65)).unwrap_err();
    assert!(err.contains("pattern"), "{err}");
}

/// References must land on a subschema the portability rules walk
#[test]
fn references_into_unwalked_locations() {
    // a reference to a location outside the known keywords is refused, so its target's
    // property names, missing properties object, and recursion can't hide there
    let hidden = [
        json!({"type": "object", "properties": {"a": {"$ref": "#/x"}}, "x": {"type": "object", "properties": {"bad-name": {}}}}),
        json!({"type": "object", "properties": {"a": {"$ref": "#/x"}}, "x": {"type": "object"}}),
        json!({"type": "object", "properties": {"a": {"$ref": "#/x"}}, "x": {"type": "object", "properties": {"n": {"$ref": "#/x"}}}}),
        json!({"type": "object", "properties": {"a": {"$ref": "#/properties"}}}),
        json!({"type": "object", "properties": {"a": {"$ref": "#missing"}}}),
    ];
    for input_schema in hidden {
        let err = check(&with_schema(&input_schema)).unwrap_err();
        assert!(err.contains("$ref"), "{input_schema}: {err}");
    }
    // a percent-encoded reference to a walked subschema publishes and its recursion is reported
    let encoded = json!({
        "type": "object",
        "properties": {"root": {"$ref": "#/$defs/a%20b"}},
        "$defs": {"a b": {"type": "object", "properties": {"next": {"$ref": "#/$defs/a%20b"}}}},
    });
    assert!(check(&with_schema(&encoded)).is_ok());
    let report = with_schema(&encoded).portability();
    assert_eq!(report.strict_issues.len(), 1, "{report:?}");
    assert!(report.strict_issues[0].contains("recursive"), "{report:?}");
    // dynamic and recursive references aren't strict-compatible
    for keyword in ["$dynamicRef", "$recursiveRef"] {
        let dynamic = json!({"type": "object", "properties": {"n": {keyword: "#"}}});
        let report = with_schema(&dynamic).portability();
        assert!(!report.strict_compatible, "{keyword}: {report:?}");
        assert!(
            report
                .strict_issues
                .iter()
                .any(|issue| issue.contains(keyword)),
            "{keyword}: {report:?}"
        );
        let on = AgenticTool {
            strict: ToolStrictness::On,
            ..with_schema(&dynamic)
        };
        assert!(check(&on).unwrap_err().contains(keyword));
    }
}

/// Every icon field is bounded and a `data:` icon's media type agrees with its URI
#[test]
fn icon_fields_are_bounded() {
    // build a tool with one icon
    let tool = |icon: ToolIcon| AgenticTool {
        icons: vec![icon],
        ..mcp_tool()
    };
    let https = |src: String| ToolIcon {
        src,
        ..ToolIcon::default()
    };
    // an https url over 32 KiB is refused
    let long = format!("https://example.com/{}", "a".repeat(1024 * 1024));
    let err = check(&tool(https(long))).unwrap_err();
    assert!(err.contains("icons[0]"), "{err}");
    // too many sizes, or a size that is too long, is refused
    let many_sizes = ToolIcon {
        sizes: vec!["48x48".to_owned(); 9],
        ..https("https://example.com/i.png".to_owned())
    };
    assert!(
        check(&tool(many_sizes))
            .unwrap_err()
            .contains("icons[0].sizes")
    );
    let long_size = ToolIcon {
        sizes: vec!["4".repeat(64)],
        ..https("https://example.com/i.png".to_owned())
    };
    assert!(
        check(&tool(long_size))
            .unwrap_err()
            .contains("icons[0].sizes")
    );
    // a media type that is too long is refused
    let long_type = ToolIcon {
        mime_type: Some(format!("image/{}", "p".repeat(1024))),
        ..https("https://example.com/i.png".to_owned())
    };
    assert!(
        check(&tool(long_type))
            .unwrap_err()
            .contains("icons[0].mime_type")
    );
    // a data: icon whose media type contradicts its URI is refused and a matching one publishes
    let data = |mime_type: &str| ToolIcon {
        mime_type: Some(mime_type.to_owned()),
        sizes: vec!["48x48".to_owned()],
        ..https("data:image/png;base64,iVBORw0KGgo=".to_owned())
    };
    assert!(
        check(&tool(data("image/svg+xml")))
            .unwrap_err()
            .contains("icons[0].mime_type")
    );
    assert!(check(&tool(data("image/png"))).is_ok());
}
