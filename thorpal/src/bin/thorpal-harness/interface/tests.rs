//! Tests for the run files, the start checks, and the model conversion

use futures::StreamExt;
use rig::agent::MultiTurnStreamItem;
use rig::streaming::{StreamedAssistantContent, StreamingPrompt};
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use thorium::models::{
    AwsRoleSource, AzureClientAuth, CredentialKind, CredentialType, ModelApi, ModelProvider,
    ToolInvocation, ToolMechanism,
};
use thorpal::AiProviderConfig;
use uuid::Uuid;

use super::{ConversionError, RunFiles, capabilities, plan, provider_config};

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
    // an Anthropic API on another provider names the provider
    let mut entry = fixtures::model(
        BASE_URL,
        ModelApi::AnthropicMessages,
        CredentialKind::ApiKey,
    );
    entry.provider = ModelProvider::LiteLlm {
        base_url: BASE_URL.to_owned(),
        key_header: None,
    };
    let error = provider_config(&entry).unwrap_err();
    assert!(matches!(error, ConversionError::Provider(_)), "{error:?}");
    assert!(error.to_string().contains("LiteLlm"), "{error}");
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
