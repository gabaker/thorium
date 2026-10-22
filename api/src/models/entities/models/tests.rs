//! Tests for the model endpoint, profile, and credential models

use chrono::Utc;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Debug;
use std::str::FromStr;
use strum::IntoEnumIterator;
use uuid::Uuid;

use super::{
    AwsRoleSource, AzureApiMode, AzureClientAuth, BedrockEndpointKind, CatalogEntrySource,
    CredentialKind, CredentialSecrets, CredentialType, GeminiApiVersion, LimitsSource, ModelApi,
    ModelAuthLevel, ModelAuthSource, ModelBan, ModelBanKind, ModelBanUpdate, ModelCapabilities,
    ModelCatalogEntry, ModelCatalogEntryRequest, ModelConfigRef, ModelCredentialEntity,
    ModelCredentialEntityRequest, ModelEndpointEntity, ModelEndpointEntityRequest, ModelLimits,
    ModelMatcher, ModelParameters, ModelProfileEntity, ModelProfileEntityRequest, ModelProvider,
    ModelProviderKind, ModelRefreshState, ModelRefreshStatus, ModelRequirements, ModelSecret,
    RedactSecrets, VertexLocation, VertexMultiRegion, build_catalog, update_form_fields,
};
use crate::models::entities::IdentifyingEntityInfo;
use crate::models::{
    Entity, EntityKinds, EntityMetadata, EntityMetadataRequest, EntityMetadataUpdate,
};

/// The secret value every test credential holds
const SECRET: &str = "sk-test-model-secret-value";

/// Serialize a value to JSON and back, checking nothing changed
///
/// # Arguments
///
/// * `value` - The value to round trip
fn round_trip<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: &T) {
    // serialize and deserialize the value
    let json = serde_json::to_string(value).unwrap();
    let back: T = serde_json::from_str(&json).unwrap();
    assert_eq!(&back, value, "the value changed in a round trip");
}

/// Serialize a value to JSON and back, checking it serializes the same both times
///
/// # Arguments
///
/// * `value` - The value to round trip
fn round_trip_json<T: Serialize + DeserializeOwned>(value: &T) {
    // compare the serialized values since the type isn't comparable
    let first = serde_json::to_value(value).unwrap();
    let back: T = serde_json::from_value(first.clone()).unwrap();
    assert_eq!(serde_json::to_value(&back).unwrap(), first);
}

/// One of every provider with its settings set
fn every_provider() -> Vec<ModelProvider> {
    vec![
        ModelProvider::OpenAi {
            base_url: Some("https://api.openai.com/v1".into()),
            organization: Some("org".into()),
            project: Some("proj".into()),
        },
        ModelProvider::Anthropic {
            base_url: None,
            version: "2023-06-01".into(),
            betas: BTreeSet::from(["beta-a".to_owned()]),
            workspace_id: Some("ws".into()),
        },
        ModelProvider::Bedrock {
            region: "us-east-1".into(),
            endpoint_kind: BedrockEndpointKind::Mantle,
            endpoint_url: Some("https://vpce.local".into()),
            workspace_id: None,
        },
        ModelProvider::Vertex {
            project_id: Some("project".into()),
            location: VertexLocation::MultiRegion(VertexMultiRegion::Eu),
            endpoint_url: None,
        },
        ModelProvider::Gemini {
            base_url: None,
            api_version: GeminiApiVersion::V1,
        },
        ModelProvider::AzureOpenAi {
            endpoint: "https://res.openai.azure.com".into(),
            api_mode: AzureApiMode::Legacy {
                api_version: "2024-10-21".into(),
            },
            entra_scope: None,
        },
        ModelProvider::LiteLlm {
            base_url: "https://gw.local".into(),
            key_header: Some("x-gateway-key".into()),
        },
        ModelProvider::OpenAiCompatible {
            base_url: "http://vllm.local:8000/v1".into(),
            extra_headers: BTreeMap::from([("X-Request-Source".to_owned(), "thorium".to_owned())]),
            key_header: None,
        },
    ]
}

/// One of every credential kind with its metadata set
fn every_kind() -> Vec<CredentialKind> {
    vec![
        CredentialKind::ApiKey,
        CredentialKind::AwsAccessKey {
            access_key_id: "AKIA".into(),
        },
        CredentialKind::AwsAssumeRole {
            role_arn: "arn:aws:iam::1:role/r".into(),
            external_id: Some("ext".into()),
            session_name: None,
            duration_seconds: Some(900),
            source: AwsRoleSource::AccessKey(Uuid::new_v4()),
        },
        CredentialKind::AwsBearerToken,
        CredentialKind::GcpCredentialsJson {
            impersonate_service_account: Some("sa@p.iam".into()),
        },
        CredentialKind::AzureEntraClient {
            tenant_id: "tenant".into(),
            client_id: "client".into(),
            auth: AzureClientAuth::Certificate,
        },
    ]
}

/// A credential entity holding a secret and a session token
fn credential() -> ModelCredentialEntity {
    ModelCredentialEntity {
        kind: CredentialKind::AwsAccessKey {
            access_key_id: "AKIA".into(),
        },
        secrets: CredentialSecrets {
            primary: Some(ModelSecret::from(SECRET)),
            session_token: Some(ModelSecret::from("session-token-value")),
            fingerprint: Some(ModelSecret::from(SECRET).fingerprint()),
        },
        expires: Some(Utc::now()),
        rotated: Some(Utc::now()),
        bans: HashMap::default(),
        modified: Utc::now(),
    }
}

/// An endpoint entity with a catalog, bans, and disabled models
fn endpoint() -> ModelEndpointEntity {
    // a catalog entry with a ban
    let ban = ModelBan::new(ModelBanKind::ModelRemoved);
    let entry = ModelCatalogEntry {
        model_id: "claude-sonnet-4-5".into(),
        limits: ModelLimits {
            context_window: Some(200_000),
            max_output_tokens: Some(64_000),
            source: LimitsSource::Refreshed { at: Utc::now() },
        },
        capabilities: ModelCapabilities {
            tool_calling: true,
            thinking: true,
            ..ModelCapabilities::default()
        },
        bans: HashMap::from([(ban.id, ban)]),
        source: CatalogEntrySource::Refreshed {
            first_seen: Utc::now(),
        },
    };
    let endpoint_ban = ModelBan::generic("paused");
    ModelEndpointEntity {
        provider: every_provider().remove(1),
        credential: Some(Uuid::new_v4()),
        catalog: vec![entry],
        bans: HashMap::from([(endpoint_ban.id, endpoint_ban)]),
        refresh: ModelRefreshStatus {
            state: ModelRefreshState::Failed,
            requested: Some(Utc::now()),
            finished: Some(Utc::now()),
            error: Some("unreachable".into()),
        },
        disabled_models: BTreeSet::from(["claude-sonnet-latest".to_owned()]),
        modified: Utc::now(),
    }
}

/// A profile entity with float parameters and overrides
fn profile() -> ModelProfileEntity {
    ModelProfileEntity {
        endpoint: Uuid::new_v4(),
        model_id: "claude-sonnet-4-5".into(),
        api: ModelApi::AnthropicMessages,
        parameters: ModelParameters {
            temperature: Some(0.2),
            top_p: Some(0.9),
            max_output_tokens: Some(4096),
            thinking_budget_tokens: Some(1024),
            stop_sequences: vec!["STOP".into()],
        },
        limits: Some(ModelLimits::default()),
        capabilities: Some(ModelCapabilities::default()),
        bans: HashMap::default(),
        modified: Utc::now(),
    }
}

/// Wrap model metadata in an entity
///
/// # Arguments
///
/// * `metadata` - The metadata to wrap
fn entity(metadata: EntityMetadata) -> Entity {
    // build an entity of the metadata's kind
    Entity {
        id: Uuid::new_v4(),
        name: "model".into(),
        kind: EntityKinds::from(&metadata),
        metadata,
        description: None,
        submitter: "user".into(),
        groups: vec!["g".into()],
        tags: HashMap::default(),
        image: None,
        created: Utc::now(),
    }
}

/// An Azure endpoint's Entra scope must be an Azure AI scope, so a token is never minted for
/// another resource
#[test]
fn entra_scopes() {
    // build an Azure endpoint with a scope
    let azure = |scope: Option<&str>| ModelProvider::AzureOpenAi {
        endpoint: "https://thorium.openai.azure.com".to_owned(),
        api_mode: AzureApiMode::V1,
        entra_scope: scope.map(ToOwned::to_owned),
    };
    // the default and the Azure AI scopes are accepted
    for scope in [
        None,
        Some("https://ai.azure.com/.default"),
        Some("https://cognitiveservices.azure.com/.default"),
        Some("https://thorium.cognitiveservices.azure.com/.default"),
    ] {
        assert!(azure(scope).validate().is_ok(), "{scope:?}");
    }
    // any other resource's scope is refused naming the field
    for scope in [
        "https://graph.microsoft.com/.default",
        "https://management.azure.com/.default",
        "https://evil.example/cognitiveservices.azure.com/.default",
        "https://cognitiveservices.azure.com.evil.example/.default",
        "https://ai.azure.com/user_impersonation",
        "",
    ] {
        let error = azure(Some(scope)).validate().unwrap_err();
        assert!(error.contains("entra_scope"), "{scope}: {error}");
    }
}

/// Every provider's settings and every credential kind round trip, and providers that aren't
/// supported directly are refused by name
#[test]
fn providers_and_kinds_round_trip() {
    // every provider and every kind round trips
    for provider in every_provider() {
        round_trip(&provider);
    }
    for kind in every_kind() {
        round_trip(&kind);
    }
    // providers reached only through gateways have no variant
    for name in ["DeepSeek", "Mistral", "AnthropicCompatible"] {
        let raw = json!({ name: {"base_url": "https://x.local"} });
        assert!(
            serde_json::from_value::<ModelProvider>(raw).is_err(),
            "{name}"
        );
        assert!(ModelProviderKind::from_str(name).is_err(), "{name}");
    }
}

/// Every struct and enum round trips, and the kind enums convert to and from strings
#[test]
fn every_type_round_trips() {
    // the entity structs
    round_trip(&endpoint());
    round_trip(&profile());
    round_trip(&credential());
    // the shared structs and enums
    round_trip(&ModelCatalogEntryRequest::new("gpt-5"));
    round_trip(
        &ModelBanUpdate::default()
            .add_ban(ModelBan::generic("x"))
            .remove_ban(Uuid::new_v4()),
    );
    round_trip(&ModelRefreshStatus::default());
    for kind in [
        ModelBanKind::Generic("x".into()),
        ModelBanKind::ModelRemoved,
        ModelBanKind::CredentialRejected {
            status: 401,
            message: "bad key".into(),
        },
        ModelBanKind::CredentialExpired,
        ModelBanKind::EndpointUnreachable("timeout".into()),
        ModelBanKind::BrokenReference(Uuid::new_v4()),
        ModelBanKind::NotAllowedByDeployment,
    ] {
        round_trip(&ModelBan::new(kind));
    }
    round_trip(&ModelMatcher {
        model_id: "gpt-5".into(),
        provider: Some(ModelProviderKind::OpenAi),
        api: Some(ModelApi::OpenAiResponses),
        api_versions: BTreeSet::from(["v1".to_owned()]),
    });
    round_trip(&ModelRequirements {
        min_context_window: Some(1),
        min_output_tokens: Some(2),
        capabilities: ModelCapabilities {
            vision: true,
            ..ModelCapabilities::default()
        },
    });
    round_trip(&ModelAuthSource {
        levels: BTreeSet::from([ModelAuthLevel::User, ModelAuthLevel::Group]),
    });
    round_trip(&ModelConfigRef::Group(Uuid::new_v4()));
    round_trip(&ModelConfigRef::User(Uuid::new_v4()));
    // requests and metadata compare by their JSON
    round_trip_json(&ModelEndpointEntityRequest::new(every_provider().remove(0)).disable("x"));
    round_trip_json(&ModelProfileEntityRequest::new(
        Uuid::new_v4(),
        "m",
        ModelApi::OpenAiChat,
    ));
    round_trip_json(&ModelCredentialEntityRequest::new(
        CredentialKind::ApiKey,
        Some(SECRET.into()),
    ));
    round_trip_json(&EntityMetadata::ModelEndpoint(endpoint()));
    round_trip_json(&EntityMetadata::ModelProfile(profile()));
    round_trip_json(&EntityMetadata::ModelCredential(credential()));
    round_trip_json(&EntityMetadataUpdate::model_endpoint().disable_model("x"));
    round_trip_json(&EntityMetadataUpdate::model_profile().clear_profile_limits());
    round_trip_json(&EntityMetadataUpdate::model_credential().credential_secret(SECRET));
    // the kind enums convert to and from strings and list every variant
    let providers = ModelProviderKind::iter().collect::<Vec<_>>();
    assert_eq!(providers.len(), every_provider().len());
    for kind in providers {
        assert_eq!(ModelProviderKind::from_str(kind.as_ref()).unwrap(), kind);
    }
    let types = CredentialType::iter().collect::<Vec<_>>();
    assert_eq!(types.len(), every_kind().len());
    for kind in types {
        assert_eq!(CredentialType::from_str(kind.as_ref()).unwrap(), kind);
    }
}

/// Require a type to implement `Hash`
fn hashable<T: std::hash::Hash>() {}

/// The requests fit in the hashed request enum, and both secret types are importable
#[test]
fn requests_hash_and_names_import() {
    // every request implements Hash
    hashable::<ModelEndpointEntityRequest>();
    hashable::<ModelProfileEntityRequest>();
    hashable::<ModelCredentialEntityRequest>();
    hashable::<EntityMetadataRequest>();
    // the model secret and the volume secret are both importable through the models module
    let _ = std::any::type_name::<crate::models::ModelSecret>();
    let _ = std::any::type_name::<crate::models::Secret>();
}

/// No `Debug` output of a value holding a secret prints the secret
#[test]
fn debug_never_prints_secrets() {
    // the secret type and the structs holding it
    let secret = ModelSecret::from(SECRET);
    let request = ModelCredentialEntityRequest {
        kind: CredentialKind::AwsAccessKey {
            access_key_id: "AKIA".into(),
        },
        secret: Some(secret.clone()),
        session_token: Some("session-token-value".into()),
        expires: None,
    };
    let outputs = [
        format!("{secret:?}"),
        format!("{:?}", credential().secrets),
        format!("{:?}", credential()),
        format!("{:#?}", EntityMetadata::ModelCredential(credential())),
        format!(
            "{:?}",
            EntityMetadataRequest::ModelCredential(request.clone())
        ),
        format!("{request:?}"),
        format!(
            "{:?}",
            EntityMetadataUpdate::model_credential()
                .credential_secret(SECRET)
                .credential_session_token("session-token-value")
        ),
    ];
    for output in outputs {
        assert!(
            !output.contains(SECRET),
            "a debug output printed the secret"
        );
        assert!(
            !output.contains("session-token-value"),
            "a debug output printed the session token"
        );
    }
}

/// The secret type serializes as its plain value, prints a fixed redaction, and fingerprints with
/// the start of its SHA-256
#[test]
fn secret_format_and_fingerprint() {
    // serde sees the plain value
    let secret = ModelSecret::from("sk-test");
    assert_eq!(serde_json::to_value(&secret).unwrap(), json!("sk-test"));
    let back: ModelSecret = serde_json::from_value(json!("sk-test")).unwrap();
    assert_eq!(back, secret);
    // debug is exactly the redaction
    assert_eq!(format!("{secret:?}"), "ModelSecret(<redacted>)");
    // the fingerprint is the first 12 hex characters of the secret's SHA-256
    assert_eq!(secret.fingerprint(), "f3abf2a6cc4f");
    assert_ne!(
        ModelSecret::from("sk-other").fingerprint(),
        secret.fingerprint()
    );
    // a credential without a session token serializes no session token key
    let secrets = CredentialSecrets {
        primary: Some(secret),
        session_token: None,
        fingerprint: Some("f3abf2a6cc4f".into()),
    };
    let value = serde_json::to_value(&secrets).unwrap();
    assert!(value.get("session_token").is_none(), "{value}");
}

/// Redacting an entity clears a credential's secrets, keeps its fingerprint, and leaves other
/// kinds alone
#[test]
fn entity_redaction() {
    // a credential loses its secret values
    let mut cred = entity(EntityMetadata::ModelCredential(credential()));
    cred.redact_secrets();
    let EntityMetadata::ModelCredential(meta) = &cred.metadata else {
        unreachable!()
    };
    assert_eq!(meta.secrets.primary, None);
    assert_eq!(meta.secrets.session_token, None);
    assert_eq!(
        meta.secrets.fingerprint,
        Some(ModelSecret::from(SECRET).fingerprint())
    );
    // the redacted entity's JSON holds no secret
    let json = serde_json::to_string(&cred).unwrap();
    assert!(
        !json.contains(SECRET),
        "the redacted entity holds the secret"
    );
    // other kinds are unchanged
    let mut other = entity(EntityMetadata::ModelEndpoint(endpoint()));
    let before = serde_json::to_value(&other).unwrap();
    other.redact_secrets();
    assert_eq!(serde_json::to_value(&other).unwrap(), before);
}

/// Defaults fill in absent settings
#[test]
fn defaults() {
    // an Anthropic endpoint without a version sends the default one
    let anthropic: ModelProvider = serde_json::from_value(json!({"Anthropic": {}})).unwrap();
    let ModelProvider::Anthropic { version, betas, .. } = anthropic else {
        unreachable!()
    };
    assert_eq!(version, "2023-06-01");
    assert!(betas.is_empty());
    // the enum defaults
    assert_eq!(BedrockEndpointKind::default(), BedrockEndpointKind::Runtime);
    assert_eq!(GeminiApiVersion::default(), GeminiApiVersion::V1Beta);
    assert_eq!(LimitsSource::default(), LimitsSource::Manual);
    assert_eq!(ModelRefreshState::default(), ModelRefreshState::Never);
    assert_eq!(AzureClientAuth::default(), AzureClientAuth::ClientSecret);
    assert!(ModelAuthSource::default().levels.is_empty());
    // absent provider settings use their defaults
    let bedrock: ModelProvider =
        serde_json::from_value(json!({"Bedrock": {"region": "us-east-1"}})).unwrap();
    assert!(matches!(
        bedrock,
        ModelProvider::Bedrock {
            endpoint_kind: BedrockEndpointKind::Runtime,
            ..
        }
    ));
    let gemini: ModelProvider = serde_json::from_value(json!({"Gemini": {}})).unwrap();
    assert!(matches!(
        gemini,
        ModelProvider::Gemini {
            api_version: GeminiApiVersion::V1Beta,
            ..
        }
    ));
}

/// Build an OpenAI-compatible provider with some extra headers and an optional key header
///
/// # Arguments
///
/// * `headers` - The extra header names
/// * `key_header` - The header the key is sent in, if not the default
fn compatible(headers: &[&str], key_header: Option<&str>) -> ModelProvider {
    ModelProvider::OpenAiCompatible {
        base_url: "https://gw.example/v1".into(),
        extra_headers: headers
            .iter()
            .map(|name| ((*name).to_owned(), "value".to_owned()))
            .collect(),
        key_header: key_header.map(ToOwned::to_owned),
    }
}

/// Protocols, credential kinds, and extra headers follow the provider table
#[test]
fn provider_table_checks() {
    // a Bedrock protocol on an OpenAI endpoint is refused
    let openai = every_provider().remove(0);
    assert!(openai.check_api(ModelApi::BedrockConverse).is_err());
    // an API key on a Vertex endpoint with a project is refused
    let vertex = every_provider().remove(3);
    assert!(vertex.check_credential(&CredentialKind::ApiKey).is_err());
    // an authorization extra header is refused
    assert!(compatible(&["authorization"], None).validate().is_err());
}

/// Extra header names are checked without case, including the variant's own key header
#[test]
fn extra_header_names() {
    // every known key header is refused whatever its case
    for name in ["Authorization", "X-API-KEY", "Api-Key", "x-goog-api-key"] {
        assert!(compatible(&[name], None).validate().is_err(), "{name}");
    }
    // a header named like the key header is refused
    assert!(
        compatible(&["X-Gateway-Key"], Some("x-gateway-key"))
            .validate()
            .is_err()
    );
    // other headers are accepted and round trip
    let ok = compatible(&["X-Request-Source"], None);
    assert!(ok.validate().is_ok());
    round_trip(&ok);
}

/// Required provider settings must be set and urls must be web urls
#[test]
fn required_provider_settings() {
    // empty required settings are refused and named
    let empty = [
        (
            ModelProvider::Bedrock {
                region: String::new(),
                endpoint_kind: BedrockEndpointKind::Runtime,
                endpoint_url: None,
                workspace_id: None,
            },
            "Bedrock.region",
        ),
        (
            ModelProvider::AzureOpenAi {
                endpoint: String::new(),
                api_mode: AzureApiMode::V1,
                entra_scope: None,
            },
            "AzureOpenAi.endpoint",
        ),
        (
            ModelProvider::LiteLlm {
                base_url: String::new(),
                key_header: None,
            },
            "LiteLlm.base_url",
        ),
    ];
    for (provider, field) in empty {
        let err = provider.validate().unwrap_err();
        assert!(err.contains(field), "{err}");
    }
    let mut empty_compatible = compatible(&[], None);
    if let ModelProvider::OpenAiCompatible { base_url, .. } = &mut empty_compatible {
        base_url.clear();
    }
    assert!(
        empty_compatible
            .validate()
            .unwrap_err()
            .contains("OpenAiCompatible.base_url")
    );
    // urls that aren't http or https are refused
    for url in ["ftp://gw", "gw.example", "not a url"] {
        let provider = ModelProvider::LiteLlm {
            base_url: url.into(),
            key_header: None,
        };
        assert!(provider.validate().is_err(), "{url}");
        let openai = ModelProvider::OpenAi {
            base_url: Some(url.into()),
            organization: None,
            project: None,
        };
        assert!(openai.validate().is_err(), "{url}");
    }
    // Vertex needs a project for service accounts but not for express-mode keys
    let express = ModelProvider::Vertex {
        project_id: None,
        location: VertexLocation::Global,
        endpoint_url: None,
    };
    assert!(
        express
            .check_credential(&CredentialKind::GcpCredentialsJson {
                impersonate_service_account: None
            })
            .is_err()
    );
    assert!(express.check_credential(&CredentialKind::ApiKey).is_ok());
}

/// Regions must be host labels and project and workspace ids must be single path segments, since
/// Thorium builds provider urls from them
#[test]
fn provider_identifiers_are_safe() {
    // regions that would change the host a default url points at are refused
    for region in [
        "attacker.example/#",
        "us-east-1.evil",
        "US-EAST-1",
        "-us",
        "us-",
        "us--east",
    ] {
        let bedrock = ModelProvider::Bedrock {
            region: region.into(),
            endpoint_kind: BedrockEndpointKind::Runtime,
            endpoint_url: None,
            workspace_id: None,
        };
        assert!(bedrock.validate().is_err(), "Bedrock {region}");
        let vertex = ModelProvider::Vertex {
            project_id: Some("p".into()),
            location: VertexLocation::Region(region.into()),
            endpoint_url: None,
        };
        assert!(vertex.validate().is_err(), "Vertex {region}");
    }
    // real regions are accepted
    for region in ["us-east-1", "europe-west4", "us-gov-west-1"] {
        let bedrock = ModelProvider::Bedrock {
            region: region.into(),
            endpoint_kind: BedrockEndpointKind::Runtime,
            endpoint_url: None,
            workspace_id: Some("ws-1".into()),
        };
        assert!(bedrock.validate().is_ok(), "Bedrock {region}");
        let vertex = ModelProvider::Vertex {
            project_id: Some("my-project-1".into()),
            location: VertexLocation::Region(region.into()),
            endpoint_url: None,
        };
        assert!(vertex.validate().is_ok(), "Vertex {region}");
    }
    // project and workspace ids that would leave their path segment are refused
    for id in ["evil.example/?", "a/b", "a?b", "a#b", "a b", "", ".."] {
        let vertex = ModelProvider::Vertex {
            project_id: Some(id.into()),
            location: VertexLocation::Global,
            endpoint_url: None,
        };
        assert!(vertex.validate().is_err(), "Vertex project {id:?}");
        let bedrock = ModelProvider::Bedrock {
            region: "us-east-1".into(),
            endpoint_kind: BedrockEndpointKind::Runtime,
            endpoint_url: None,
            workspace_id: Some(id.into()),
        };
        assert!(bedrock.validate().is_err(), "Bedrock workspace {id:?}");
        let anthropic = ModelProvider::Anthropic {
            base_url: None,
            version: "2023-06-01".into(),
            betas: BTreeSet::new(),
            workspace_id: Some(id.into()),
        };
        assert!(anthropic.validate().is_err(), "Anthropic workspace {id:?}");
    }
}

/// Each provider serves its row of protocols with its first protocol as the default, and only
/// the generic gateways may skip a credential
#[test]
fn protocol_table() {
    // build a Bedrock endpoint of each family
    let bedrock = |endpoint_kind| ModelProvider::Bedrock {
        region: "us-east-1".into(),
        endpoint_kind,
        endpoint_url: None,
        workspace_id: None,
    };
    let mantle = bedrock(BedrockEndpointKind::Mantle);
    let runtime = bedrock(BedrockEndpointKind::Runtime);
    let providers = every_provider();
    let (vertex, gemini) = (&providers[3], &providers[4]);
    // protocols outside a row are refused
    assert!(mantle.check_api(ModelApi::BedrockConverse).is_err());
    assert!(runtime.check_api(ModelApi::AnthropicMessages).is_err());
    assert!(vertex.check_api(ModelApi::GeminiInteractions).is_err());
    assert!(gemini.check_api(ModelApi::VertexRawPredict).is_err());
    // the default protocol is the first in each row
    let defaults = [
        (&providers[0], ModelApi::OpenAiChat),
        (&providers[1], ModelApi::AnthropicMessages),
        (&runtime, ModelApi::BedrockConverse),
        (&mantle, ModelApi::AnthropicMessages),
        (vertex, ModelApi::GeminiGenerateContent),
        (gemini, ModelApi::GeminiGenerateContent),
        (&providers[5], ModelApi::OpenAiChat),
        (&providers[6], ModelApi::OpenAiChat),
        (&providers[7], ModelApi::OpenAiChat),
    ];
    for (provider, api) in defaults {
        assert_eq!(provider.default_api(), api, "{provider:?}");
        assert!(provider.check_api(api).is_ok());
    }
    // only the generic gateways may skip a credential
    for provider in every_provider() {
        let keyless = matches!(
            provider.kind(),
            ModelProviderKind::LiteLlm | ModelProviderKind::OpenAiCompatible
        );
        assert_eq!(provider.allows_keyless(), keyless, "{provider:?}");
    }
}

/// Credential kinds follow the provider table
#[test]
fn credential_kinds_per_provider() {
    // build the providers by kind
    let providers = every_provider();
    let (openai, anthropic, bedrock, vertex, azure) = (
        &providers[0],
        &providers[1],
        &providers[2],
        &providers[3],
        &providers[5],
    );
    let kinds = every_kind();
    // refused pairs
    assert!(
        anthropic
            .check_credential(&CredentialKind::AwsBearerToken)
            .is_err()
    );
    assert!(bedrock.check_credential(&CredentialKind::ApiKey).is_err());
    assert!(openai.check_credential(&kinds[5]).is_err());
    // accepted pairs
    assert!(azure.check_credential(&kinds[5]).is_ok());
    assert!(vertex.check_credential(&kinds[4]).is_ok());
    for kind in &kinds[1..4] {
        assert!(bedrock.check_credential(kind).is_ok(), "{kind:?}");
    }
}

/// Auth levels sort by precedence and matchers default their narrowing fields
#[test]
fn auth_levels_and_matchers() {
    // the derived order is the precedence
    assert!(ModelAuthLevel::Harness < ModelAuthLevel::Workflow);
    assert!(ModelAuthLevel::Workflow < ModelAuthLevel::Group);
    assert!(ModelAuthLevel::Group < ModelAuthLevel::User);
    // a matcher with only a model id
    let matcher: ModelMatcher = serde_json::from_value(json!({"model_id": "gpt-5"})).unwrap();
    assert_eq!(matcher.provider, None);
    assert_eq!(matcher.api, None);
    assert!(matcher.api_versions.is_empty());
}

/// Disabled model ids round trip and default to empty when absent
#[test]
fn disabled_models_round_trip() {
    // the entity and its request keep their disabled ids
    let meta = endpoint();
    round_trip(&meta);
    let req = ModelEndpointEntityRequest::new(every_provider().remove(0)).disable("gpt-5-latest");
    let back: ModelEndpointEntityRequest =
        serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
    assert_eq!(back.disabled_models, req.disabled_models);
    // an endpoint stored without the field has none
    let mut value = serde_json::to_value(&meta).unwrap();
    value.as_object_mut().unwrap().remove("disabled_models");
    let back: ModelEndpointEntity = serde_json::from_value(value).unwrap();
    assert!(back.disabled_models.is_empty());
    let value = json!({"provider": {"Anthropic": {}}, "credential": null});
    let back: ModelEndpointEntityRequest = serde_json::from_value(value).unwrap();
    assert!(back.disabled_models.is_empty());
}

/// Parameters must fit the model's thinking capability and output limit
#[test]
fn parameters_fit_models() {
    // a thinking budget needs a model that thinks
    let thinking = ModelParameters {
        thinking_budget_tokens: Some(100),
        ..ModelParameters::default()
    };
    let caps = ModelCapabilities::default();
    assert!(thinking.check_fit(&ModelLimits::default(), &caps).is_err());
    let thinker = ModelCapabilities {
        thinking: true,
        ..ModelCapabilities::default()
    };
    assert!(
        thinking
            .check_fit(&ModelLimits::default(), &thinker)
            .is_ok()
    );
    // the output cap can't exceed a known model limit
    let capped = ModelParameters {
        max_output_tokens: Some(9000),
        ..ModelParameters::default()
    };
    let limits = ModelLimits {
        max_output_tokens: Some(8192),
        ..ModelLimits::default()
    };
    assert!(capped.check_fit(&limits, &caps).is_err());
    assert!(capped.check_fit(&ModelLimits::default(), &caps).is_ok());
    // float parameters round trip
    let floats = ModelParameters {
        temperature: Some(0.2),
        ..ModelParameters::default()
    };
    round_trip(&floats);
}

/// The strict client parse refuses parameters with a key that isn't a known parameter and names
/// the key
#[cfg(feature = "client")]
#[test]
fn strict_parameters_refuse_unknown_keys() {
    // a misspelled key is refused with the key in the error
    let err = ModelParameters::from_value_strict(json!({"temperature": 0.5, "max_tokens": 100}))
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("max_tokens"),
        "the error didn't name the key: {msg}"
    );
    // a value of the wrong type is refused too
    assert!(ModelParameters::from_value_strict(json!({"top_p": "high"})).is_err());
}

/// The strict client parse accepts every known parameter and reads it unchanged
#[cfg(feature = "client")]
#[test]
fn strict_parameters_accept_known_keys() {
    // every known key reads
    let known = ModelParameters::from_value_strict(json!({
        "temperature": 0.5,
        "top_p": 0.9,
        "max_output_tokens": 100,
        "thinking_budget_tokens": 64,
        "stop_sequences": ["x"],
    }))
    .unwrap();
    let expected = ModelParameters {
        temperature: Some(0.5),
        top_p: Some(0.9),
        max_output_tokens: Some(100),
        thinking_budget_tokens: Some(64),
        stop_sequences: vec!["x".to_owned()],
    };
    assert_eq!(known, expected);
    // the serialized form of any parameters reads back strictly
    let value = serde_json::to_value(&expected).unwrap();
    assert_eq!(ModelParameters::from_value_strict(value).unwrap(), expected);
    // an empty document reads as the defaults
    assert_eq!(
        ModelParameters::from_value_strict(json!({})).unwrap(),
        ModelParameters::default()
    );
}

/// Stored profiles and responses whose parameters hold a key this version doesn't know still
/// read, so adding a parameter never breaks older readers
#[test]
fn stored_parameters_tolerate_unknown_keys() {
    // store a profile and add a parameter a newer version wrote
    let stored = EntityMetadata::ModelProfile(profile());
    let mut raw = serde_json::to_value(&stored).unwrap();
    raw["ModelProfile"]["parameters"]["min_p"] = json!(0.1);
    // the stored profile reads with its known parameters
    let read: EntityMetadata = serde_json::from_value(raw).unwrap();
    let EntityMetadata::ModelProfile(read) = read else {
        panic!("the stored profile read as another kind");
    };
    assert_eq!(read.parameters, profile().parameters);
    // bare parameters with an unknown key read too
    let params = serde_json::from_str::<ModelParameters>(r#"{"top_p":0.5,"min_p":0.1}"#).unwrap();
    assert_eq!(params.top_p, Some(0.5));
}

/// Catalogs are sorted and refuse repeated or empty model ids
#[test]
fn catalogs() {
    // entries come back sorted and manual
    let catalog = build_catalog(vec![
        ModelCatalogEntryRequest::new("b"),
        ModelCatalogEntryRequest::new("a"),
    ])
    .unwrap();
    let ids = catalog
        .iter()
        .map(|entry| entry.model_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["a", "b"]);
    assert!(
        catalog
            .iter()
            .all(|entry| entry.source == CatalogEntrySource::Manual
                && entry.limits.source == LimitsSource::Manual)
    );
    // a repeated id is refused and named
    let err = build_catalog(vec![
        ModelCatalogEntryRequest::new("a"),
        ModelCatalogEntryRequest::new("a"),
    ])
    .unwrap_err();
    assert!(err.contains("`a`"), "{err}");
    // an empty id is refused
    assert!(build_catalog(vec![ModelCatalogEntryRequest::new(" ")]).is_err());
    // an upsert keeps the catalog sorted and replaces by id
    let mut meta = endpoint();
    meta.upsert_entry(ModelCatalogEntryRequest::new("a-model").into_entry());
    meta.upsert_entry(ModelCatalogEntryRequest::new("claude-sonnet-4-5").into_entry());
    let ids = meta
        .catalog
        .iter()
        .map(|entry| entry.model_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["a-model", "claude-sonnet-4-5"]);
    assert_eq!(
        meta.catalog_entry("claude-sonnet-4-5").unwrap().source,
        CatalogEntrySource::Manual
    );
}

/// Get the names of a list of form fields
///
/// # Arguments
///
/// * `fields` - The form fields
fn names(fields: &[(&'static str, String)]) -> BTreeSet<&'static str> {
    fields.iter().map(|(name, _)| *name).collect()
}

/// Requests and updates send exactly their prefixed form fields, and a credential's form carries
/// its secret while its `Debug` doesn't
#[test]
fn form_fields() {
    // an endpoint request
    let endpoint = ModelEndpointEntityRequest::new(every_provider().remove(0))
        .credential(Uuid::new_v4())
        .model(ModelCatalogEntryRequest::new("gpt-5"))
        .disable("gpt-5-latest");
    assert_eq!(
        names(&endpoint.form_fields().unwrap()),
        BTreeSet::from([
            "kind",
            "metadata[model_endpoint_provider]",
            "metadata[model_endpoint_credential]",
            "metadata[model_endpoint_models]",
            "metadata[model_endpoint_disabled_models][]",
        ])
    );
    // a profile request
    let profile = ModelProfileEntityRequest::new(Uuid::new_v4(), "gpt-5", ModelApi::OpenAiChat)
        .parameters(&ModelParameters::default())
        .unwrap()
        .limits(Some(1), Some(2))
        .unwrap();
    let mut profile = profile;
    profile.capabilities = Some(ModelCapabilities::default());
    let fields = profile.form_fields().unwrap();
    assert_eq!(
        names(&fields),
        BTreeSet::from([
            "kind",
            "metadata[model_profile_endpoint]",
            "metadata[model_profile_model_id]",
            "metadata[model_profile_api]",
            "metadata[model_profile_parameters]",
            "metadata[model_profile_limits]",
            "metadata[model_profile_capabilities]",
        ])
    );
    assert!(fields.contains(&("metadata[model_profile_api]", "OpenAiChat".to_owned())));
    assert_eq!(
        ModelApi::parse_name("OpenAiChat").unwrap(),
        ModelApi::OpenAiChat
    );
    assert!(ModelApi::parse_name("Nope").is_err());
    // a credential request carries its secret in the form but never in its debug output
    let mut credential = ModelCredentialEntityRequest::new(
        CredentialKind::AwsAccessKey {
            access_key_id: "AKIA".into(),
        },
        Some(SECRET.into()),
    );
    credential.session_token = Some("session-token-value".into());
    credential.expires = Some(Utc::now());
    let fields = credential.form_fields().unwrap();
    assert_eq!(
        names(&fields),
        BTreeSet::from([
            "kind",
            "metadata[model_credential_kind]",
            "metadata[model_credential_secret]",
            "metadata[model_credential_session_token]",
            "metadata[model_credential_expires]",
        ])
    );
    assert!(fields.contains(&("metadata[model_credential_secret]", SECRET.to_owned())));
    let debug = format!("{:?}", EntityMetadataRequest::ModelCredential(credential));
    assert!(
        !debug.contains(SECRET),
        "the request debug output printed the secret"
    );
    // each kind is sent by its name
    assert!(
        endpoint
            .form_fields()
            .unwrap()
            .contains(&("kind", "ModelEndpoint".to_owned()))
    );
}

/// Model updates send exactly their prefixed form fields
#[test]
fn update_form_field_names() {
    // an endpoint update with every field set
    let update = EntityMetadataUpdate::model_endpoint()
        .endpoint_provider(every_provider().remove(0))
        .endpoint_credential(Uuid::new_v4())
        .clear_endpoint_credential()
        .add_catalog_model(ModelCatalogEntryRequest::new("a"))
        .remove_catalog_model("b")
        .disable_model("c")
        .enable_model("d")
        .model_bans_update(ModelBanUpdate::default().add_ban(ModelBan::generic("x")))
        .catalog_bans("a", ModelBanUpdate::default().remove_ban(Uuid::new_v4()));
    assert_eq!(
        names(&update_form_fields(&update).unwrap()),
        BTreeSet::from([
            "metadata[model_endpoint_provider]",
            "metadata[model_endpoint_credential]",
            "metadata[clear_model_endpoint_credential]",
            "metadata[add_model_endpoint_models]",
            "metadata[remove_model_endpoint_models][]",
            "metadata[disable_model_endpoint_models][]",
            "metadata[enable_model_endpoint_models][]",
            "metadata[model_endpoint_bans]",
            "metadata[model_endpoint_model_bans]",
        ])
    );
    // a profile update with every field set
    let update = EntityMetadataUpdate::model_profile()
        .profile_endpoint(Uuid::new_v4())
        .profile_model("m")
        .profile_api(ModelApi::OpenAiChat)
        .profile_parameters(ModelParameters::default())
        .profile_limits(ModelLimits::default())
        .clear_profile_limits()
        .profile_capabilities(ModelCapabilities::default())
        .clear_profile_capabilities()
        .model_bans_update(ModelBanUpdate::default().add_ban(ModelBan::generic("x")));
    assert_eq!(
        names(&update_form_fields(&update).unwrap()),
        BTreeSet::from([
            "metadata[model_profile_endpoint]",
            "metadata[model_profile_model_id]",
            "metadata[model_profile_api]",
            "metadata[model_profile_parameters]",
            "metadata[model_profile_limits]",
            "metadata[clear_model_profile_limits]",
            "metadata[model_profile_capabilities]",
            "metadata[clear_model_profile_capabilities]",
            "metadata[model_profile_bans]",
        ])
    );
    // a credential update with every field set
    let update = EntityMetadataUpdate::model_credential()
        .credential_secret(SECRET)
        .credential_session_token("tok")
        .clear_credential_session_token()
        .credential_expires(Utc::now())
        .clear_credential_expires()
        .model_bans_update(ModelBanUpdate::default().add_ban(ModelBan::generic("x")));
    let fields = update_form_fields(&update).unwrap();
    assert_eq!(
        names(&fields),
        BTreeSet::from([
            "metadata[model_credential_secret]",
            "metadata[model_credential_session_token]",
            "metadata[clear_model_credential_session_token]",
            "metadata[model_credential_expires]",
            "metadata[clear_model_credential_expires]",
            "metadata[model_credential_bans]",
        ])
    );
    // an empty update sends nothing
    assert_eq!(
        update_form_fields(&EntityMetadataUpdate::model_endpoint()).unwrap(),
        Vec::new()
    );
}

/// The model kinds take no part in sigma scanning, roots, parents, or identification
#[test]
fn shared_kind_functions() {
    // build a request of each model kind
    let requests = [
        EntityMetadataRequest::ModelEndpoint(ModelEndpointEntityRequest::new(
            every_provider().remove(0),
        )),
        EntityMetadataRequest::ModelProfile(ModelProfileEntityRequest::new(
            Uuid::new_v4(),
            "m",
            ModelApi::OpenAiChat,
        )),
        EntityMetadataRequest::ModelCredential(ModelCredentialEntityRequest::new(
            CredentialKind::ApiKey,
            Some(SECRET.into()),
        )),
    ];
    for meta in requests {
        // model configuration isn't scanned by sigma rules
        assert_eq!(meta.to_sigma_scannable().unwrap(), None);
        assert!(matches!(
            IdentifyingEntityInfo::from(&meta),
            IdentifyingEntityInfo::Unidentifiable
        ));
        // model configuration has no roots or parents
        let req = crate::models::EntityRequest::new("model", meta.clone(), vec!["g"]);
        assert_eq!(req.root_kind(), None);
        assert!(req.parent_info().is_none());
        let kind = req.kind();
        assert_eq!(kind.to_sigma_applies_to(), None);
        assert_eq!(kind.root_kinds().len(), 0);
    }
    // stored model entities aren't identifiable
    for meta in [
        EntityMetadata::ModelEndpoint(endpoint()),
        EntityMetadata::ModelProfile(profile()),
        EntityMetadata::ModelCredential(credential()),
    ] {
        assert!(matches!(
            IdentifyingEntityInfo::from(&meta),
            IdentifyingEntityInfo::Unidentifiable
        ));
    }
    // the kinds' string forms are their names
    for (kind, name) in [
        (EntityKinds::ModelEndpoint, "ModelEndpoint"),
        (EntityKinds::ModelProfile, "ModelProfile"),
        (EntityKinds::ModelCredential, "ModelCredential"),
    ] {
        assert_eq!(kind.as_str(), name);
        assert_eq!(EntityKinds::from_str(name).unwrap(), kind);
    }
}

/// Every model entity type is importable through the models module
#[test]
fn reexports() {
    // name every model type through the models re-exports
    let _ = std::any::type_name::<crate::models::ModelProvider>();
    let _ = std::any::type_name::<crate::models::ModelProviderKind>();
    let _ = std::any::type_name::<crate::models::BedrockEndpointKind>();
    let _ = std::any::type_name::<crate::models::VertexLocation>();
    let _ = std::any::type_name::<crate::models::VertexMultiRegion>();
    let _ = std::any::type_name::<crate::models::GeminiApiVersion>();
    let _ = std::any::type_name::<crate::models::AzureApiMode>();
    let _ = std::any::type_name::<crate::models::ModelApi>();
    let _ = std::any::type_name::<crate::models::CredentialKind>();
    let _ = std::any::type_name::<crate::models::CredentialType>();
    let _ = std::any::type_name::<crate::models::AwsRoleSource>();
    let _ = std::any::type_name::<crate::models::AzureClientAuth>();
    let _ = std::any::type_name::<crate::models::ModelSecret>();
    let _ = std::any::type_name::<crate::models::CredentialSecrets>();
    let _ = std::any::type_name::<dyn crate::models::RedactSecrets>();
    let _ = std::any::type_name::<crate::models::ModelLimits>();
    let _ = std::any::type_name::<crate::models::LimitsSource>();
    let _ = std::any::type_name::<crate::models::ModelCapabilities>();
    let _ = std::any::type_name::<crate::models::ModelParameters>();
    let _ = std::any::type_name::<crate::models::ModelCatalogEntry>();
    let _ = std::any::type_name::<crate::models::CatalogEntrySource>();
    let _ = std::any::type_name::<crate::models::ModelRefreshStatus>();
    let _ = std::any::type_name::<crate::models::ModelRefreshState>();
    let _ = std::any::type_name::<crate::models::ModelBan>();
    let _ = std::any::type_name::<crate::models::ModelBanKind>();
    let _ = std::any::type_name::<crate::models::ModelBanUpdate>();
    let _ = std::any::type_name::<crate::models::ModelEndpointEntity>();
    let _ = std::any::type_name::<crate::models::ModelProfileEntity>();
    let _ = std::any::type_name::<crate::models::ModelCredentialEntity>();
    let _ = std::any::type_name::<crate::models::ModelCatalogEntryRequest>();
    let _ = std::any::type_name::<crate::models::ModelEndpointEntityRequest>();
    let _ = std::any::type_name::<crate::models::ModelProfileEntityRequest>();
    let _ = std::any::type_name::<crate::models::ModelCredentialEntityRequest>();
    let _ = std::any::type_name::<crate::models::ModelMatcher>();
    let _ = std::any::type_name::<crate::models::ModelRequirements>();
    let _ = std::any::type_name::<crate::models::ModelAuthLevel>();
    let _ = std::any::type_name::<crate::models::ModelAuthSource>();
    let _ = std::any::type_name::<crate::models::ModelConfigRef>();
}

/// The model metadata forms never print a secret in their `Debug` output
#[cfg(feature = "api")]
#[test]
fn metadata_forms_redact_secrets() {
    // fill both forms' secret fields
    let form = crate::models::EntityMetadataForm {
        model_credential_secret: Some(SECRET.into()),
        model_credential_session_token: Some("session-token-value".into()),
        ..crate::models::EntityMetadataForm::default()
    };
    let update = crate::models::EntityMetadataUpdateForm {
        model_credential_secret: Some(SECRET.into()),
        model_credential_session_token: Some("session-token-value".into()),
        ..crate::models::EntityMetadataUpdateForm::default()
    };
    let entity_form = crate::models::EntityForm {
        metadata: form,
        ..crate::models::EntityForm::default()
    };
    // neither form's debug output holds a secret
    for output in [format!("{entity_form:?}"), format!("{update:#?}")] {
        assert!(
            !output.contains(SECRET),
            "a debug output printed the secret"
        );
        assert!(
            !output.contains("session-token-value"),
            "a debug output printed the session token"
        );
    }
}

/// The ban kinds display their detail and the hand-made ban is generic
#[test]
fn ban_helpers() {
    // a generic ban carries its message
    let ban = ModelBan::generic("paused");
    assert_eq!(ban.ban_kind, ModelBanKind::Generic("paused".into()));
    assert_eq!(ban.ban_kind.to_string(), "Generic: paused");
    assert_eq!(ModelBanKind::ModelRemoved.to_string(), "ModelRemoved");
    // an update reports whether it changes anything
    assert!(ModelBanUpdate::default().is_empty());
    assert!(
        !ModelBanUpdate::default()
            .remove_ban(Uuid::new_v4())
            .is_empty()
    );
    let _: Value = serde_json::to_value(ModelBanUpdate::default()).unwrap();
}

/// Capabilities stored before the tool capabilities existed read them as off, and each one
/// round trips when set
#[test]
fn tool_capabilities_default_off() {
    // capabilities written with only the original flags
    let caps: ModelCapabilities = serde_json::from_value(json!({
        "tool_calling": true,
        "vision": false,
        "pdf_input": false,
        "structured_output": true,
        "thinking": false
    }))
    .unwrap();
    assert!(!caps.strict_tools && !caps.parallel_tool_calls && !caps.forced_tool_choice);
    // each tool-calling capability flag round trips
    for caps in [
        ModelCapabilities {
            strict_tools: true,
            ..ModelCapabilities::default()
        },
        ModelCapabilities {
            parallel_tool_calls: true,
            ..ModelCapabilities::default()
        },
        ModelCapabilities {
            forced_tool_choice: true,
            ..ModelCapabilities::default()
        },
    ] {
        round_trip(&caps);
    }
}

/// AWS credential values are written one per line into the harness's AWS config file, so a value
/// with a control character, which could start a new line such as `credential_process`, is refused
#[test]
fn aws_credential_values_stay_on_one_config_line() {
    // build an assumed role with the given fields
    let role = |role_arn: &str, external_id: Option<&str>, session_name: Option<&str>| {
        CredentialKind::AwsAssumeRole {
            role_arn: role_arn.into(),
            external_id: external_id.map(Into::into),
            session_name: session_name.map(Into::into),
            duration_seconds: None,
            source: AwsRoleSource::AccessKey(Uuid::new_v4()),
        }
    };
    let arn = "arn:aws:iam::000000000000:role/r";
    // values that would add or corrupt lines of the config file are refused in every field
    for bad in [
        "x\ncredential_process = /usr/bin/touch /tmp/injected",
        "x\r\ncredential_process = y",
        "x\ry",
        "x\u{0}y",
        "x\ty",
    ] {
        let access_key = CredentialKind::AwsAccessKey {
            access_key_id: bad.into(),
        };
        assert!(access_key.validate().is_err(), "access_key_id {bad:?}");
        assert!(
            role(bad, None, None).validate().is_err(),
            "role_arn {bad:?}"
        );
        assert!(
            role(arn, Some(bad), None).validate().is_err(),
            "external_id {bad:?}"
        );
        assert!(
            role(arn, None, Some(bad)).validate().is_err(),
            "session_name {bad:?}"
        );
    }
    // realistic values are accepted
    let access_key = CredentialKind::AwsAccessKey {
        access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
    };
    assert!(access_key.validate().is_ok());
    assert!(
        role(arn, Some("ext-id:1/2=3"), Some("thorium-run@team.1"))
            .validate()
            .is_ok()
    );
    assert!(role(arn, None, None).validate().is_ok());
}

/// An access key's secret and session token are written on lines of an assumed role's AWS config
/// file, so control characters are refused in them, while other kinds' secrets may span lines
#[test]
fn aws_access_key_secrets_stay_on_one_config_line() {
    // build secrets from a primary secret and a session token
    let secrets = |primary: &str, token: Option<&str>| CredentialSecrets {
        primary: Some(ModelSecret::from(primary)),
        session_token: token.map(ModelSecret::from),
        fingerprint: None,
    };
    let access_key = CredentialKind::AwsAccessKey {
        access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
    };
    let injected = "s\n[profile thorium]\ncredential_process = /usr/bin/touch /tmp/injected";
    // a line break in the secret or the session token is refused
    assert!(
        access_key
            .validate_secrets(&secrets(injected, None))
            .is_err()
    );
    assert!(
        access_key
            .validate_secrets(&secrets("s", Some(injected)))
            .is_err()
    );
    assert!(access_key.validate_secrets(&secrets("s\r", None)).is_err());
    // real looking secrets are accepted
    assert!(
        access_key
            .validate_secrets(&secrets(
                "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
                Some("IQoJb3JpZ2luX2VjEJr//////////wEaCXVzLWVhc3QtMSJHMEUCIQ+=")
            ))
            .is_ok()
    );
    // a GCP JSON file or a PEM certificate spans lines and is written to a file of its own
    let gcp = CredentialKind::GcpCredentialsJson {
        impersonate_service_account: None,
    };
    assert!(
        gcp.validate_secrets(&secrets("{\n  \"type\": \"service_account\"\n}", None))
            .is_ok()
    );
}
