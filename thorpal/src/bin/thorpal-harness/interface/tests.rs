//! Tests for the run files, the start checks, and the model conversion

use futures::StreamExt;
use rig::agent::MultiTurnStreamItem;
use rig::streaming::{StreamedAssistantContent, StreamingPrompt};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use thorium::models::{
    AgenticToolBacking, AwsRoleSource, AzureClientAuth, ContainerToolBacking, CredentialKind,
    CredentialType, ModelApi, ModelProvider, ToolInvocation, ToolMechanism,
};
use thorpal::AiProviderConfig;
use uuid::Uuid;

use super::{
    ConversionError, EnvSetting, HarnessStatus, NetworkSettings, RunFiles, StatusTool,
    capabilities, plan, provider_config,
};

#[path = "../../../../tests/common/fixtures.rs"]
#[allow(dead_code)]
mod fixtures;
#[path = "../../../../tests/common/mock.rs"]
#[allow(dead_code)]
mod mock;

use fixtures::{KEY, MODEL, RunFixture};

/// The base url of the fixture's model endpoint
const BASE_URL: &str = "http://127.0.0.1:9/anthropic";

/// Every model API, so a new one can't be left out of the conversion tests
fn all_apis() -> Vec<ModelApi> {
    // the match fails to compile when a protocol is added without listing it here
    let listed = |api: ModelApi| match api {
        ModelApi::OpenAiChat
        | ModelApi::OpenAiResponses
        | ModelApi::AnthropicMessages
        | ModelApi::BedrockConverse
        | ModelApi::BedrockInvoke
        | ModelApi::GeminiGenerateContent
        | ModelApi::GeminiInteractions
        | ModelApi::VertexRawPredict => api,
    };
    vec![
        listed(ModelApi::OpenAiChat),
        listed(ModelApi::OpenAiResponses),
        listed(ModelApi::AnthropicMessages),
        listed(ModelApi::BedrockConverse),
        listed(ModelApi::BedrockInvoke),
        listed(ModelApi::GeminiGenerateContent),
        listed(ModelApi::GeminiInteractions),
        listed(ModelApi::VertexRawPredict),
    ]
}

/// One credential of every kind, so a new kind can't be left out of the conversion tests
fn all_kinds() -> Vec<CredentialKind> {
    // one credential of each kind
    let kinds = vec![
        CredentialKind::ApiKey,
        CredentialKind::AwsAccessKey {
            access_key_id: "AKIAEXAMPLE".to_owned(),
        },
        CredentialKind::AwsAssumeRole {
            role_arn: "arn:aws:iam::123456789012:role/thorium".to_owned(),
            external_id: None,
            session_name: None,
            duration_seconds: None,
            source: AwsRoleSource::AccessKey(Uuid::nil()),
        },
        CredentialKind::AwsBearerToken,
        CredentialKind::GcpCredentialsJson {
            impersonate_service_account: None,
        },
        CredentialKind::AzureEntraClient {
            tenant_id: "tenant".to_owned(),
            client_id: "client".to_owned(),
            auth: AzureClientAuth::ClientSecret,
        },
    ];
    // the match fails to compile when a kind is added without listing it here
    for kind in &kinds {
        match kind {
            CredentialKind::ApiKey
            | CredentialKind::AwsAccessKey { .. }
            | CredentialKind::AwsAssumeRole { .. }
            | CredentialKind::AwsBearerToken
            | CredentialKind::GcpCredentialsJson { .. }
            | CredentialKind::AzureEntraClient { .. } => (),
        }
    }
    kinds
}

/// The declared capabilities convert to exactly the fixture's endpoint, key, and model, and every
/// other model API, provider, and credential kind fails with an error naming it
#[test]
fn conversion_follows_declared_capabilities() {
    // the harness declares one mechanism, one API, and one credential kind
    let declared = capabilities();
    assert_eq!(
        declared.mechanisms,
        std::collections::BTreeSet::from([ToolMechanism::ThoriumApi])
    );
    assert!(declared.file_io);
    assert!(!declared.per_agent_models);
    assert_eq!(
        declared.model_apis,
        std::collections::BTreeSet::from([ModelApi::AnthropicMessages])
    );
    assert_eq!(
        declared.credential_kinds,
        std::collections::BTreeSet::from([CredentialType::ApiKey])
    );
    // try every pairing of model API and credential kind
    for api in all_apis() {
        for kind in all_kinds() {
            let kind_type = kind.cred_type();
            let entry = fixtures::model(BASE_URL, api, kind);
            let converted = provider_config(&entry);
            if declared.model_apis.contains(&api) && declared.credential_kinds.contains(&kind_type)
            {
                // a declared pairing is exactly the fixture's endpoint, key, and model
                match converted {
                    Ok(AiProviderConfig::Anthropic {
                        endpoint,
                        api_key,
                        model,
                    }) => {
                        assert_eq!(endpoint.as_deref(), Some(BASE_URL));
                        assert_eq!(api_key, KEY);
                        assert_eq!(model, MODEL);
                    }
                    other => panic!("{api:?} with {kind_type} converted to {other:?}"),
                }
            } else {
                // anything else fails, naming the API or the credential kind
                let error = converted.expect_err("an undeclared pairing must fail");
                let message = error.to_string();
                let named = if declared.model_apis.contains(&api) {
                    kind_type.to_string()
                } else {
                    api.name()
                };
                assert!(
                    message.contains(&named),
                    "{api:?} with {kind_type}: {message}"
                );
            }
        }
    }
    // an Anthropic API on a provider other than Anthropic or LiteLlm names the provider
    let mut entry = fixtures::model(
        BASE_URL,
        ModelApi::AnthropicMessages,
        CredentialKind::ApiKey,
    );
    entry.provider = ModelProvider::OpenAiCompatible {
        base_url: BASE_URL.to_owned(),
        extra_headers: std::collections::BTreeMap::new(),
        key_header: None,
    };
    let error = provider_config(&entry).unwrap_err();
    assert!(matches!(error, ConversionError::Provider(_)), "{error:?}");
    assert!(error.to_string().contains("OpenAiCompatible"), "{error}");
    // a model without a credential or secret fails
    let mut entry = fixtures::model(
        BASE_URL,
        ModelApi::AnthropicMessages,
        CredentialKind::ApiKey,
    );
    entry.credential = None;
    assert_eq!(
        provider_config(&entry).unwrap_err(),
        ConversionError::NoCredential
    );
    let mut entry = fixtures::model(
        BASE_URL,
        ModelApi::AnthropicMessages,
        CredentialKind::ApiKey,
    );
    if let Some(credential) = entry.credential.as_mut() {
        credential.secrets.primary = None;
    }
    assert_eq!(
        provider_config(&entry).unwrap_err(),
        ConversionError::NoSecret
    );
    // the official endpoint is used when the provider names no base url
    let mut entry = fixtures::model(
        BASE_URL,
        ModelApi::AnthropicMessages,
        CredentialKind::ApiKey,
    );
    entry.provider = ModelProvider::Anthropic {
        base_url: None,
        version: "2023-06-01".to_owned(),
        betas: std::collections::BTreeSet::new(),
        workspace_id: None,
    };
    assert!(matches!(
        provider_config(&entry),
        Ok(AiProviderConfig::Anthropic { endpoint: None, .. })
    ));
}

/// Build the fixture model on a `LiteLlm` endpoint
///
/// # Arguments
///
/// * `api` - The protocol to call the model with
/// * `key_header` - The header the endpoint sends its key in, if it sets one
fn litellm(api: ModelApi, key_header: Option<&str>) -> thorium::models::HarnessModelEntry {
    // the fixture model with only its provider swapped
    let mut entry = fixtures::model(BASE_URL, api, CredentialKind::ApiKey);
    entry.provider = ModelProvider::LiteLlm {
        base_url: BASE_URL.to_owned(),
        key_header: key_header.map(str::to_owned),
    };
    entry
}

/// A `LiteLlm` endpoint called with the Anthropic Messages API converts to the Anthropic config
/// at its base url, with the key and model from the models file
#[test]
fn litellm_over_anthropic_messages_converts() {
    // no key header and an x-api-key header in any case both convert
    for key_header in [None, Some("x-api-key"), Some("X-Api-Key")] {
        let entry = litellm(ModelApi::AnthropicMessages, key_header);
        match provider_config(&entry) {
            Ok(AiProviderConfig::Anthropic {
                endpoint,
                api_key,
                model,
            }) => {
                assert_eq!(endpoint.as_deref(), Some(BASE_URL));
                assert_eq!(api_key, KEY);
                assert_eq!(model, MODEL);
            }
            other => panic!("{key_header:?} converted to {other:?}"),
        }
    }
}

/// A `LiteLlm` endpoint is refused with any API but Anthropic Messages, with a key header the
/// Anthropic client can't send, and with a credential the harness can't use
#[test]
fn litellm_refusals_name_the_reason() {
    // every other model API is refused, naming it
    for api in all_apis()
        .into_iter()
        .filter(|api| *api != ModelApi::AnthropicMessages)
    {
        let error = provider_config(&litellm(api, None)).unwrap_err();
        assert_eq!(error, ConversionError::Api(api));
        assert!(error.to_string().contains(&api.name()), "{error}");
    }
    // a key header other than x-api-key is refused, naming the header and the fix
    let error =
        provider_config(&litellm(ModelApi::AnthropicMessages, Some("Authorization"))).unwrap_err();
    assert_eq!(
        error,
        ConversionError::KeyHeader("Authorization".to_owned())
    );
    let message = error.to_string();
    assert!(message.contains("Authorization"), "{message}");
    assert!(message.contains("x-api-key"), "{message}");
    // a credential that isn't an API key is refused, naming its kind
    for kind in all_kinds()
        .into_iter()
        .filter(|kind| kind.cred_type() != CredentialType::ApiKey)
    {
        let named = kind.cred_type().to_string();
        let mut entry = litellm(ModelApi::AnthropicMessages, None);
        entry.credential = fixtures::model(BASE_URL, ModelApi::AnthropicMessages, kind).credential;
        let error = provider_config(&entry).unwrap_err();
        assert!(error.to_string().contains(&named), "{error}");
    }
    // a LiteLlm model without a credential or a secret is refused
    let mut entry = litellm(ModelApi::AnthropicMessages, None);
    entry.credential = None;
    assert_eq!(
        provider_config(&entry).unwrap_err(),
        ConversionError::NoCredential
    );
    let mut entry = litellm(ModelApi::AnthropicMessages, None);
    if let Some(credential) = entry.credential.as_mut() {
        credential.secrets.primary = None;
    }
    assert_eq!(
        provider_config(&entry).unwrap_err(),
        ConversionError::NoSecret
    );
}

/// Load a fixture's files the way the harness does
///
/// # Arguments
///
/// * `fixture` - The run to write
/// * `dir` - The directory to write it into
fn load(fixture: &RunFixture, dir: &Path) -> RunFiles {
    // write the files, then read them back through the harness's paths
    let args = fixture.write(dir);
    let paths = super::HarnessPaths::from_args(&args).unwrap();
    RunFiles::load(&paths.inputs).unwrap()
}

/// A models or keys file that doesn't parse is reported by its place and kind, quoting no value
#[test]
fn credential_file_errors_quote_no_values() {
    // write a supported run's files
    let dir = tempfile::tempdir().unwrap();
    let fixture = RunFixture::new(BASE_URL, "http://127.0.0.1:9");
    let paths = super::HarnessPaths::from_args(&fixture.write(dir.path())).unwrap();
    // a models file with a value of the wrong type, which serde would quote
    std::fs::write(&paths.inputs.models, r#"{"agents": "sk-leaked-model-key"}"#).unwrap();
    let Err(error) = RunFiles::load(&paths.inputs) else {
        panic!("a malformed models file loaded");
    };
    assert!(!error.contains("sk-leaked"), "{error}");
    assert!(
        error.contains("wrong type or shape at line 1 column"),
        "{error}"
    );
    // a keys file whose token isn't a string
    fixture.write(dir.path());
    std::fs::write(
        &paths.inputs.thorium_keys,
        "api: http://127.0.0.1:9\ntoken: {sk-leaked-token: 1}\n",
    )
    .unwrap();
    let Err(error) = RunFiles::load(&paths.inputs) else {
        panic!("a malformed keys file loaded");
    };
    assert!(!error.contains("sk-leaked"), "{error}");
    assert!(error.contains("isn't a keys file"), "{error}");
    // the other files keep the parser's text, which names the field
    fixture.write(dir.path());
    std::fs::write(&paths.inputs.run_request, "{}").unwrap();
    let Err(error) = RunFiles::load(&paths.inputs) else {
        panic!("an empty run file loaded");
    };
    assert!(error.contains("missing field"), "{error}");
}

/// Every start check refuses a run before any model call with a reason naming the problem
#[test]
fn start_checks_name_the_reason() {
    // the run's files and samples go in a scratch directory
    let dir = tempfile::tempdir().unwrap();
    let samples = dir.path().join("samples");
    // a supported run plans the fixture's model, tool, and sample
    let fixture = RunFixture::new(BASE_URL, "http://127.0.0.1:9");
    let planned = plan(&load(&fixture, dir.path()), &samples).unwrap();
    assert_eq!(planned.tools.len(), 1);
    assert_eq!(planned.binary, samples.join("a".repeat(64)));
    assert!(
        planned.prompt.contains(&"a".repeat(64)),
        "{}",
        planned.prompt
    );
    // a run with no sample
    let mut fixture = RunFixture::new(BASE_URL, "http://127.0.0.1:9");
    fixture.run.samples.clear();
    let error = plan(&load(&fixture, dir.path()), &samples).unwrap_err();
    assert!(error.contains("needs a sample"), "{error}");
    // a run whose input isn't a SHA256, which would reach outside the samples directory
    let mut fixture = RunFixture::new(BASE_URL, "http://127.0.0.1:9");
    fixture.run.samples = vec!["../../etc/passwd".to_owned()];
    let error = plan(&load(&fixture, dir.path()), &samples).unwrap_err();
    assert!(error.contains("isn't a SHA256"), "{error}");
    // a run whose entry agent delegates
    let mut fixture = RunFixture::new(BASE_URL, "http://127.0.0.1:9");
    let mut sub = fixture.agent.agent.clone();
    sub.name = "callisto-k3f9".to_owned();
    fixture.agent.sub_agents.push(sub);
    let error = plan(&load(&fixture, dir.path()), &samples).unwrap_err();
    assert!(error.contains("callisto-k3f9"), "{error}");
    // a tool that needs approval
    let mut fixture = RunFixture::new(BASE_URL, "http://127.0.0.1:9");
    for tool in fixture.tools.values_mut() {
        tool.requires_approval = true;
    }
    let error = plan(&load(&fixture, dir.path()), &samples).unwrap_err();
    assert!(error.contains("lookup_file"), "{error}");
    assert!(error.contains("approv"), "{error}");
    // a tool bound to the CLI wrapper
    let mut fixture = RunFixture::new(BASE_URL, "http://127.0.0.1:9");
    for tool in fixture.tools.values_mut() {
        tool.invocation = ToolInvocation::Cli {
            command: vec!["thorium-agent".to_owned()],
        };
    }
    let error = plan(&load(&fixture, dir.path()), &samples).unwrap_err();
    assert!(error.contains("Cli"), "{error}");
    // a model the harness can't call
    let mut fixture = RunFixture::new(BASE_URL, "http://127.0.0.1:9");
    fixture.entry_model().api = ModelApi::OpenAiChat;
    let error = plan(&load(&fixture, dir.path()), &samples).unwrap_err();
    assert!(error.contains("OpenAiChat"), "{error}");
}

/// The converted config streams from the endpoint in the models file with the fixture's key and
/// model
#[tokio::test]
async fn converted_config_streams_to_the_endpoint() {
    // a recording endpoint that answers every message
    let server = mock::MockServer::start(Arc::new(|request: &mock::Recorded, _| {
        mock::anthropic_text(request, "streamed answer")
    }))
    .await;
    // convert the fixture model pointed at the endpoint
    let entry = fixtures::model(
        &server.url,
        ModelApi::AnthropicMessages,
        CredentialKind::ApiKey,
    );
    let provider = provider_config(&entry).unwrap();
    let client = provider.connect().await.unwrap();
    let agent = client.agent(provider.model()).max_tokens(64).build();
    // stream one prompt and gather the text
    let mut stream = agent.stream_prompt("hello").await;
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        if let MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(part)) =
            item.unwrap()
        {
            text.push_str(&part.text);
        }
    }
    assert_eq!(text, "streamed answer");
    // the endpoint got a streaming request with the fixture's key and model
    let requests = server.on("/v1/messages");
    assert_eq!(requests.len(), 1, "{:?}", server.requests);
    assert_eq!(
        requests[0].headers.get("x-api-key").map(String::as_str),
        Some(KEY)
    );
    assert_eq!(requests[0].body["model"], Value::from(MODEL));
    assert_eq!(requests[0].body["stream"], Value::Bool(true));
}

/// Build a set of env vars from name and value pairs
///
/// # Arguments
///
/// * `pairs` - The env var names and values
fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    // own every name and value
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

/// Build the setting one env var gives
///
/// # Arguments
///
/// * `var` - The env var's name
/// * `value` - The value it gives
fn setting(var: &'static str, value: &str) -> EnvSetting {
    // the setting the harness reports for this var
    EnvSetting {
        var,
        value: value.to_owned(),
    }
}

/// With no proxy or CA env var set, calls go direct and TLS uses the system store
#[test]
fn network_settings_say_direct_without_a_proxy() {
    // nothing set, or only empty values, means no settings
    assert_eq!(
        NetworkSettings::from_vars(&vars(&[])),
        NetworkSettings::default()
    );
    let empty = NetworkSettings::from_vars(&vars(&[
        ("HTTPS_PROXY", ""),
        ("NO_PROXY", " "),
        ("SSL_CERT_DIR", ""),
    ]));
    assert_eq!(empty, NetworkSettings::default());
    // the log line says calls go direct with the system's CA certificates
    let line = NetworkSettings::default().to_string();
    assert!(
        line.contains("no proxy is set, so calls go direct"),
        "{line}"
    );
    assert!(line.contains("no NO_PROXY"), "{line}");
    assert!(line.contains("system store"), "{line}");
    assert_eq!(NetworkSettings::default().ca_file_problem(), None);
    assert_eq!(
        NetworkSettings::default().ca_dir_warnings(),
        Vec::<String>::new()
    );
}

/// A present but empty `SSL_CERT_FILE` replaces the system store in the HTTP clients, so it is
/// reported as set, quoted so the empty value shows
#[test]
fn network_settings_report_an_empty_ca_bundle() {
    // an empty and a blank bundle path are both kept as given
    for value in ["", " "] {
        let settings = NetworkSettings::from_vars(&vars(&[("SSL_CERT_FILE", value)]));
        assert_eq!(settings.ca_file, Some(setting("SSL_CERT_FILE", value)));
        // the log line quotes the value instead of claiming the system store
        let line = settings.to_string();
        assert!(!line.contains("system store"), "{line}");
        assert!(
            line.contains(&format!("CA certificates from SSL_CERT_FILE=\"{value}\"")),
            "{line}"
        );
        // the bundle can't be read
        assert!(settings.ca_file_problem().is_some(), "{value:?}");
    }
}

/// Proxy urls lose their user info, the upper-case name wins like it does in the HTTP clients,
/// and the CA settings are reported
#[test]
fn network_settings_redact_and_prefer_upper_case() {
    // every kind of setting, with user info in the proxies and both cases of each name
    let settings = NetworkSettings::from_vars(&vars(&[
        ("HTTPS_PROXY", "http://europa:hunter2@proxy.example:3128"),
        ("https_proxy", "http://lower.example:1"),
        ("http_proxy", "http://io:k3f9@plain.example:8080"),
        ("all_proxy", "socks5://ganymede@socks.example:1080"),
        ("NO_PROXY", "thorium-api.thorium.svc.cluster.local.,.svc"),
        ("no_proxy", "ignored.example"),
        ("SSL_CERT_FILE", "/etc/thorpal-ca/ca.pem"),
        ("SSL_CERT_DIR", "/etc/thorpal-ca/dir"),
        ("HOME", "/home/thorium"),
    ]));
    assert_eq!(
        settings,
        NetworkSettings {
            https_proxy: Some(setting("HTTPS_PROXY", "http://proxy.example:3128")),
            http_proxy: Some(setting("http_proxy", "http://plain.example:8080")),
            all_proxy: Some(setting("all_proxy", "socks5://socks.example:1080")),
            no_proxy: Some(setting(
                "NO_PROXY",
                "thorium-api.thorium.svc.cluster.local.,.svc"
            )),
            ca_file: Some(setting("SSL_CERT_FILE", "/etc/thorpal-ca/ca.pem")),
            ca_dir: Some(setting("SSL_CERT_DIR", "/etc/thorpal-ca/dir")),
        }
    );
    // the log line names every setting and none of the user info
    let line = settings.to_string();
    for expected in [
        "HTTPS_PROXY=http://proxy.example:3128",
        "http_proxy=http://plain.example:8080",
        "all_proxy=socks5://socks.example:1080",
        "NO_PROXY=thorium-api.thorium.svc.cluster.local.,.svc",
        "SSL_CERT_FILE=/etc/thorpal-ca/ca.pem",
        "SSL_CERT_DIR=/etc/thorpal-ca/dir",
    ] {
        assert!(line.contains(expected), "{expected} missing from {line}");
    }
    for secret in [
        "europa",
        "hunter2",
        "io:",
        "k3f9",
        "ganymede",
        "lower.example",
    ] {
        assert!(!line.contains(secret), "{secret} leaked into {line}");
    }
    // the lower-case names are used when the upper-case ones aren't set
    let lower = NetworkSettings::from_vars(&vars(&[
        ("https_proxy", "http://lower.example:1"),
        ("no_proxy", "a.example"),
    ]));
    assert_eq!(
        lower.https_proxy,
        Some(setting("https_proxy", "http://lower.example:1"))
    );
    assert_eq!(lower.no_proxy, Some(setting("no_proxy", "a.example")));
    // an empty upper-case name hides the lower-case one, as it does in the HTTP clients
    let hidden = NetworkSettings::from_vars(&vars(&[
        ("HTTPS_PROXY", ""),
        ("https_proxy", "http://lower.example:1"),
    ]));
    assert_eq!(hidden.https_proxy, None);
}

/// A CA bundle that can't be read and a CA directory that doesn't exist are reported, and readable
/// ones are not
#[test]
fn network_settings_check_the_ca_files() {
    // a real bundle and directory raise no problem
    let dir = tempfile::tempdir().unwrap();
    let bundle = dir.path().join("ca.pem");
    std::fs::write(&bundle, "-----BEGIN CERTIFICATE-----\n").unwrap();
    let readable = NetworkSettings::from_vars(&vars(&[
        ("SSL_CERT_FILE", bundle.to_str().unwrap()),
        ("SSL_CERT_DIR", dir.path().to_str().unwrap()),
    ]));
    assert_eq!(readable.ca_file_problem(), None);
    assert_eq!(readable.ca_dir_warnings(), Vec::<String>::new());
    // a missing bundle and directory are both named
    let missing = dir.path().join("missing.pem");
    let gone = dir.path().join("gone");
    let unreadable = NetworkSettings::from_vars(&vars(&[
        ("SSL_CERT_FILE", missing.to_str().unwrap()),
        (
            "SSL_CERT_DIR",
            &format!("{}:{}", dir.path().display(), gone.display()),
        ),
    ]));
    let problem = unreadable.ca_file_problem().unwrap();
    assert!(problem.contains("SSL_CERT_FILE"), "{problem}");
    assert!(problem.contains("missing.pem"), "{problem}");
    // only the missing directory is named
    let warnings = unreadable.ca_dir_warnings();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("SSL_CERT_DIR"), "{warnings:?}");
    assert!(warnings[0].contains("gone"), "{warnings:?}");
}

/// The planned run lists every tool the entry agent was given, in tools.json order, with its
/// tool instance and what backs it
#[test]
fn plan_lists_the_mapped_tools() {
    // the fixture run with a Container tool bound beside its MCP tool
    let dir = tempfile::tempdir().unwrap();
    let mut fixture = RunFixture::new(BASE_URL, "http://127.0.0.1:9");
    let instance = Uuid::new_v4();
    let image = Uuid::new_v4();
    let mut strings = fixtures::mcp_tool();
    strings.function_name = "strings".to_owned();
    strings.invocation = ToolInvocation::ThoriumApi {
        tool_instance: instance,
        backing: AgenticToolBacking::Container(ContainerToolBacking {
            tool_instance: image,
            inputs: thorium::models::ToolInputMapping::default(),
            outputs: thorium::models::ToolOutputDelivery::default(),
        }),
    };
    fixture.tools.insert(instance, strings);
    fixture.agent.agent.bindings.push(
        serde_json::from_value(serde_json::json!({
            "tool_instance": instance,
            "mechanism": "ThoriumApi",
        }))
        .unwrap(),
    );
    // a tool in tools.json that the entry agent isn't bound to is left out
    let unbound = Uuid::new_v4();
    let mut other = fixtures::mcp_tool();
    other.function_name = "unbound_tool".to_owned();
    fixture.tools.insert(unbound, other);
    // plan the run and list its tools
    let planned = plan(&load(&fixture, dir.path()), &dir.path().join("samples")).unwrap();
    let listed = planned.status_tools();
    let expected: Vec<StatusTool> = fixture
        .tools
        .iter()
        .filter(|(id, _)| **id != unbound)
        .map(|(id, tool)| StatusTool {
            function_name: tool.function_name.clone(),
            tool_instance: *id,
            backing: if tool.function_name == "strings" {
                format!("Container {image}")
            } else {
                "ThoriumMcp get_file".to_owned()
            },
        })
        .collect();
    assert_eq!(listed, expected);
    assert_eq!(listed.len(), 2);
    // each tool's log line names it, its instance, and its backing
    let line = listed
        .iter()
        .find(|tool| tool.function_name == "strings")
        .unwrap()
        .to_string();
    assert_eq!(
        line,
        format!("tool strings (instance {instance}): Container {image}")
    );
}

/// The status lists the run's tools when it is serialized, and a refused run lists none
#[test]
fn status_includes_the_tools() {
    // a refused run has an empty tool list
    let refused = serde_json::to_value(HarnessStatus::refused("no sample")).unwrap();
    assert_eq!(refused["tools"], serde_json::json!([]), "{refused}");
    // a run's tools are serialized with their instance and backing
    let instance = Uuid::new_v4();
    let status = HarnessStatus {
        tools: vec![StatusTool {
            function_name: "sample_info".to_owned(),
            tool_instance: instance,
            backing: "ThoriumMcp get_sample".to_owned(),
        }],
        ..HarnessStatus::default()
    };
    let value = serde_json::to_value(&status).unwrap();
    assert_eq!(
        value["tools"],
        serde_json::json!([{
            "function_name": "sample_info",
            "tool_instance": instance,
            "backing": "ThoriumMcp get_sample",
        }])
    );
}
