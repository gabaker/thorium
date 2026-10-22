//! Tests for harness capabilities, native tools, tool mechanisms, harness env, and the
//! descriptors a harness reads

use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

use super::super::{
    AgenticTool, AgenticToolBacking, ApprovalMode, BindingCheck, HarnessArgs, HarnessCapabilities,
    HarnessEnv, HarnessEnvVar, HarnessInterface, HarnessSpec, HarnessToolAccess, HarnessToolEntry,
    HarnessToolPolicy, McpConfigFormat, McpConfigTarget, OrphanedRunPolicy, ToolAnnotations,
    ToolApproval, ToolCapabilities, ToolEffect, ToolEntity, ToolExposure, ToolInvocation, ToolKind,
    ToolLoading, ToolMechanism, ToolServerAuth, ToolServerDescriptor, ToolServerInput,
    ToolServerTimeouts, ToolServerToolOverride, ToolServerTools, ToolServerTransport,
    ToolServerTrust, ToolServerValue, ToolServersFile, ToolSpec, ToolStrictness,
    is_valid_server_id,
};
use super::{harness_spec, k8s_workload, mcp_tool, round_trip};
use crate::models::entities::digests::ThorDigest;
use crate::models::{ArgStrategy, CredentialType, ModelApi};

/// Hash a harness spec the way a published harness instance with no native tools is hashed
///
/// # Arguments
///
/// * `harness` - The harness spec to hash
fn harness_hash(harness: &HarnessSpec) -> String {
    // a harness pins nothing
    ThorDigest::tool_spec(&ToolSpec::Harness(harness.clone()), None).unwrap()
}

/// A harness spec with every capability, policy, argument, and tool access setting changed
fn full_harness() -> HarnessSpec {
    // set every new part away from its default
    HarnessSpec {
        interface: HarnessInterface {
            args: HarnessArgs {
                run: ArgStrategy::Kwarg("--run".into()),
                agent: ArgStrategy::Append,
                tools: ArgStrategy::Kwarg("--tools".into()),
                models: ArgStrategy::Kwarg("--models".into()),
                thorium_keys: ArgStrategy::Kwarg("--keys".into()),
                results: ArgStrategy::Kwarg("--results".into()),
                result_files: ArgStrategy::Kwarg("--result-files".into()),
                reaction: ArgStrategy::Kwarg("--reaction".into()),
            },
            tool_access: Some(HarnessToolAccess {
                cli: vec!["/h/wrap".into(), "run".into()],
                mcp_url: Some("http://h/mcp".into()),
            }),
            mcp_config: Some(McpConfigTarget {
                format: McpConfigFormat::ClaudeCode,
                path: "/work/.mcp.json".into(),
            }),
            ..HarnessInterface::default()
        },
        capabilities: HarnessCapabilities {
            mechanisms: [ToolMechanism::Cli, ToolMechanism::Mcp]
                .into_iter()
                .collect(),
            file_io: true,
            model_apis: [ModelApi::AnthropicMessages].into_iter().collect(),
            credential_kinds: [CredentialType::ApiKey].into_iter().collect(),
            per_agent_models: true,
        },
        tool_policy: HarnessToolPolicy {
            denied_effects: [ToolEffect::Destructive].into_iter().collect(),
            max_result_bytes: 4096,
        },
        orphaned_runs: OrphanedRunPolicy::Warn,
        ..harness_spec()
    }
}

/// A native agentic tool
///
/// # Arguments
///
/// * `config` - The harness-specific config of the backing
fn native_tool(config: Option<&str>) -> AgenticTool {
    // a tool the harness carries out itself, with no timeout
    AgenticTool {
        function_name: "ghidra_decompile".into(),
        backing: AgenticToolBacking::HarnessNative {
            tool: "decompile".into(),
            config: config.map(ToOwned::to_owned),
        },
        timeout: None,
        ..mcp_tool()
    }
}

/// A harness with every part at its default keeps a fixed spec hash, so published harnesses keep
/// theirs
#[test]
fn harness_spec_hash_known_answer() {
    // the K8s workload with every harness part at its default
    assert_eq!(
        harness_hash(&harness_spec()),
        "v1:ac8f91950499e62d17eedc991b708dc6007721662b181a988b0d3b139caab67e"
    );
}

/// A harness spec's capabilities, tool policy, orphaned runs policy, args, and tool access
/// round-trip, default when they're missing, and each changes the spec hash
#[test]
fn harness_spec_parts() {
    // a harness with every part set round-trips
    let full = full_harness();
    round_trip(&ToolSpec::Harness(full.clone()));
    // a harness with only its workload takes every documented default
    let workload = serde_json::to_value(k8s_workload()).unwrap();
    let workload_only = json!({"Harness": {"workload": workload}});
    let ToolSpec::Harness(parsed) = ToolSpec::parse_request(&workload_only.to_string()).unwrap()
    else {
        panic!("expected a harness spec");
    };
    assert_eq!(parsed.orphaned_runs, OrphanedRunPolicy::Delete);
    assert!(parsed.tool_policy.denied_effects.is_empty());
    assert_eq!(parsed.tool_policy.max_result_bytes, 0);
    assert_eq!(parsed.interface.args, HarnessArgs::default());
    assert_eq!(parsed.interface.args.run, ArgStrategy::None);
    assert_eq!(parsed.interface.args.reaction, ArgStrategy::None);
    assert_eq!(parsed.interface.tool_access, None);
    assert_eq!(parsed.interface.mcp_config, None);
    assert_eq!(parsed.capabilities, HarnessCapabilities::default());
    // a harness without the new parts hashes as it did before they existed
    let base = harness_spec();
    assert_eq!(harness_hash(&parsed), harness_hash(&base));
    // each part changes the spec hash
    let base_hash = harness_hash(&base);
    let changes: Vec<(&str, HarnessSpec)> = vec![
        (
            "capabilities",
            HarnessSpec {
                capabilities: full.capabilities.clone(),
                ..base.clone()
            },
        ),
        (
            "tool_policy",
            HarnessSpec {
                tool_policy: full.tool_policy.clone(),
                ..base.clone()
            },
        ),
        (
            "orphaned_runs",
            HarnessSpec {
                orphaned_runs: OrphanedRunPolicy::Warn,
                ..base.clone()
            },
        ),
        (
            "args",
            HarnessSpec {
                interface: HarnessInterface {
                    args: full.interface.args.clone(),
                    ..HarnessInterface::default()
                },
                ..base.clone()
            },
        ),
        (
            "tool_access",
            HarnessSpec {
                interface: HarnessInterface {
                    tool_access: full.interface.tool_access.clone(),
                    ..HarnessInterface::default()
                },
                ..base.clone()
            },
        ),
    ];
    for (part, changed) in changes {
        assert_ne!(harness_hash(&changed), base_hash, "{part}");
    }
    // a misspelled part is refused
    let misspelled = json!({"Harness": {"workload": workload, "tool_polcy": {}}});
    assert!(ToolSpec::parse_request(&misspelled.to_string()).is_err());
}

/// The model capabilities of a harness default to empty and false, and each changes the spec
/// hash
#[test]
fn harness_model_capabilities() {
    // capabilities without the model fields parse empty and false
    let parsed: HarnessCapabilities =
        serde_json::from_value(json!({"mechanisms": ["Cli"]})).unwrap();
    assert!(parsed.model_apis.is_empty());
    assert!(parsed.credential_kinds.is_empty());
    assert!(!parsed.per_agent_models);
    // each model field changes the spec hash
    let base = harness_spec();
    let base_hash = harness_hash(&base);
    let with = |capabilities: HarnessCapabilities| HarnessSpec {
        capabilities,
        ..base.clone()
    };
    let changed = [
        with(HarnessCapabilities {
            model_apis: [ModelApi::OpenAiChat].into_iter().collect(),
            ..HarnessCapabilities::default()
        }),
        with(HarnessCapabilities {
            credential_kinds: [CredentialType::AwsAccessKey].into_iter().collect(),
            ..HarnessCapabilities::default()
        }),
        with(HarnessCapabilities {
            per_agent_models: true,
            ..HarnessCapabilities::default()
        }),
    ];
    for spec in changed {
        assert_ne!(harness_hash(&spec), base_hash, "{:?}", spec.capabilities);
    }
}

/// `Native` can't be declared as a harness mechanism, while the other mechanisms can
#[test]
fn native_mechanism_refused() {
    // a harness declaring Native is refused
    let mut native = harness_spec();
    native.capabilities.mechanisms = [ToolMechanism::Native, ToolMechanism::Cli]
        .into_iter()
        .collect();
    let err = ToolSpec::Harness(native).validate(1024).unwrap_err();
    assert!(err.contains("Native"), "{err}");
    // every other mechanism is accepted
    let mut other = harness_spec();
    other.capabilities.mechanisms = [
        ToolMechanism::ThoriumApi,
        ToolMechanism::Cli,
        ToolMechanism::Mcp,
    ]
    .into_iter()
    .collect();
    assert!(ToolSpec::Harness(other).validate(1024).is_ok());
}

/// A native backing needs no timeout, a tool name, and JSON config; mechanisms are derived from
/// the backing
#[test]
fn native_backings() {
    // a native tool without a timeout is valid, with or without config
    for config in [None, Some(r#"{"depth": 2}"#)] {
        let tool = native_tool(config);
        assert!(tool.validate(65_536).is_ok(), "{config:?}");
        round_trip(&ToolSpec::Agentic(tool));
    }
    // a backing without config parses with none
    let parsed: AgenticToolBacking =
        serde_json::from_value(json!({"HarnessNative": {"tool": "decompile"}})).unwrap();
    assert_eq!(
        parsed,
        AgenticToolBacking::HarnessNative {
            tool: "decompile".into(),
            config: None
        }
    );
    // an empty tool name is refused
    let mut empty = native_tool(None);
    empty.backing = AgenticToolBacking::HarnessNative {
        tool: " ".into(),
        config: None,
    };
    let err = empty.validate(65_536).unwrap_err();
    assert!(err.contains("HarnessNative.tool"), "{err}");
    // config that isn't JSON is refused
    let err = native_tool(Some("{not json")).validate(65_536).unwrap_err();
    assert!(err.contains("HarnessNative.config"), "{err}");
    // a native tool's only mechanism is Native, and tools behind Thorium get the other three
    let native = native_tool(None).backing.mechanisms();
    assert_eq!(native, BTreeSet::from([ToolMechanism::Native]));
    let behind = BTreeSet::from([
        ToolMechanism::ThoriumApi,
        ToolMechanism::Cli,
        ToolMechanism::Mcp,
    ]);
    assert_eq!(mcp_tool().backing.mechanisms(), behind);
    assert_eq!(
        super::container_tool(Uuid::new_v4()).backing.mechanisms(),
        behind
    );
    assert_eq!(native_tool(None).backing.name(), "HarnessNative");
}

/// A native backing's config is hashed byte for byte
#[test]
fn native_config_hashed_as_written() {
    // the same JSON written two ways hashes differently
    let compact = ToolSpec::Agentic(native_tool(Some(r#"{"a":1}"#)));
    let spaced = ToolSpec::Agentic(native_tool(Some(r#"{ "a": 1 }"#)));
    assert_ne!(
        ThorDigest::tool_spec(&compact, None).unwrap(),
        ThorDigest::tool_spec(&spaced, None).unwrap()
    );
}

/// A harness instance's spec hash covers the native tools it provides
#[test]
fn provides_in_spec_hash() {
    // a harness with no native tools hashes as if the field didn't exist
    let spec = ToolSpec::Harness(harness_spec());
    let plain = ThorDigest::tool_spec(&spec, None).unwrap();
    assert_eq!(
        ThorDigest::provided_tool_spec(&spec, None, &[]).unwrap(),
        plain
    );
    let value = ThorDigest::provided_tool_spec_value(&spec, None, &[]).unwrap();
    assert!(value.get("provides").is_none(), "{value}");
    // providing native tools changes the hash, in any order
    let one = ThorDigest::provided_tool_spec(&spec, None, &["v1:a"]).unwrap();
    let two = ThorDigest::provided_tool_spec(&spec, None, &["v1:a", "v1:b"]).unwrap();
    let swapped = ThorDigest::provided_tool_spec(&spec, None, &["v1:b", "v1:a"]).unwrap();
    assert_ne!(one, plain);
    assert_ne!(two, one);
    assert_eq!(two, swapped);
}

/// A harness MCP config file can't be the result files directory itself, and an answer directory
/// written with a trailing slash still keeps files out of it
#[test]
fn harness_mcp_config_directory_boundaries() {
    // validate a harness writing its neutral MCP config at a path, with an optional answer
    let validate = |path: &str, answer: Option<&str>| {
        let mut harness = harness_spec();
        if let Some(answer) = answer {
            harness.interface.outputs.answer = answer.to_owned();
        }
        harness.interface.mcp_config = Some(McpConfigTarget {
            format: McpConfigFormat::Neutral,
            path: path.to_owned(),
        });
        ToolSpec::Harness(harness).validate(1024)
    };
    // the result files directory itself is refused
    let err = validate("/tmp/thorium/result-files", None).unwrap_err();
    assert!(err.contains("inside"), "{err}");
    // a file under an answer directory written with a trailing slash is refused
    let err = validate("/tmp/thorium/out/x.json", Some("/tmp/thorium/out/")).unwrap_err();
    assert!(err.contains("inside"), "{err}");
    // names that only share a prefix with the directories are fine
    assert!(validate("/tmp/thorium/result-files.json", None).is_ok());
    assert!(validate("/tmp/thorium/outer/x.json", Some("/tmp/thorium/out/")).is_ok());
}

/// A harness MCP config path can't hide a collision behind empty, `.`, or `..` segments, land in
/// the result files or answer locations, or exceed the path length cap, and Claude Code's
/// settings file beside it can't replace one of the harness's files
#[test]
fn harness_mcp_config_paths() {
    // validate a harness writing its MCP config file in a format at a path
    let validate = |format: McpConfigFormat, path: &str, tools: Option<&str>| {
        let mut harness = harness_spec();
        if let Some(tools) = tools {
            harness.interface.inputs.tools = tools.to_owned();
        }
        harness.interface.mcp_config = Some(McpConfigTarget {
            format,
            path: path.to_owned(),
        });
        ToolSpec::Harness(harness).validate(1024)
    };
    // paths with empty, `.`, or `..` segments are refused
    for path in [
        "/tmp/thorium/ephemeral/./tools.json",
        "/tmp/thorium/ephemeral//tools.json",
        "/tmp/thorium/x/../keys.yml",
        "/work/mcp/",
    ] {
        let err = validate(McpConfigFormat::Neutral, path, None).unwrap_err();
        assert!(err.contains("segment"), "{path}: {err}");
    }
    // paths in the result files directory or under the answer are refused
    for path in [
        "/tmp/thorium/result-files/mcp.json",
        "/tmp/thorium/results/mcp.json",
    ] {
        let err = validate(McpConfigFormat::Neutral, path, None).unwrap_err();
        assert!(err.contains("inside"), "{path}: {err}");
    }
    // a path over the length cap is refused
    let long = format!("/work/{}.json", "a".repeat(4096));
    let err = validate(McpConfigFormat::Neutral, &long, None).unwrap_err();
    assert!(err.contains("longer"), "{err}");
    // Claude Code's settings file beside its config can't replace a harness file
    let err = validate(
        McpConfigFormat::ClaudeCode,
        "/work/.mcp.json",
        Some("/work/.claude/settings.json"),
    )
    .unwrap_err();
    assert!(err.contains("inputs.tools"), "{err}");
    // the same layout is fine for a format without a second file, as are names that only share
    // a prefix with the answer
    assert!(
        validate(
            McpConfigFormat::Neutral,
            "/work/.mcp.json",
            Some("/work/.claude/settings.json")
        )
        .is_ok()
    );
    assert!(validate(McpConfigFormat::ClaudeCode, "/work/.mcp.json", None).is_ok());
    assert!(validate(McpConfigFormat::Codex, "/tmp/thorium/results.toml", None).is_ok());
}

/// The models file and the Thorium keys file can't be, or land under, a directory results are
/// collected from, so credentials are never uploaded as results
#[test]
fn harness_secret_inputs_stay_out_of_results() {
    // validate a harness whose models file and keys file are at paths, with an optional answer
    let validate = |models: &str, keys: &str, answer: Option<&str>| {
        let mut harness = harness_spec();
        if let Some(answer) = answer {
            harness.interface.outputs.answer = answer.to_owned();
        }
        harness.interface.inputs.models = models.to_owned();
        harness.interface.inputs.thorium_keys = keys.to_owned();
        ToolSpec::Harness(harness).validate(1024)
    };
    // the default layout is fine
    assert!(
        validate(
            "/tmp/thorium/ephemeral/models.json",
            "/tmp/thorium/keys.yml",
            None
        )
        .is_ok()
    );
    // a models file or keys file in the result files directory or under the answer is refused,
    // naming the path
    for path in [
        "/tmp/thorium/result-files",
        "/tmp/thorium/result-files/models.json",
        "/tmp/thorium/results/models.json",
    ] {
        let err = validate(path, "/tmp/thorium/keys.yml", None).unwrap_err();
        assert!(
            err.contains(path) && err.contains("inputs.models") && err.contains("inside"),
            "{path}: {err}"
        );
        let err = validate("/tmp/thorium/ephemeral/models.json", path, None).unwrap_err();
        assert!(
            err.contains(path) && err.contains("inputs.thorium_keys") && err.contains("inside"),
            "{path}: {err}"
        );
    }
    // an answer directory written with a trailing slash still keeps credentials out of it
    let err = validate(
        "/tmp/thorium/ephemeral/models.json",
        "/tmp/thorium/out/keys.yml",
        Some("/tmp/thorium/out/"),
    )
    .unwrap_err();
    assert!(err.contains("/tmp/thorium/out/keys.yml"), "{err}");
    // a path that reaches a results directory through `..`, `.`, or an empty segment is refused
    for path in [
        "/tmp/thorium/x/../result-files/keys.yml",
        "/tmp/thorium/./keys.yml",
        "/tmp/thorium//keys.yml",
    ] {
        let err = validate("/tmp/thorium/ephemeral/models.json", path, None).unwrap_err();
        assert!(
            err.contains(path) && err.contains("segment"),
            "{path}: {err}"
        );
    }
    // names that only share a prefix with a results directory are fine
    assert!(
        validate(
            "/tmp/thorium/result-files.json",
            "/tmp/thorium/results.yml",
            None
        )
        .is_ok()
    );
}

/// The harness env reserves the `THORIUM_` prefix, `PATH`, `HOME`, `LD_*`, and `*_PROXY` names
/// whatever their case, while the standard `THORIUM_` names a harness exports stay allowed
#[test]
fn harness_env_reserved_prefixes() {
    // a rename onto each reserved name is refused, naming it
    for reserved in [
        "THORIUM_KEYS__API",
        "THORIUM_FOO",
        "thorium_foo",
        "PATH",
        "path",
        "HOME",
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "ld_audit",
        "HTTP_PROXY",
        "https_proxy",
        "NO_PROXY",
        "all_proxy",
    ] {
        let raw = json!({"export": ["AwsRegion"], "names": {"AwsRegion": reserved}});
        let err = HarnessEnv::parse_request(&raw.to_string()).unwrap_err();
        assert!(err.contains(reserved) && err.contains("reserved"), "{err}");
    }
    // names that only look like a reserved one are kept
    for name in [
        "REGION",
        "PATHS",
        "MYHOME",
        "LDX",
        "PROXY_URL",
        "THORIUM",
        "LIBRARY_PATH",
    ] {
        let raw = json!({"export": ["AwsRegion"], "names": {"AwsRegion": name}});
        assert!(
            HarnessEnv::parse_request(&raw.to_string()).is_ok(),
            "{name}"
        );
    }
    // the standard names that start with the prefix are still exported unrenamed
    let raw = json!({"export": ["ModelId", "ModelsFile"]});
    let env = HarnessEnv::parse_request(&raw.to_string()).unwrap();
    assert_eq!(env.name_of(HarnessEnvVar::ModelId), "THORIUM_MODEL");
    assert_eq!(
        env.name_of(HarnessEnvVar::ModelsFile),
        "THORIUM_MODELS_FILE"
    );
}

/// Harness MCP config files are optional, hashed only when set, and checked against the
/// harness's own files
#[test]
fn harness_mcp_config() {
    // an interface without mcp_config hashes like one written before the field existed
    let base = harness_spec();
    let workload = serde_json::to_value(k8s_workload()).unwrap();
    let old =
        json!({"Harness": {"workload": workload, "interface": {"inputs": {}, "outputs": {}}}});
    let parsed = ToolSpec::parse_request(&old.to_string()).unwrap();
    assert_eq!(
        ThorDigest::tool_spec(&parsed, None).unwrap(),
        harness_hash(&base)
    );
    // an unset mcp_config is left out of the stored JSON
    let stored = serde_json::to_value(&base).unwrap();
    assert!(stored["interface"].get("mcp_config").is_none(), "{stored}");
    // setting it changes the hash and round-trips
    let mut with = base.clone();
    with.interface.mcp_config = Some(McpConfigTarget {
        format: McpConfigFormat::Codex,
        path: "/work/config.toml".into(),
    });
    assert_ne!(harness_hash(&with), harness_hash(&base));
    round_trip(&with);
    assert!(ToolSpec::Harness(with.clone()).validate(1024).is_ok());
    // a relative path is refused
    let mut relative = with.clone();
    relative.interface.mcp_config = Some(McpConfigTarget {
        format: McpConfigFormat::Neutral,
        path: "work/mcp.json".into(),
    });
    let err = ToolSpec::Harness(relative).validate(1024).unwrap_err();
    assert!(err.contains("absolute"), "{err}");
    // the tools file's path is refused
    let mut clash = with;
    clash.interface.mcp_config = Some(McpConfigTarget {
        format: McpConfigFormat::Neutral,
        path: clash.interface.inputs.tools.clone(),
    });
    let err = ToolSpec::Harness(clash).validate(1024).unwrap_err();
    assert!(err.contains("inputs.tools"), "{err}");
}

/// A tools.json entry with every field round-trips, as does each invocation, and tools.json is
/// an object keyed by tool instance id
#[test]
fn harness_tool_entries() {
    // an entry with every field set
    let tool = mcp_tool();
    let entry = HarnessToolEntry {
        function_name: tool.function_name.clone(),
        title: tool.title.clone(),
        model_description: tool.model_description.clone(),
        input_schema: tool.input_schema.clone(),
        output_schema: Some(super::schema(&json!({"ok": {"type": "boolean"}}))),
        input_examples: vec![json!({"sha256": "abc"})],
        strict: ToolStrictness::On,
        loading: ToolLoading::Deferred,
        icons: Vec::new(),
        capabilities: ToolCapabilities {
            effects: [ToolEffect::WritesExternal].into_iter().collect(),
            idempotent: true,
            ..ToolCapabilities::default()
        },
        annotations: ToolAnnotations::from(&tool),
        portability: tool.portability(),
        result_format: tool.result_format,
        requires_approval: true,
        max_result_bytes: 500,
        timeout: Some(600),
        invocation: ToolInvocation::Cli {
            command: vec!["/opt/thorium/thorium-agent".into(), "tool".into()],
        },
    };
    round_trip(&entry);
    // every invocation round-trips
    let id = Uuid::new_v4();
    for invocation in [
        ToolInvocation::Native {
            tool: "decompile".into(),
            config: Some("{}".into()),
        },
        ToolInvocation::ThoriumApi {
            tool_instance: id,
            backing: tool.backing.clone(),
        },
        ToolInvocation::Cli {
            command: vec!["run".into()],
        },
        ToolInvocation::Mcp {
            server_url: "http://api/mcp".into(),
            tool: "run_tool".into(),
        },
    ] {
        round_trip(&invocation);
    }
    // tools.json maps each tool instance id to its entry
    let file = BTreeMap::from([(id, entry.clone())]);
    let value = serde_json::to_value(&file).unwrap();
    assert!(value.get(id.to_string()).is_some(), "{value}");
    let back: BTreeMap<Uuid, HarnessToolEntry> = serde_json::from_value(value).unwrap();
    assert_eq!(back.get(&id), Some(&entry));
    // the annotations follow the MCP mapping with MCP's field names
    let annotations = serde_json::to_value(&entry.annotations).unwrap();
    assert_eq!(annotations["readOnlyHint"], json!(true));
    assert_eq!(annotations["openWorldHint"], json!(false));
}

/// The tool server descriptor types round-trip, values parse as literals or inputs, and server
/// ids are checked
#[test]
fn tool_server_descriptors() {
    // a descriptor with every part set round-trips
    let descriptor = ToolServerDescriptor {
        id: "thorium".into(),
        title: Some("Thorium".into()),
        version: Some("2.0.0".into()),
        protocol_versions: vec!["2026-07-28".into()],
        transport: ToolServerTransport::Http {
            url: "http://api/mcp".into(),
            headers: BTreeMap::from([(
                "Authorization".into(),
                ToolServerValue::Input {
                    input: "thorium-token".into(),
                    prefix: Some("Bearer ".into()),
                },
            )]),
        },
        inputs: BTreeMap::from([(
            "thorium-token".into(),
            ToolServerInput {
                description: Some("The run token".into()),
                env: "THORIUM_TOKEN".into(),
            },
        )]),
        auth: ToolServerAuth::Bearer {
            input: "thorium-token".into(),
        },
        timeouts: ToolServerTimeouts {
            startup_ms: Some(5000),
            tool_call_ms: None,
        },
        enabled: true,
        required: true,
        tools: ToolServerTools {
            expose: ToolExposure {
                allow: vec!["run_tool".into()],
                deny: Vec::new(),
            },
            approval: ToolApproval {
                default: ApprovalMode::Auto,
                ask: vec!["run_tool".into()],
                ..ToolApproval::default()
            },
            overrides: BTreeMap::from([(
                "run_tool".into(),
                ToolServerToolOverride {
                    timeout_ms: Some(600_000),
                    output_token_limit: Some(1000),
                },
            )]),
        },
        trust: ToolServerTrust::Trusted,
        extensions: BTreeMap::from([(McpConfigFormat::Goose, json!({"timeout": 300}))]),
    };
    let stdio = ToolServerDescriptor {
        id: "local-1".into(),
        transport: ToolServerTransport::Stdio {
            command: "server".into(),
            args: vec!["--stdio".into()],
            env: BTreeMap::from([("MODE".into(), ToolServerValue::Literal("fast".into()))]),
            cwd: Some("/work".into()),
        },
        enabled: false,
        required: false,
        ..descriptor.clone()
    };
    round_trip(&ToolServersFile {
        servers: vec![descriptor, stdio],
    });
    // a plain string is a literal and an object is an input reference
    let literal: ToolServerValue = serde_json::from_value(json!("x")).unwrap();
    assert_eq!(literal, ToolServerValue::Literal("x".into()));
    let input: ToolServerValue =
        serde_json::from_value(json!({"input": "t", "prefix": "Bearer "})).unwrap();
    assert_eq!(
        input,
        ToolServerValue::Input {
            input: "t".into(),
            prefix: Some("Bearer ".into())
        }
    );
    // the defaults are no auth, asking for approval, and untrusted annotations
    assert_eq!(ToolServerAuth::default(), ToolServerAuth::None);
    assert_eq!(ApprovalMode::default(), ApprovalMode::Ask);
    assert_eq!(ToolServerTrust::default(), ToolServerTrust::Untrusted);
    // server ids are short lowercase names without underscores
    for good in ["thorium", "a-1", &"a".repeat(31)] {
        assert!(is_valid_server_id(good), "{good}");
    }
    for bad in ["my_server", "Thorium", "1x", "", &"a".repeat(32)] {
        assert!(!is_valid_server_id(bad), "{bad}");
    }
}

/// The harness env and disabled models round-trip on the root, are left out when unset, and a
/// root stored before they existed reads with neither
#[test]
fn harness_root_fields() {
    // a root with an env and disabled models round-trips
    let env = HarnessEnv {
        export: [HarnessEnvVar::OpenAiApiKey, HarnessEnvVar::ModelId]
            .into_iter()
            .collect(),
        names: BTreeMap::from([(HarnessEnvVar::OpenAiApiKey, "LITELLM_API_KEY".into())]),
    };
    let mut root = ToolEntity::new(ToolKind::Harness, chrono::Utc::now());
    root.harness_env = Some(env.clone());
    root.disabled_models = ["b-model".to_owned(), "a-model".to_owned()]
        .into_iter()
        .collect();
    round_trip(&root);
    // renamed values are keyed by their plain names and the disabled models are sorted
    let value = serde_json::to_value(&root).unwrap();
    assert_eq!(
        value["harness_env"]["names"],
        json!({"OpenAiApiKey": "LITELLM_API_KEY"})
    );
    assert_eq!(value["disabled_models"], json!(["a-model", "b-model"]));
    // an unset env and no disabled models are left out
    let empty =
        serde_json::to_value(ToolEntity::new(ToolKind::Harness, chrono::Utc::now())).unwrap();
    assert!(empty.get("harness_env").is_none(), "{empty}");
    assert!(empty.get("disabled_models").is_none(), "{empty}");
    // a root stored before the fields existed reads with neither
    let old = json!({
        "kind": "Agentic",
        "latest": null,
        "latest_version": null,
        "latest_spec_hash": null,
        "modified": "2026-01-01T00:00:00Z",
    });
    let parsed: ToolEntity = serde_json::from_value(old).unwrap();
    assert_eq!(parsed.harness_env, None);
    assert!(parsed.disabled_models.is_empty());
    // neither is part of a spec, so neither can change a spec hash
    let spec = serde_json::to_value(ToolSpec::Harness(harness_spec())).unwrap();
    assert!(!spec.to_string().contains("harness_env"));
    // a rename to a name every harness pod already sets is refused
    let reserved = HarnessEnv {
        export: [HarnessEnvVar::OpenAiApiKey].into_iter().collect(),
        names: BTreeMap::from([(HarnessEnvVar::OpenAiApiKey, "THORIUM_TOKEN".into())]),
    };
    assert!(reserved.validate().unwrap_err().contains("THORIUM_TOKEN"));
    assert!(env.validate().is_ok());
}

/// The harness env refuses renaming a value it doesn't export and renaming a value to something
/// that isn't an env var name
#[test]
fn harness_env_renames() {
    // a rename of a value that isn't exported is refused, naming the value
    let raw = json!({"export": ["OpenAiApiKey"], "names": {"AwsRegion": "REGION"}});
    let err = HarnessEnv::parse_request(&raw.to_string()).unwrap_err();
    assert!(
        err.contains("AwsRegion") && err.contains("isn't exported"),
        "{err}"
    );
    // a rename to something that isn't an env var name is refused, naming the name
    for name in ["1KEY", "MY-KEY", "", "KEY NAME"] {
        let raw = json!({"export": ["OpenAiApiKey"], "names": {"OpenAiApiKey": name}});
        let err = HarnessEnv::parse_request(&raw.to_string()).unwrap_err();
        assert!(
            err.contains(&format!("`{name}`")) && err.contains("valid env var name"),
            "{err}"
        );
    }
}

/// The harness env refuses renames onto names in use and unknown values, and accepts a new name
#[test]
fn harness_env_names() {
    // a rename onto each name the pod already sets is refused
    for reserved in ["THORIUM_URL", "THORIUM_TOKEN", "THORIUM_MODELS"] {
        let raw = json!({"export": ["AwsRegion"], "names": {"AwsRegion": reserved}});
        let err = HarnessEnv::parse_request(&raw.to_string()).unwrap_err();
        assert!(err.contains(reserved), "{err}");
    }
    // a rename onto another exported value's standard name is refused
    let raw = json!({
        "export": ["OpenAiApiKey", "AnthropicApiKey"],
        "names": {"OpenAiApiKey": "ANTHROPIC_API_KEY"},
    });
    assert!(HarnessEnv::parse_request(&raw.to_string()).is_err());
    // two renames onto the same name are refused
    let raw = json!({
        "export": ["OpenAiApiKey", "GeminiApiKey"],
        "names": {"OpenAiApiKey": "KEY", "GeminiApiKey": "KEY"},
    });
    assert!(HarnessEnv::parse_request(&raw.to_string()).is_err());
    // an unknown value and JSON that isn't an env are refused
    for raw in [
        json!({"export": ["SlackToken"]}).to_string(),
        "{not json".to_owned(),
        json!({"exports": []}).to_string(),
    ] {
        assert!(HarnessEnv::parse_request(&raw).is_err(), "{raw}");
    }
    // a new name for an exported value is kept
    let raw = json!({"export": ["OpenAiApiKey"], "names": {"OpenAiApiKey": "LITELLM_API_KEY"}});
    let env = HarnessEnv::parse_request(&raw.to_string()).unwrap();
    assert_eq!(env.name_of(HarnessEnvVar::OpenAiApiKey), "LITELLM_API_KEY");
    assert_eq!(env.name_of(HarnessEnvVar::ModelId), "THORIUM_MODEL");
}

/// Binding checks file I/O, denied effects, native providers, and the common mechanisms, and
/// picks the highest-priority common mechanism
#[test]
fn binding_rules() {
    // a harness supporting some mechanisms
    let harness = |mechanisms: &[ToolMechanism]| HarnessSpec {
        capabilities: HarnessCapabilities {
            mechanisms: mechanisms.iter().copied().collect(),
            ..HarnessCapabilities::default()
        },
        ..harness_spec()
    };
    let check = |tool: &ToolSpec, harness: &HarnessSpec, provided: bool| {
        BindingCheck {
            tool_name: "scan@1.0.0",
            tool,
            harness_name: "PalHarness@0.0.1",
            harness,
            provided,
        }
        .bind()
    };
    // the highest-priority common mechanism wins
    let container = ToolSpec::Agentic(super::container_tool(Uuid::new_v4()));
    let cases = [
        (
            vec![ToolMechanism::Cli, ToolMechanism::Mcp],
            ToolMechanism::Cli,
        ),
        (
            vec![ToolMechanism::ThoriumApi, ToolMechanism::Mcp],
            ToolMechanism::ThoriumApi,
        ),
        (vec![ToolMechanism::Mcp], ToolMechanism::Mcp),
    ];
    for (mechanisms, expected) in cases {
        assert_eq!(
            check(&container, &harness(&mechanisms), false),
            Ok(expected)
        );
    }
    // no common mechanism names the tool, what it needs, and the harness
    let err = check(&container, &harness(&[]), false).unwrap_err();
    assert!(err.contains("scan@1.0.0"), "{err}");
    assert!(err.contains("ThoriumApi, Cli, or Mcp"), "{err}");
    assert!(err.contains("PalHarness@0.0.1"), "{err}");
    // a tool moving files needs file I/O
    let mut files = mcp_tool();
    files.capabilities.file_outputs = true;
    let files = ToolSpec::Agentic(files);
    let err = check(&files, &harness(&[ToolMechanism::Cli]), false).unwrap_err();
    assert!(err.contains("file_io"), "{err}");
    let mut with_io = harness(&[ToolMechanism::Cli]);
    with_io.capabilities.file_io = true;
    assert_eq!(check(&files, &with_io, false), Ok(ToolMechanism::Cli));
    // a denied effect is refused
    let mut destructive = mcp_tool();
    destructive.capabilities.effects = [ToolEffect::Destructive].into_iter().collect();
    let destructive = ToolSpec::Agentic(destructive);
    let mut denying = harness(&[ToolMechanism::Cli]);
    denying.tool_policy.denied_effects = [ToolEffect::Destructive].into_iter().collect();
    let err = check(&destructive, &denying, false).unwrap_err();
    assert!(err.contains("Destructive"), "{err}");
    // a native tool binds to Native only when it's provided, without declaring Native
    let native = ToolSpec::Agentic(native_tool(None));
    assert_eq!(
        check(&native, &harness(&[]), true),
        Ok(ToolMechanism::Native)
    );
    let err = check(&native, &harness(&[ToolMechanism::Cli]), false).unwrap_err();
    assert!(
        err.contains("isn't provided by `PalHarness@0.0.1`"),
        "{err}"
    );
    // a submitted mechanism must be one both sides support
    let binding = BindingCheck {
        tool_name: "scan@1.0.0",
        tool: &container,
        harness_name: "PalHarness@0.0.1",
        harness: &harness(&[ToolMechanism::Cli, ToolMechanism::Mcp]),
        provided: false,
    };
    assert!(binding.check(ToolMechanism::Mcp).is_ok());
    assert!(binding.check(ToolMechanism::ThoriumApi).is_err());
    assert!(binding.check(ToolMechanism::Native).is_err());
}

/// Every new harness type serializes its enums as plain strings
#[test]
fn harness_enums_are_strings() {
    // mechanisms, formats, and policies serialize as their names
    let value: Value = serde_json::to_value(ToolMechanism::ThoriumApi).unwrap();
    assert_eq!(value, json!("ThoriumApi"));
    assert_eq!(
        serde_json::to_value(McpConfigFormat::GeminiCli).unwrap(),
        json!("GeminiCli")
    );
    assert_eq!(
        serde_json::to_value(OrphanedRunPolicy::Delete).unwrap(),
        json!("Delete")
    );
    assert_eq!(McpConfigFormat::default(), McpConfigFormat::Neutral);
    assert_eq!(ToolMechanism::Cli.to_string(), "Cli");
}
