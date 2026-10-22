//! Tests for the tool and tool instance models

use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use strum::IntoEnumIterator;
use uuid::Uuid;

use super::{
    AgenticTool, AgenticToolBacking, ArgTarget, ContainerToolBacking,
    DEFAULT_MAX_TOOL_SCHEMA_BYTES, HarnessInputs, HarnessInterface, HarnessOutputs, HarnessSpec,
    ToolCapabilities, ToolEffect, ToolEntity, ToolEntityRequest, ToolInputMapping,
    ToolInstanceEntity, ToolInstanceEntityRequest, ToolInstanceSummary, ToolKind, ToolLoading,
    ToolOutputDelivery, ToolResultFormat, ToolSpec, ToolStrictness, ToolUpdateFields, ToolVersion,
    is_valid_function_name, tool_instance_update_form_fields, tool_update_form_fields,
};
use crate::models::entities::IdentifyingEntityInfo;
use crate::models::{
    BareMetalEnvironment, EntityKinds, EntityMetadata, EntityMetadataRequest, K8sEnvironment,
    KvmEnvironment, SchedulingSettings, WindowsEnvironment, WorkloadEnvironment, WorkloadSpec,
};

#[cfg(any(feature = "api", feature = "client"))]
mod harness;
#[cfg(any(feature = "api", feature = "client"))]
mod model_api;

/// Build an input schema with some top-level properties
///
/// # Arguments
///
/// * `properties` - The schema of each top-level property
fn schema(properties: &Value) -> serde_json::Map<String, Value> {
    // build an object schema with these properties
    let value = json!({"type": "object", "properties": properties});
    value.as_object().unwrap().clone()
}

/// An agentic tool backed by Thorium's MCP server
fn mcp_tool() -> AgenticTool {
    // build a tool with every required field set
    AgenticTool {
        function_name: "get_sample".into(),
        title: Some("Get a sample".into()),
        model_description: "Get a sample's details".into(),
        input_schema: schema(&json!({"sha256": {"type": "string"}})),
        output_schema: None,
        capabilities: ToolCapabilities::default(),
        backing: AgenticToolBacking::ThoriumMcp {
            tool: "get_sample".into(),
        },
        result_format: ToolResultFormat::default(),
        requires_approval: false,
        timeout: Some(60),
        input_examples: Vec::new(),
        strict: ToolStrictness::Auto,
        loading: ToolLoading::Eager,
        icons: Vec::new(),
    }
}

/// An agentic tool backed by a container tool
///
/// # Arguments
///
/// * `pinned` - The Image tool instance the backing pins
fn container_tool(pinned: Uuid) -> AgenticTool {
    // build a container backing that maps every input
    let backing = ContainerToolBacking {
        tool_instance: pinned,
        inputs: ToolInputMapping {
            args: BTreeMap::from([
                ("path".to_owned(), ArgTarget::Positional),
                ("depth".to_owned(), ArgTarget::Kwarg("--depth".into())),
                (
                    "recursive".to_owned(),
                    ArgTarget::Switch("--recursive".into()),
                ),
            ]),
            samples: Some("samples".into()),
        },
        outputs: ToolOutputDelivery {
            path: Some("/tmp/out".into()),
            max_result_bytes: 1024,
        },
    };
    // build a tool whose schema has every mapped property
    AgenticTool {
        function_name: "yara_scan".into(),
        input_schema: schema(&json!({
            "path": {"type": "string"},
            "depth": {"type": "integer"},
            "recursive": {"type": "boolean"},
            "samples": {"type": "array", "items": {"type": "string"}},
        })),
        backing: AgenticToolBacking::Container(backing),
        ..mcp_tool()
    }
}

/// A K8s image workload
fn k8s_workload() -> WorkloadSpec {
    // build a K8s workload with an image and scheduling settings
    WorkloadSpec {
        environment: WorkloadEnvironment::K8s(K8sEnvironment {
            image: "repo/tool:1.0".into(),
            scheduling: SchedulingSettings::default(),
            ..K8sEnvironment::default()
        }),
        ..WorkloadSpec::default()
    }
}

/// Serialize a value and deserialize it back as JSON, checking nothing was lost
///
/// # Arguments
///
/// * `value` - The value to round-trip
fn round_trip<T: serde::Serialize + serde::de::DeserializeOwned>(value: &T) {
    // serialize the value
    let first = serde_json::to_value(value).unwrap();
    // deserialize it and serialize it again
    let back: T = serde_json::from_value(first.clone()).unwrap();
    assert_eq!(serde_json::to_value(&back).unwrap(), first);
}

/// A harness spec running a K8s workload with the standard file layout
fn harness_spec() -> HarnessSpec {
    // the K8s workload with every interface path at its default
    HarnessSpec {
        workload: k8s_workload(),
        interface: HarnessInterface::default(),
        ..HarnessSpec::default()
    }
}

/// A container tool's outputs land in its function's default directory unless a path is set,
/// and the result size a model gets is the smaller non-zero of the backing's and harness's limits
#[test]
fn output_delivery_resolves_path_and_size() {
    // an unset path delivers under the function's default directory
    let delivery = ToolOutputDelivery::default();
    assert_eq!(
        delivery.resolved_path("run_ghidra"),
        "/tmp/thorium/tool-results/run_ghidra"
    );
    // a set path is used as is
    let custom = ToolOutputDelivery {
        path: Some("/work/ghidra".to_owned()),
        max_result_bytes: 0,
    };
    assert_eq!(custom.resolved_path("run_ghidra"), "/work/ghidra");
    // the effective size for (backing, harness) limits, where 0 is unlimited
    for (backing, harness, expected) in [(0, 0, 0), (100, 0, 100), (0, 50, 50), (100, 50, 50)] {
        let delivery = ToolOutputDelivery {
            path: None,
            max_result_bytes: backing,
        };
        assert_eq!(
            delivery.effective_max_result_bytes(harness),
            expected,
            "({backing}, {harness})"
        );
    }
}

/// A tool call input maps each mapped property onto a positional, a kwarg, or a switch and
/// reads its files from the samples property; unmapped properties are left out
#[test]
fn input_mapping_maps_input() {
    // map a target, depths, a verbose flag, a quiet flag, and files
    let mapping = ToolInputMapping {
        args: BTreeMap::from([
            ("target".to_owned(), ArgTarget::Positional),
            ("extra".to_owned(), ArgTarget::Positional),
            ("depth".to_owned(), ArgTarget::Kwarg("--depth".to_owned())),
            (
                "verbose".to_owned(),
                ArgTarget::Switch("--verbose".to_owned()),
            ),
            ("quiet".to_owned(), ArgTarget::Switch("--quiet".to_owned())),
        ]),
        samples: Some("files".to_owned()),
    };
    let input = json!({
        "target": "main",
        "extra": ["a", 2],
        "depth": [1, 2],
        "verbose": true,
        "quiet": false,
        "files": ["abc", "def"],
        "unmapped": "ignored",
    });
    let mapped = mapping.map_input(input.as_object().unwrap()).unwrap();
    // positionals follow the property names in order, arrays append each value
    assert_eq!(mapped.args.positionals, vec!["a", "2", "main"]);
    assert_eq!(
        mapped.args.kwargs,
        BTreeMap::from([("--depth".to_owned(), vec!["1".to_owned(), "2".to_owned()])])
    );
    // only true switches are passed
    assert_eq!(mapped.args.switches, vec!["--verbose"]);
    assert_eq!(
        mapped.samples,
        Some(vec!["abc".to_owned(), "def".to_owned()])
    );
    // missing and null properties map to nothing, and missing files leave the samples unset
    let sparse = json!({"target": null});
    let mapped = mapping.map_input(sparse.as_object().unwrap()).unwrap();
    assert_eq!(mapped, super::MappedToolInput::default());
    // a scalar kwarg is one value
    let scalar = json!({"depth": "3"});
    let mapped = mapping.map_input(scalar.as_object().unwrap()).unwrap();
    assert_eq!(mapped.args.kwargs["--depth"], vec!["3"]);
    // a switch that isn't a boolean is refused naming its property
    let bad_switch = json!({"verbose": "yes"});
    let error = mapping
        .map_input(bad_switch.as_object().unwrap())
        .unwrap_err();
    assert!(error.contains("verbose"), "{error}");
    // files that aren't a list of strings are refused naming the property
    for files in [json!("abc"), json!([1, 2])] {
        let bad_files = json!({ "files": files });
        let error = mapping
            .map_input(bad_files.as_object().unwrap())
            .unwrap_err();
        assert!(error.contains("files"), "{error}");
    }
    // an object value is passed as its JSON text
    let object = json!({"target": {"a": 1}});
    let mapped = mapping.map_input(object.as_object().unwrap()).unwrap();
    assert_eq!(mapped.args.positionals, vec!["{\"a\":1}"]);
}

/// The harness file layout defaults to the standard Thorium paths, and overriding one path
/// leaves the others at their defaults
#[test]
fn harness_default_paths() {
    // every input defaults to its standard path
    let inputs = HarnessInputs::default();
    assert_eq!(inputs.run_request, "/tmp/thorium/ephemeral/run.json");
    assert_eq!(inputs.agent, "/tmp/thorium/ephemeral/agent.json");
    assert_eq!(inputs.tools, "/tmp/thorium/ephemeral/tools.json");
    assert_eq!(inputs.models, "/tmp/thorium/ephemeral/models.json");
    assert_eq!(inputs.thorium_keys, "/tmp/thorium/keys.yml");
    // every output defaults to the thorium-agent's result paths
    let outputs = HarnessOutputs::default();
    assert_eq!(outputs.answer, "/tmp/thorium/results");
    assert_eq!(outputs.status, "/tmp/thorium/result-files/status.json");
    // a harness without an interface gets the whole default layout
    let workload = serde_json::to_value(k8s_workload()).unwrap();
    let bare = json!({"Harness": {"workload": workload}});
    let ToolSpec::Harness(parsed) = ToolSpec::parse_request(&bare.to_string()).unwrap() else {
        panic!("expected a harness spec");
    };
    assert_eq!(parsed, harness_spec());
    // overriding one input or output path keeps the rest at their defaults
    let custom = json!({"Harness": {"workload": workload, "interface": {
        "inputs": {"models": "/work/models.json"},
        "outputs": {"status": "/work/status.json"},
    }}});
    let ToolSpec::Harness(parsed) = ToolSpec::parse_request(&custom.to_string()).unwrap() else {
        panic!("expected a harness spec");
    };
    let expected = HarnessInputs {
        models: "/work/models.json".into(),
        ..HarnessInputs::default()
    };
    assert_eq!(parsed.interface.inputs, expected);
    let expected = HarnessOutputs {
        status: "/work/status.json".into(),
        ..HarnessOutputs::default()
    };
    assert_eq!(parsed.interface.outputs, expected);
    // a misspelled interface field is refused
    let misspelled = json!({"Harness": {"workload": workload, "interface": {"input": {}}}});
    assert!(ToolSpec::parse_request(&misspelled.to_string()).is_err());
    // the spec round-trips
    round_trip(&ToolSpec::Harness(harness_spec()));
}

/// A harness must run in K8s, while Image tools may use any environment
#[test]
fn harness_workloads_are_k8s() {
    // a K8s harness is valid and has no function name or portability report
    let spec = ToolSpec::Harness(harness_spec());
    assert!(spec.validate(DEFAULT_MAX_TOOL_SCHEMA_BYTES).is_ok());
    assert_eq!(spec.kind(), ToolKind::Harness);
    assert_eq!(spec.function_name(), None);
    assert_eq!(spec.pinned_instance(), None);
    // a K8s harness still needs an image
    let mut empty = harness_spec();
    if let WorkloadEnvironment::K8s(env) = &mut empty.workload.environment {
        env.image = String::new();
    }
    let err = ToolSpec::Harness(empty)
        .validate(DEFAULT_MAX_TOOL_SCHEMA_BYTES)
        .unwrap_err();
    assert!(err.contains("environment.K8s.image"), "{err}");
    // every other environment is refused for a harness and accepted for an Image tool
    for environment in [
        WorkloadEnvironment::BareMetal(BareMetalEnvironment::default()),
        WorkloadEnvironment::Windows(WindowsEnvironment {
            image: "repo/tool:1.0".into(),
            ..WindowsEnvironment::default()
        }),
        WorkloadEnvironment::Kvm(KvmEnvironment::default()),
        WorkloadEnvironment::External,
    ] {
        // the same workload as a harness and as an Image tool
        let workload = WorkloadSpec {
            environment,
            ..WorkloadSpec::default()
        };
        let harness = ToolSpec::Harness(HarnessSpec {
            workload: workload.clone(),
            interface: HarnessInterface::default(),
            ..HarnessSpec::default()
        });
        let err = harness.validate(DEFAULT_MAX_TOOL_SCHEMA_BYTES).unwrap_err();
        assert!(err.contains("K8s"), "{err}");
        assert!(
            ToolSpec::Image(workload)
                .validate(DEFAULT_MAX_TOOL_SCHEMA_BYTES)
                .is_ok()
        );
    }
}

/// Function names, input schema types, and the schema size cap are validated
#[test]
fn agentic_validation() {
    // these names can be called by a model
    let long = format!("a{}", "B_-9".repeat(16))[..64].to_owned();
    for name in ["a", "get_strings", "yara-scan", long.as_str()] {
        assert!(is_valid_function_name(name), "{name}");
        let tool = AgenticTool {
            function_name: name.to_owned(),
            ..mcp_tool()
        };
        assert!(
            tool.validate(DEFAULT_MAX_TOOL_SCHEMA_BYTES).is_ok(),
            "{name}"
        );
    }
    // these names can't
    for name in ["", &"a".repeat(65), "my.tool", "my tool", "ツール"] {
        assert!(!is_valid_function_name(name), "{name}");
        let tool = AgenticTool {
            function_name: name.to_owned(),
            ..mcp_tool()
        };
        assert!(
            tool.validate(DEFAULT_MAX_TOOL_SCHEMA_BYTES).is_err(),
            "{name}"
        );
    }
    // a JSON array can't be an input schema
    assert!(
        serde_json::from_value::<AgenticTool>(json!({
            "function_name": "a",
            "model_description": "",
            "input_schema": [],
            "backing": {"ThoriumMcp": {"tool": "get_sample"}},
        }))
        .is_err()
    );
    // the input schema must have the object type
    for bad in [json!({"type": "array"}), json!({"properties": {}})] {
        let tool = AgenticTool {
            input_schema: bad.as_object().unwrap().clone(),
            ..mcp_tool()
        };
        assert!(
            tool.validate(DEFAULT_MAX_TOOL_SCHEMA_BYTES).is_err(),
            "{bad}"
        );
    }
    // a schema of exactly the cap is accepted and one byte over is refused
    let base = mcp_tool();
    let base_len = serde_json::to_vec(&base.input_schema).unwrap().len();
    let mut at_cap = base.clone();
    at_cap.input_schema.insert(
        "description".into(),
        Value::String("x".repeat(DEFAULT_MAX_TOOL_SCHEMA_BYTES - base_len - 17)),
    );
    assert_eq!(
        serde_json::to_vec(&at_cap.input_schema).unwrap().len(),
        DEFAULT_MAX_TOOL_SCHEMA_BYTES
    );
    assert!(at_cap.validate(DEFAULT_MAX_TOOL_SCHEMA_BYTES).is_ok());
    let mut over = at_cap.clone();
    over.input_schema.insert(
        "description".into(),
        Value::String("x".repeat(DEFAULT_MAX_TOOL_SCHEMA_BYTES - base_len - 16)),
    );
    assert!(over.validate(DEFAULT_MAX_TOOL_SCHEMA_BYTES).is_err());
    // the output schema is capped too
    let output = AgenticTool {
        output_schema: Some(over.input_schema.clone()),
        ..mcp_tool()
    };
    assert!(output.validate(DEFAULT_MAX_TOOL_SCHEMA_BYTES).is_err());
}

/// Raw JSON specs parse into typed specs and anything else is refused
#[test]
fn spec_requests_parse() {
    // a raw JSON spec parses into an equal typed spec
    let spec = ToolSpec::Agentic(mcp_tool());
    let raw = serde_json::to_string(&spec).unwrap();
    assert_eq!(ToolSpec::parse_request(&raw).unwrap(), spec);
    // whitespace and key order in the raw JSON don't matter
    let pretty = serde_json::to_string_pretty(&serde_json::to_value(&spec).unwrap()).unwrap();
    assert_eq!(ToolSpec::parse_request(&pretty).unwrap(), spec);
    // invalid JSON is refused
    assert!(ToolSpec::parse_request("{not json").is_err());
    // an unknown field is refused instead of dropped
    let mut unknown = serde_json::to_value(&spec).unwrap();
    unknown["Agentic"]["placement"] = json!("Sidecar");
    assert!(ToolSpec::parse_request(&unknown.to_string()).is_err());
    // an ExternalMcp backing is an unknown variant
    let mut external = serde_json::to_value(&spec).unwrap();
    external["Agentic"]["backing"] = json!({"ExternalMcp": {"url": "http://x", "tool": "t"}});
    assert!(ToolSpec::parse_request(&external.to_string()).is_err());
    // an unknown container backing field is refused
    let container = ToolSpec::Agentic(container_tool(Uuid::new_v4()));
    let mut placement = serde_json::to_value(&container).unwrap();
    placement["Agentic"]["backing"]["Container"]["placement"] = json!("Sidecar");
    assert!(ToolSpec::parse_request(&placement.to_string()).is_err());
    // an unknown input mapping field is refused
    let mut mapping = serde_json::to_value(&container).unwrap();
    mapping["Agentic"]["backing"]["Container"]["inputs"]["extra"] = json!(1);
    assert!(ToolSpec::parse_request(&mapping.to_string()).is_err());
    // an unknown output delivery field is refused
    let mut outputs = serde_json::to_value(&container).unwrap();
    outputs["Agentic"]["backing"]["Container"]["outputs"] = json!({"max_result_byte": 5});
    assert!(ToolSpec::parse_request(&outputs.to_string()).is_err());
    // an unknown workload field is refused
    let workload = r#"{"Image":{"environment":"External","executon":{"timeout":5}}}"#;
    assert!(ToolSpec::parse_request(workload).is_err());
    // an unknown field in each group of workload settings is refused
    let k8s = serde_json::to_value(ToolSpec::Image(k8s_workload())).unwrap();
    for path in [
        &["Image", "execution"][..],
        &["Image", "data"],
        &["Image", "environment", "K8s"],
        &["Image", "environment", "K8s", "scheduling"],
    ] {
        // add a misspelled field at this path
        let mut spec = k8s.clone();
        let mut target = &mut spec;
        for key in path {
            target = &mut target[*key];
        }
        target["timout"] = json!(5);
        assert!(
            ToolSpec::parse_request(&spec.to_string()).is_err(),
            "{path:?}"
        );
    }
    // an unknown field in every other environment is refused
    for environment in [
        WorkloadEnvironment::BareMetal(BareMetalEnvironment::default()),
        WorkloadEnvironment::Windows(WindowsEnvironment::default()),
        WorkloadEnvironment::Kvm(KvmEnvironment::default()),
    ] {
        // add a misspelled field to this environment's settings
        let mut environment = serde_json::to_value(environment).unwrap();
        if let Some(settings) = environment
            .as_object_mut()
            .and_then(|map| map.values_mut().next())
        {
            settings["timout"] = json!(5);
        }
        // a spec in this environment is refused
        let spec = json!({"Image": {"environment": environment}});
        assert!(
            ToolSpec::parse_request(&spec.to_string()).is_err(),
            "{spec}"
        );
    }
}

/// Image specs need an environment, and container environments need an image and scheduling
#[test]
fn image_spec_validation() {
    // a spec without an environment doesn't parse
    assert!(ToolSpec::parse_request(r#"{"Image": {}}"#).is_err());
    // an External environment needs no settings
    let external = ToolSpec::parse_request(r#"{"Image": {"environment": "External"}}"#).unwrap();
    assert!(external.validate(DEFAULT_MAX_TOOL_SCHEMA_BYTES).is_ok());
    // a K8s environment without an image or scheduling doesn't parse
    let scheduling = json!({"resources": {"cpu": 1, "memory": 1}, "spawn_limit": "Unlimited"});
    let no_image = json!({"Image": {"environment": {"K8s": {"scheduling": scheduling}}}});
    assert!(ToolSpec::parse_request(&no_image.to_string()).is_err());
    let no_scheduling = json!({"Image": {"environment": {"K8s": {"image": "repo/tool"}}}});
    assert!(ToolSpec::parse_request(&no_scheduling.to_string()).is_err());
    // an empty K8s or Windows image is refused, naming the field
    let empty = json!({"Image": {"environment": {"K8s": {"image": "", "scheduling": scheduling}}}});
    let err = ToolSpec::parse_request(&empty.to_string())
        .unwrap()
        .validate(DEFAULT_MAX_TOOL_SCHEMA_BYTES)
        .unwrap_err();
    assert!(err.contains("environment.K8s.image"), "{err}");
    let windows =
        json!({"Image": {"environment": {"Windows": {"image": "", "scheduling": scheduling}}}});
    let err = ToolSpec::parse_request(&windows.to_string())
        .unwrap()
        .validate(DEFAULT_MAX_TOOL_SCHEMA_BYTES)
        .unwrap_err();
    assert!(err.contains("environment.Windows.image"), "{err}");
    // a K8s environment with an image is valid
    assert!(
        ToolSpec::Image(k8s_workload())
            .validate(DEFAULT_MAX_TOOL_SCHEMA_BYTES)
            .is_ok()
    );
}

/// Every backing behind Thorium needs a timeout, and input mappings must name schema properties
#[test]
fn backing_validation() {
    // a ThoriumMcp backing without a timeout is refused
    let mcp = AgenticTool {
        timeout: None,
        ..mcp_tool()
    };
    assert!(mcp.validate(DEFAULT_MAX_TOOL_SCHEMA_BYTES).is_err());
    // a Container backing without a timeout is refused
    let container = AgenticTool {
        timeout: None,
        ..container_tool(Uuid::new_v4())
    };
    assert!(container.validate(DEFAULT_MAX_TOOL_SCHEMA_BYTES).is_err());
    // a container backing whose mapping names schema properties is valid
    assert!(
        container_tool(Uuid::new_v4())
            .validate(DEFAULT_MAX_TOOL_SCHEMA_BYTES)
            .is_ok()
    );
    // an arg that isn't a top-level property is refused, naming it
    let mut unknown_arg = container_tool(Uuid::new_v4());
    if let AgenticToolBacking::Container(backing) = &mut unknown_arg.backing {
        backing
            .inputs
            .args
            .insert("missing".into(), ArgTarget::Positional);
    }
    let err = unknown_arg
        .validate(DEFAULT_MAX_TOOL_SCHEMA_BYTES)
        .unwrap_err();
    assert!(err.contains("missing"), "{err}");
    // a samples property that isn't an array of strings is refused, naming it
    let mut bad_samples = container_tool(Uuid::new_v4());
    if let AgenticToolBacking::Container(backing) = &mut bad_samples.backing {
        backing.inputs.samples = Some("path".into());
    }
    let err = bad_samples
        .validate(DEFAULT_MAX_TOOL_SCHEMA_BYTES)
        .unwrap_err();
    assert!(err.contains("path"), "{err}");
    // a samples property that doesn't exist is refused
    let mut no_samples = container_tool(Uuid::new_v4());
    if let AgenticToolBacking::Container(backing) = &mut no_samples.backing {
        backing.inputs.samples = Some("nope".into());
    }
    assert!(no_samples.validate(DEFAULT_MAX_TOOL_SCHEMA_BYTES).is_err());
}

/// Omitted capabilities and result formats take their defaults
#[test]
fn capability_defaults() {
    // capabilities have no effects and the result format is JSON by default
    assert_eq!(
        ToolCapabilities::default(),
        ToolCapabilities {
            effects: BTreeSet::new(),
            idempotent: false,
            file_inputs: false,
            file_outputs: false,
            open_world: false,
        }
    );
    assert_eq!(ToolResultFormat::default(), ToolResultFormat::Json);
    // a spec that omits them deserializes with their defaults
    let tool: AgenticTool = serde_json::from_value(json!({
        "function_name": "a",
        "model_description": "",
        "input_schema": {"type": "object"},
        "backing": {"ThoriumMcp": {"tool": "get_sample"}},
        "timeout": 5,
    }))
    .unwrap();
    assert_eq!(tool.capabilities, ToolCapabilities::default());
    assert_eq!(tool.result_format, ToolResultFormat::Json);
    // effects and mechanisms serialize as plain strings
    assert_eq!(
        serde_json::to_value(ToolEffect::WritesThorium).unwrap(),
        json!("WritesThorium")
    );
    assert_eq!(
        serde_json::to_value(ToolEffect::Destructive).unwrap(),
        json!("Destructive")
    );
    assert_eq!(
        serde_json::to_value(ToolResultFormat::Files).unwrap(),
        json!("Files")
    );
}

/// MCP annotations follow a tool's capabilities and its definition carries over
#[cfg(feature = "api")]
#[test]
fn mcp_conversion() {
    // a tool without effects is read-only and not destructive
    let read_only = mcp_tool();
    let annotations = read_only.mcp_annotations();
    assert_eq!(annotations.read_only_hint, Some(true));
    assert_eq!(annotations.destructive_hint, Some(false));
    assert_eq!(annotations.idempotent_hint, Some(false));
    assert_eq!(annotations.open_world_hint, Some(false));
    // a destructive tool isn't read-only
    let mut destructive = mcp_tool();
    destructive
        .capabilities
        .effects
        .insert(ToolEffect::Destructive);
    let annotations = destructive.mcp_annotations();
    assert_eq!(annotations.read_only_hint, Some(false));
    assert_eq!(annotations.destructive_hint, Some(true));
    assert_eq!(annotations.open_world_hint, Some(false));
    // a writing tool isn't read-only or destructive
    let mut writes = mcp_tool();
    writes
        .capabilities
        .effects
        .insert(ToolEffect::WritesExternal);
    let annotations = writes.mcp_annotations();
    assert_eq!(annotations.read_only_hint, Some(false));
    assert_eq!(annotations.destructive_hint, Some(false));
    // writing outside Thorium reaches an open world
    assert_eq!(annotations.open_world_hint, Some(true));
    // a read-only tool that reaches outside Thorium is open too
    let mut open = mcp_tool();
    open.capabilities.open_world = true;
    let annotations = open.mcp_annotations();
    assert_eq!(annotations.read_only_hint, Some(true));
    assert_eq!(annotations.open_world_hint, Some(true));
    // an idempotent tool says so
    let mut idempotent = mcp_tool();
    idempotent.capabilities.idempotent = true;
    assert_eq!(idempotent.mcp_annotations().idempotent_hint, Some(true));
    // the MCP tool keeps the name, description, and input schema
    let tool = read_only.to_mcp_tool();
    assert_eq!(tool.name, "get_sample");
    assert_eq!(tool.description.as_deref(), Some("Get a sample's details"));
    assert_eq!(*tool.input_schema, read_only.input_schema);
    assert_eq!(tool.title.as_deref(), Some("Get a sample"));
    assert_eq!(tool.annotations, Some(read_only.mcp_annotations()));
}

/// Every type round-trips through JSON with no loss
#[test]
fn serde_round_trips() {
    // build an instance of every struct and enum
    let pinned = Uuid::new_v4();
    let now = chrono::Utc::now();
    let summary = ToolInstanceSummary {
        id: Uuid::new_v4(),
        version: ToolVersion::from("1.0.0"),
        spec_hash: "v1:abc".into(),
        deprecated: true,
        created: now,
    };
    let tool = ToolEntity {
        kind: ToolKind::Agentic,
        latest: Some(summary.id),
        latest_version: Some(summary.version.clone()),
        latest_spec_hash: Some(summary.spec_hash.clone()),
        modified: now,
        instances: vec![summary.clone()],
        harness_env: None,
        disabled_models: std::collections::BTreeSet::new(),
    };
    let instance = ToolInstanceEntity {
        tool: Uuid::new_v4(),
        version: ToolVersion::Custom("x-1".into()),
        previous: Some(Uuid::new_v4()),
        spec_hash: "v1:def".into(),
        deprecated: false,
        spec: ToolSpec::Agentic(container_tool(pinned)),
        portability: None,
        provides: Vec::new(),
    };
    let request = ToolEntityRequest::new("1.0.0", &ToolSpec::Image(k8s_workload())).unwrap();
    let instance_request =
        ToolInstanceEntityRequest::new(Uuid::new_v4(), "1.0.1", &ToolSpec::Agentic(mcp_tool()))
            .unwrap();
    // check each round-trips
    round_trip(&tool);
    round_trip(&summary);
    round_trip(&instance);
    round_trip(&ToolSpec::Agentic(mcp_tool()));
    round_trip(&ToolSpec::Image(k8s_workload()));
    round_trip(&container_tool(pinned));
    round_trip(&mcp_tool().backing);
    round_trip(&ToolCapabilities {
        effects: BTreeSet::from([ToolEffect::WritesThorium, ToolEffect::Destructive]),
        idempotent: true,
        file_inputs: true,
        file_outputs: true,
        open_world: true,
    });
    round_trip(&ToolResultFormat::Text);
    round_trip(&ToolKind::Image);
    round_trip(&request);
    round_trip(&instance_request);
    round_trip(&EntityMetadata::Tool(tool));
    round_trip(&EntityMetadata::ToolInstance(instance));
    round_trip(&EntityMetadataRequest::Tool(request));
    round_trip(&EntityMetadataRequest::ToolInstance(instance_request));
    // the instances list is left out when it's empty
    let empty = ToolEntity {
        kind: ToolKind::Image,
        latest: None,
        latest_version: None,
        latest_spec_hash: None,
        modified: now,
        instances: Vec::new(),
        harness_env: None,
        disabled_models: std::collections::BTreeSet::new(),
    };
    assert!(
        serde_json::to_value(&empty)
            .unwrap()
            .get("instances")
            .is_none()
    );
}

/// Container backings round-trip and map properties to positionals, kwargs, and switches
#[test]
fn container_backings() {
    // the backing round-trips
    let pinned = Uuid::new_v4();
    let AgenticToolBacking::Container(backing) = container_tool(pinned).backing else {
        unreachable!()
    };
    round_trip(&backing);
    // each target serializes in its own shape
    let mapping = serde_json::to_value(&backing.inputs.args).unwrap();
    assert_eq!(
        mapping,
        json!({
            "depth": {"Kwarg": "--depth"},
            "path": "Positional",
            "recursive": {"Switch": "--recursive"},
        })
    );
    // the pinned instance and the output delivery are kept
    assert_eq!(backing.tool_instance, pinned);
    assert_eq!(backing.outputs.max_result_bytes, 1024);
    // an omitted mapping and delivery take their defaults
    let minimal: ContainerToolBacking =
        serde_json::from_value(json!({"tool_instance": pinned})).unwrap();
    assert_eq!(minimal.inputs, ToolInputMapping::default());
    assert_eq!(minimal.outputs, ToolOutputDelivery::default());
}

/// Tool kinds round-trip as strings and list every kind
#[test]
fn tool_kinds() {
    // each kind round-trips through its string
    for (kind, name) in [
        (ToolKind::Agentic, "Agentic"),
        (ToolKind::Image, "Image"),
        (ToolKind::Harness, "Harness"),
    ] {
        assert_eq!(kind.to_string(), name);
        assert_eq!(kind.as_ref(), name);
        assert_eq!(name.parse::<ToolKind>().unwrap(), kind);
        assert_eq!(serde_json::to_value(kind).unwrap(), json!(name));
    }
    // iterating lists every kind
    assert_eq!(
        ToolKind::iter().collect::<Vec<ToolKind>>(),
        vec![ToolKind::Agentic, ToolKind::Image, ToolKind::Harness]
    );
    // a spec's kind is its variant
    assert_eq!(ToolSpec::Agentic(mcp_tool()).kind(), ToolKind::Agentic);
    assert_eq!(ToolSpec::Image(k8s_workload()).kind(), ToolKind::Image);
    assert_eq!(ToolSpec::Harness(harness_spec()).kind(), ToolKind::Harness);
}

/// The defaults that anchor digest pruning are the documented ones
#[test]
fn documented_defaults() {
    // a spec defaults to an empty agentic tool
    assert_eq!(
        ToolSpec::default(),
        ToolSpec::Agentic(AgenticTool::default())
    );
    // a backing defaults to an MCP tool with no name
    assert_eq!(
        AgenticToolBacking::default(),
        AgenticToolBacking::ThoriumMcp {
            tool: String::new()
        }
    );
    // an arg target defaults to a positional
    assert_eq!(ArgTarget::default(), ArgTarget::Positional);
}

/// The requests can be held in `EntityMetadataRequest`, which derives `Hash`
#[test]
fn requests_hash() {
    /// Require `Hash` at compile time
    ///
    /// # Arguments
    ///
    /// * `_value` - The value whose type must implement `Hash`
    fn hashable<T: std::hash::Hash>(_value: &T) {}
    // both requests hash
    hashable(&ToolEntityRequest::new("1.0.0", &ToolSpec::Agentic(mcp_tool())).unwrap());
    hashable(
        &ToolInstanceEntityRequest::new(Uuid::new_v4(), "1.0.0", &ToolSpec::Agentic(mcp_tool()))
            .unwrap(),
    );
}

/// Version labels parse as semver or custom labels and an empty label is refused
#[test]
fn version_labels() {
    // semver labels parse as semver
    assert!(matches!(
        ToolVersion::parse_label("2.1.0").unwrap(),
        ToolVersion::SemVer(_)
    ));
    // other labels are custom
    for raw in ["v2.1.0", "migrated"] {
        assert_eq!(
            ToolVersion::parse_label(raw).unwrap(),
            ToolVersion::Custom(raw.into())
        );
    }
    // labels print as they were given
    assert_eq!(ToolVersion::from("2.1.0").to_string(), "2.1.0");
    assert_eq!(ToolVersion::from("v2.1.0").to_string(), "v2.1.0");
    // an empty label is refused
    assert!(ToolVersion::parse_label("").is_err());
}

/// The forms send exactly the documented field names
#[test]
fn form_fields() {
    // a tool create sends its kind, version, and spec
    let request = ToolEntityRequest::new("1.0.0", &ToolSpec::Agentic(mcp_tool())).unwrap();
    let names = request
        .form_fields()
        .into_iter()
        .map(|(name, _)| name)
        .collect::<Vec<&str>>();
    assert_eq!(
        names,
        vec![
            "kind",
            "metadata[tool_kind]",
            "metadata[tool_version]",
            "metadata[tool_spec]"
        ]
    );
    // the spec is the whole JSON encoded spec in one field
    let fields = request.form_fields();
    assert_eq!(fields[0].1, "Tool");
    assert_eq!(fields[1].1, "Agentic");
    assert_eq!(
        ToolSpec::parse_request(&fields[3].1).unwrap(),
        ToolSpec::Agentic(mcp_tool())
    );
    // a direct instance create sends its tool, version, and spec
    let tool = Uuid::new_v4();
    let instance =
        ToolInstanceEntityRequest::new(tool, "1.0.1", &ToolSpec::Agentic(mcp_tool())).unwrap();
    let fields = instance.form_fields();
    assert_eq!(
        fields.iter().map(|(name, _)| *name).collect::<Vec<&str>>(),
        vec![
            "kind",
            "metadata[tool_instance_tool]",
            "metadata[tool_version]",
            "metadata[tool_spec]"
        ]
    );
    assert_eq!(fields[0].1, "ToolInstance");
    assert_eq!(fields[1].1, tool.to_string());
    // a tool update sends only the fields that are set
    let (version, spec) = ("2.0.0".to_owned(), "{}".to_owned());
    assert_eq!(
        tool_update_form_fields(ToolUpdateFields {
            version: Some(&version),
            spec: Some(&spec),
            ..ToolUpdateFields::default()
        })
        .into_iter()
        .map(|(name, _)| name)
        .collect::<Vec<&str>>(),
        vec!["metadata[tool_version]", "metadata[tool_spec]"]
    );
    assert_eq!(
        tool_update_form_fields(ToolUpdateFields::default()).len(),
        0
    );
    // a tool instance update sends only its deprecated flag
    assert_eq!(
        tool_instance_update_form_fields(Some(true)),
        vec![("metadata[tool_instance_deprecated]", "true".to_owned())]
    );
    assert_eq!(tool_instance_update_form_fields(None).len(), 0);
}

/// The `Tool` and `ToolInstance` kinds round-trip as strings, and entity JSON of every other
/// kind still loads
#[test]
fn entity_kinds() {
    // the tool kinds round-trip through their strings
    for (kind, name) in [
        (EntityKinds::Tool, "Tool"),
        (EntityKinds::ToolInstance, "ToolInstance"),
    ] {
        assert_eq!(kind.as_str(), name);
        assert_eq!(name.parse::<EntityKinds>().unwrap(), kind);
        assert_eq!(serde_json::to_value(kind).unwrap(), json!(name));
    }
    // iterating the kinds includes them
    let kinds = EntityKinds::iter().collect::<Vec<EntityKinds>>();
    assert!(kinds.contains(&EntityKinds::Tool));
    assert!(kinds.contains(&EntityKinds::ToolInstance));
    // entity metadata of every non-tool kind in its stored form
    let id = "00000000-0000-0000-0000-000000000001";
    let fixtures = [
        json!({"Device": {"urls": ["http://x"], "vendors": [], "critical_system": true, "sensitive_location": null, "critical_sectors": []}}),
        json!({"Vendor": {"countries": [], "critical_sectors": []}}),
        json!({"Collection": {"collection_kind": "Files", "collection_tags": {"k": ["v"]}, "start": null, "end": null}}),
        json!({"FileSystem": {"sha256": "abc", "tools": ["fs"]}}),
        json!({"Folder": {"filesystem_id": id, "names_sha256": "a", "data_sha256": "b", "all_sha256": "c"}}),
        json!({"WindowsProcessTree": {"tools": []}}),
        json!({"WindowsProcess": {"pid": 4, "parent_pid": null, "name": "init", "image_path": null, "command": null, "offset": null, "threads": null, "handles": null, "is_wow64": null, "session_id": null, "create_time": null, "exit_time": null}}),
        json!({"NetworkConnection": {"protocol": null, "source": "10.0.0.1", "source_port": null, "destination": "10.0.0.2", "destination_port": 443, "state": null, "pid": null, "process": null, "create_time": null}}),
        json!({"SigmaRule": {"rule": "title: x", "applies_to": ["WindowsProcesses"], "score": 1}}),
        json!({"Flag": {"suspicion": 1, "confidence": "Likely", "content": null, "reasoning": "r"}}),
        json!({"Incident": {"cover_term": null, "mission_teams": [], "networks": [], "machines": [], "locations": []}}),
        json!({"CompiledFunction": {"address": 16, "disassembly": [{"address": 16, "instruction": "ret"}]}}),
        json!({"DecompiledFunction": {"address": 16, "tools": ["ghidra"], "content": "int main() {}"}}),
        json!({"PeSection": {"md5": null, "raw_size": 1, "virtual_size": 2, "entropy": 0.5}}),
        json!({"PeImport": {"functions": ["CreateFileA"]}}),
        json!("Other"),
    ];
    // each still deserializes to its own kind
    for raw in fixtures {
        let parsed: EntityMetadata = serde_json::from_value(raw.clone())
            .unwrap_or_else(|err| panic!("{raw} failed to load: {err}"));
        let kind = EntityKinds::from(&parsed);
        assert!(
            kind != EntityKinds::Tool && kind != EntityKinds::ToolInstance,
            "{raw}"
        );
    }
}

/// The shared per-kind entity functions treat tools as plain definitions
#[cfg(feature = "client")]
#[test]
fn shared_kind_functions() {
    // build a request of each tool kind
    let tool = EntityMetadataRequest::Tool(
        ToolEntityRequest::new("1.0.0", &ToolSpec::Agentic(mcp_tool())).unwrap(),
    );
    let instance = EntityMetadataRequest::ToolInstance(
        ToolInstanceEntityRequest::new(Uuid::new_v4(), "1.0.0", &ToolSpec::Agentic(mcp_tool()))
            .unwrap(),
    );
    for meta in [tool, instance] {
        // tools aren't scanned by sigma rules
        assert_eq!(meta.to_sigma_scannable().unwrap(), None);
        // tools have no roots or parents
        let req = crate::models::EntityRequest::new("tool", meta.clone(), vec!["g"]);
        assert_eq!(req.root_kind(), None);
        assert!(req.parent_info().is_none());
        let kind = req.kind();
        assert_eq!(kind.to_sigma_applies_to(), None);
        assert_eq!(kind.root_kinds().len(), 0);
    }
}

/// Tool instances are identified by their tool and spec hash; tools aren't identifiable
#[test]
fn identifying_info() {
    // build a tool instance
    let tool = Uuid::new_v4();
    let instance = EntityMetadata::ToolInstance(ToolInstanceEntity {
        tool,
        version: ToolVersion::from("1.0.0"),
        previous: None,
        spec_hash: "v1:abc".into(),
        deprecated: false,
        spec: ToolSpec::Agentic(mcp_tool()),
        portability: None,
        provides: Vec::new(),
    });
    // it's identified by its tool and spec hash
    assert!(matches!(
        IdentifyingEntityInfo::from(&instance),
        IdentifyingEntityInfo::ToolInstance { tool: id, spec_hash } if *id == tool && spec_hash == "v1:abc"
    ));
    // a tool root isn't identifiable
    let root = EntityMetadata::Tool(ToolEntity {
        kind: ToolKind::Agentic,
        latest: None,
        latest_version: None,
        latest_spec_hash: None,
        modified: chrono::Utc::now(),
        instances: Vec::new(),
        harness_env: None,
        disabled_models: std::collections::BTreeSet::new(),
    });
    assert!(matches!(
        IdentifyingEntityInfo::from(&root),
        IdentifyingEntityInfo::Unidentifiable
    ));
}

/// Every tool type is importable through `thorium::models`
#[test]
fn reexports() {
    // name every tool type through the models re-exports
    let _ = std::any::type_name::<crate::models::ToolEntity>();
    let _ = std::any::type_name::<crate::models::ToolInstanceSummary>();
    let _ = std::any::type_name::<crate::models::ToolInstanceEntity>();
    let _ = std::any::type_name::<crate::models::ToolSpec>();
    let _ = std::any::type_name::<crate::models::ToolKind>();
    let _ = std::any::type_name::<crate::models::ToolVersion>();
    let _ = std::any::type_name::<crate::models::AgenticTool>();
    let _ = std::any::type_name::<crate::models::ToolCapabilities>();
    let _ = std::any::type_name::<crate::models::ToolEffect>();
    let _ = std::any::type_name::<crate::models::ToolResultFormat>();
    let _ = std::any::type_name::<crate::models::AgenticToolBacking>();
    let _ = std::any::type_name::<crate::models::ContainerToolBacking>();
    let _ = std::any::type_name::<crate::models::ToolInputMapping>();
    let _ = std::any::type_name::<crate::models::ArgTarget>();
    let _ = std::any::type_name::<crate::models::ToolOutputDelivery>();
    let _ = std::any::type_name::<crate::models::ToolEntityRequest>();
    let _ = std::any::type_name::<crate::models::ToolInstanceEntityRequest>();
    let _ = std::any::type_name::<crate::models::GroupPolicy>();
}

/// Digest checks on tool specs
#[cfg(any(feature = "api", feature = "client"))]
mod digests {
    use serde_json::json;
    use uuid::Uuid;

    use super::{container_tool, k8s_workload, mcp_tool};
    use crate::models::entities::digests::{DigestValue, ThorDigest};
    use crate::models::{
        AgenticTool, AgenticToolBacking, ArgTarget, ChildFilters, ContainerToolBacking,
        ToolCapabilities, ToolEffect, ToolIcon, ToolInputMapping, ToolLoading, ToolOutputDelivery,
        ToolResultFormat, ToolSpec, ToolStrictness, WorkloadEnvironment,
    };

    /// Every hashed tool type implements `DigestValue` and `Default`
    #[test]
    fn digest_types() {
        // require both traits at compile time, and only the digest for types hashed in sets
        /// Compiles only for a type that implements `DigestValue` and `Default`
        fn hashed<T: DigestValue + Default>() {}
        /// Compiles only for a type that implements `DigestValue`
        fn in_sets<T: DigestValue>() {}
        hashed::<ToolSpec>();
        hashed::<AgenticTool>();
        hashed::<ToolCapabilities>();
        hashed::<ToolResultFormat>();
        hashed::<AgenticToolBacking>();
        hashed::<ContainerToolBacking>();
        hashed::<ToolInputMapping>();
        hashed::<ArgTarget>();
        hashed::<ToolOutputDelivery>();
        hashed::<ToolStrictness>();
        hashed::<ToolLoading>();
        hashed::<ToolIcon>();
        // effects and icon themes are only hashed inside sets or options
        in_sets::<ToolEffect>();
        in_sets::<crate::models::ToolIconTheme>();
    }

    /// The same workload with its child filter sets filled in different orders hashes the same
    #[test]
    fn child_filter_order() {
        // fill the filters in one order
        let mut first = k8s_workload();
        first.data.child_filters = ChildFilters::default();
        for mime in ["b", "a", "c"] {
            first.data.child_filters.mime.insert(mime.into());
        }
        // fill them in another order
        let mut second = k8s_workload();
        for mime in ["c", "a", "b"] {
            second.data.child_filters.mime.insert(mime.into());
        }
        // both hash the same
        assert_eq!(
            ThorDigest::tool_spec(&ToolSpec::Image(first), None).unwrap(),
            ThorDigest::tool_spec(&ToolSpec::Image(second), None).unwrap()
        );
    }

    /// A spec hash covers only the spec and changes with any spec field
    #[test]
    fn spec_hash_covers_the_spec() {
        // the base spec's hash
        let base = ToolSpec::Agentic(mcp_tool());
        let hash = ThorDigest::tool_spec(&base, None).unwrap();
        // the same spec always hashes the same, whatever it's labeled or named
        assert_eq!(ThorDigest::tool_spec(&base.clone(), None).unwrap(), hash);
        // changing any single field changes the hash
        let mutations: Vec<fn(&mut AgenticTool)> = vec![
            |tool| tool.function_name = "other".into(),
            |tool| tool.title = None,
            |tool| tool.model_description = "other".into(),
            |tool| {
                tool.input_schema
                    .insert("required".into(), json!(["sha256"]));
            },
            |tool| tool.output_schema = Some(serde_json::Map::new()),
            |tool| {
                tool.capabilities.effects.insert(ToolEffect::WritesThorium);
            },
            |tool| tool.capabilities.idempotent = true,
            |tool| tool.capabilities.file_inputs = true,
            |tool| tool.capabilities.file_outputs = true,
            |tool| {
                tool.backing = AgenticToolBacking::ThoriumMcp {
                    tool: "get_sample_results".into(),
                };
            },
            |tool| tool.result_format = ToolResultFormat::Text,
            |tool| tool.requires_approval = true,
            |tool| tool.timeout = Some(61),
            |tool| tool.input_examples = vec![json!({"sha256": "abc"})],
            |tool| tool.strict = ToolStrictness::On,
            |tool| tool.loading = ToolLoading::Deferred,
            |tool| {
                tool.icons = vec![ToolIcon {
                    src: "https://example.com/icon.png".into(),
                    ..ToolIcon::default()
                }];
            },
            |tool| tool.capabilities.open_world = true,
        ];
        for mutate in mutations {
            // change one field
            let mut tool = mcp_tool();
            mutate(&mut tool);
            // the hash changes
            assert_ne!(
                ThorDigest::tool_spec(&ToolSpec::Agentic(tool), None).unwrap(),
                hash
            );
        }
        // the hash has the current scheme and is lowercase hex
        assert!(hash.starts_with("v1:"));
        assert_eq!(hash.len(), 67);
        assert!(
            hash[3..]
                .chars()
                .all(|c| matches!(c, '0'..='9' | 'a'..='f'))
        );
    }

    /// A container backing contributes the pinned instance's hash instead of its id
    #[test]
    fn pinned_hash_replaces_id() {
        // two specs pinning different instances with equal hashes hash the same
        let first = ToolSpec::Agentic(container_tool(Uuid::new_v4()));
        let second = ToolSpec::Agentic(container_tool(Uuid::new_v4()));
        assert_eq!(
            ThorDigest::tool_spec(&first, Some("v1:pinned")).unwrap(),
            ThorDigest::tool_spec(&second, Some("v1:pinned")).unwrap()
        );
        // a change to the pinned instance's hash changes the spec hash
        assert_ne!(
            ThorDigest::tool_spec(&first, Some("v1:pinned")).unwrap(),
            ThorDigest::tool_spec(&first, Some("v1:other")).unwrap()
        );
        // the pinned hash takes the instance id's place in the digest value
        let value = ThorDigest::tool_spec_value(&first, Some("v1:pinned")).unwrap();
        assert_eq!(
            value.pointer("/spec/Agentic/backing/Container/tool_instance"),
            Some(&json!("v1:pinned"))
        );
        // a container backing needs its pinned hash, and only it can have one
        assert!(ThorDigest::tool_spec(&first, None).is_err());
        assert!(ThorDigest::tool_spec(&ToolSpec::Agentic(mcp_tool()), Some("v1:x")).is_err());
    }

    /// Capabilities and result formats are part of the hash, at their defaults or set
    #[test]
    fn capabilities_are_hashed() {
        // omitted and explicit defaults hash the same
        let omitted: AgenticTool = serde_json::from_value(json!({
            "function_name": "get_sample",
            "title": "Get a sample",
            "model_description": "Get a sample's details",
            "input_schema": {"type": "object", "properties": {"sha256": {"type": "string"}}},
            "backing": {"ThoriumMcp": {"tool": "get_sample"}},
            "timeout": 60,
        }))
        .unwrap();
        assert_eq!(
            ThorDigest::tool_spec(&ToolSpec::Agentic(omitted.clone()), None).unwrap(),
            ThorDigest::tool_spec(&ToolSpec::Agentic(mcp_tool()), None).unwrap()
        );
        // a set capability or result format changes the hash
        let mut idempotent = omitted.clone();
        idempotent.capabilities.idempotent = true;
        let mut files = omitted.clone();
        files.result_format = ToolResultFormat::Files;
        let base = ThorDigest::tool_spec(&ToolSpec::Agentic(omitted), None).unwrap();
        assert_ne!(
            ThorDigest::tool_spec(&ToolSpec::Agentic(idempotent), None).unwrap(),
            base
        );
        assert_ne!(
            ThorDigest::tool_spec(&ToolSpec::Agentic(files), None).unwrap(),
            base
        );
    }

    /// The tool spec digest is tagged `tool_spec` and carries its kind as a name string
    #[test]
    fn tool_spec_tags() {
        // the tagged value names its type and kind
        let value = ThorDigest::tool_spec_value(&ToolSpec::Image(k8s_workload()), None).unwrap();
        assert_eq!(value["t"], json!("tool_spec"));
        assert_eq!(value["kind"], json!("Image"));
        let value = ThorDigest::tool_spec_value(&ToolSpec::Agentic(mcp_tool()), None).unwrap();
        assert_eq!(value["kind"], json!("Agentic"));
        // the same spec under another kind name hashes differently
        let mut renamed = value.clone();
        renamed["kind"] = json!("Image");
        assert_ne!(
            ThorDigest::of_canonical(&value).unwrap(),
            ThorDigest::of_canonical(&renamed).unwrap()
        );
    }

    /// Unpinned and digest-pinned images hash differently and an env value is hashed
    #[test]
    fn image_references_are_hashed() {
        // an image pinned by tag and by digest hash differently
        let mut tagged = k8s_workload();
        let mut pinned = k8s_workload();
        if let (WorkloadEnvironment::K8s(tag_env), WorkloadEnvironment::K8s(digest_env)) =
            (&mut tagged.environment, &mut pinned.environment)
        {
            tag_env.image = "repo/tool:latest".into();
            digest_env.image = format!("repo/tool@sha256:{}", "a".repeat(64));
        }
        assert_ne!(
            ThorDigest::tool_spec(&ToolSpec::Image(tagged), None).unwrap(),
            ThorDigest::tool_spec(&ToolSpec::Image(pinned), None).unwrap()
        );
    }
}

/// Semver strings parse to `SemVer`, anything else to `Custom`, and each
/// serializes externally tagged
#[test]
fn from_str_picks_variant() {
    // strings that are valid semver become SemVer versions
    for raw in ["1.2.3", "1.2.3-rc.1+b5"] {
        // convert this label and make sure it parsed as semver
        let version = ToolVersion::from(raw);
        assert_eq!(
            version,
            ToolVersion::SemVer(semver::Version::parse(raw).unwrap())
        );
        // make sure it serializes externally tagged as a SemVer
        assert_eq!(
            serde_json::to_string(&version).unwrap(),
            format!("{{\"SemVer\":\"{raw}\"}}")
        );
    }
    // strings that aren't valid semver become Custom versions
    for raw in ["v1.2", "1.2", "migrated"] {
        // convert this label and make sure it was kept as a custom label
        let version = ToolVersion::from(raw);
        assert_eq!(version, ToolVersion::Custom(raw.to_string()));
        // make sure it serializes externally tagged as a Custom
        assert_eq!(
            serde_json::to_string(&version).unwrap(),
            format!("{{\"Custom\":\"{raw}\"}}")
        );
    }
    // a borrowed String converts the same as a str
    assert_eq!(
        ToolVersion::from(&"1.2.3".to_string()),
        ToolVersion::from("1.2.3")
    );
}

/// Tool version JSON as Redis image hashes and Scylla result rows store it deserializes and
/// re-serializes byte-identical
#[test]
fn wire_compatible_with_image_version_json() {
    // the JSON stored in Redis image hashes and Scylla result rows
    for raw in [
        r#"{"SemVer":"1.2.3"}"#,
        r#"{"SemVer":"1.1.0-RC"}"#,
        r#"{"Custom":"x"}"#,
    ] {
        // load the stored JSON as a tool version
        let version: ToolVersion = serde_json::from_str(raw).unwrap();
        // serialize it again and make sure nothing changed
        assert_eq!(serde_json::to_string(&version).unwrap(), raw);
    }
    // an optional version that was never set stays null
    let none: Option<ToolVersion> = serde_json::from_str("null").unwrap();
    assert_eq!(none, None);
}
