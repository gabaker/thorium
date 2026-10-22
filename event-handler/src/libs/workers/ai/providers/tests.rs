//! Tests for the provider listings, their parsers, and their sign-in

use chrono::{TimeZone, Utc};
use reqwest::Url;
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;
use thorium::models::{
    AwsRoleSource, AzureApiMode, AzureClientAuth, BedrockEndpointKind, CredentialKind,
    CredentialSecrets, GeminiApiVersion, ListedModel, ModelBanKind, ModelProvider, ModelSecret,
    VertexLocation,
};
use thorium::test_utilities::mock_provider::{MockProvider, MockReply};

use super::auth::{self, AwsKeys, AwsScope};
use super::{
    AuthHosts, Lister, ListingCredential, ListingError, ListingLimits, azure_url, classify,
    gemini_url, listing_client, parse_anthropic, parse_gemini, parse_ids, parse_litellm,
    parse_ollama_show, parse_ollama_tags, parse_openai_compatible, parse_vertex,
};

/// The test-only RSA key the sign-in tests sign with
const TEST_KEY: &str = include_str!("../../../../../tests/fixtures/test-only-rsa-key.pem");

/// The test-only certificate for [`TEST_KEY`]
const TEST_CERT: &str = include_str!("../../../../../tests/fixtures/test-only-rsa-cert.pem");

/// The SHA-1 thumbprint of [`TEST_CERT`], base64url encoded
const TEST_CERT_X5T: &str = "GlW1jBSRsJBQtu37wze5kFF9wus";

/// Parse a JSON fixture
///
/// # Arguments
///
/// * `text` - The fixture
fn doc(text: &str) -> Value {
    serde_json::from_str(text).expect("the fixture is JSON")
}

/// Build a credential with a primary secret
///
/// # Arguments
///
/// * `kind` - The kind of credential
/// * `secret` - The primary secret
fn credential(kind: CredentialKind, secret: &str) -> ListingCredential {
    ListingCredential {
        kind,
        secrets: CredentialSecrets {
            primary: Some(ModelSecret::from(secret)),
            session_token: None,
            fingerprint: None,
        },
        source: None,
    }
}

/// Decode an unpadded base64url string
///
/// # Arguments
///
/// * `text` - The encoded text
fn unbase64url(text: &str) -> Vec<u8> {
    // map each character back to its 6 bits
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut bits = 0u32;
    let mut count = 0;
    let mut out = Vec::new();
    for c in text.bytes() {
        let value = alphabet
            .iter()
            .position(|a| *a == c)
            .expect("base64url character");
        bits = (bits << 6) | u32::try_from(value).unwrap();
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push(u8::try_from((bits >> count) & 0xff).unwrap());
        }
    }
    out
}

/// A credentials file whose token url isn't the pinned Google token endpoint is refused before
/// any request, so a file can never make the worker post to another host
#[tokio::test]
async fn gcp_token_uri_is_pinned() {
    // an internal host a credentials file names as its token url
    let internal = MockProvider::start().await.unwrap();
    internal.reply(
        "POST",
        "/token",
        MockReply::status(400, r#"{"error": "INTERNAL-ONLY-DATA"}"#),
    );
    let client = listing_client().unwrap();
    let hosts = AuthHosts::default();
    let lister = Lister::new(&client, &hosts);
    let provider = ModelProvider::Vertex {
        project_id: None,
        location: VertexLocation::Region("us-east5".to_owned()),
        endpoint_url: Some(internal.url()),
    };
    let file = service_account(&format!("{}/token", internal.url()));
    let sa = credential(
        CredentialKind::GcpCredentialsJson {
            impersonate_service_account: None,
        },
        &file,
    );
    // the file is refused naming its token url, and the internal host saw nothing
    let error = lister.list(&provider, Some(&sa)).await.unwrap_err();
    match error {
        ListingError::Rejected { status, message } => {
            assert_eq!(status, 0);
            assert!(message.contains("token_uri"), "{message}");
            assert!(!message.contains("INTERNAL-ONLY-DATA"), "{message}");
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
    assert!(internal.requests().is_empty());
    // the pinned url is Google's token endpoint
    assert_eq!(hosts.gcp_token, "https://oauth2.googleapis.com/token");
    // a file naming the pinned url, or none, signs its assertion for that url and exchanges there
    let pinned = MockProvider::start().await.unwrap();
    pinned.reply(
        "POST",
        "/token",
        MockReply::json(r#"{"access_token": "sa-token"}"#),
    );
    for publisher in ["google", "anthropic"] {
        pinned.reply(
            "GET",
            &format!("/v1beta1/publishers/{publisher}/models"),
            MockReply::json(r#"{"publisherModels": []}"#),
        );
    }
    let token_url = format!("{}/token", pinned.url());
    let hosts = AuthHosts {
        gcp_token: token_url.clone(),
        ..AuthHosts::default()
    };
    let lister = Lister::new(&client, &hosts);
    let provider = ModelProvider::Vertex {
        project_id: None,
        location: VertexLocation::Region("us-east5".to_owned()),
        endpoint_url: Some(pinned.url()),
    };
    let mut unnamed = serde_json::from_str::<Value>(&service_account(&token_url)).unwrap();
    unnamed.as_object_mut().unwrap().remove("token_uri");
    for file in [service_account(&token_url), unnamed.to_string()] {
        pinned.clear_requests();
        let sa = credential(
            CredentialKind::GcpCredentialsJson {
                impersonate_service_account: None,
            },
            &file,
        );
        lister.list(&provider, Some(&sa)).await.unwrap();
        let exchange = &pinned.requests_to("/token")[0];
        let assertion = exchange
            .body
            .split('&')
            .find_map(|pair| pair.strip_prefix("assertion="))
            .unwrap();
        let payload = assertion.split('.').nth(1).unwrap();
        let claims: Value = serde_json::from_slice(&unbase64url(payload)).unwrap();
        assert_eq!(claims["aud"], json!(token_url));
    }
}

/// Tokens a listing's sign-in gets never appear in its failure messages, and secrets are removed
/// before a long message is shortened
#[tokio::test]
// one scenario walks each kind of derived token against one mock
#[allow(clippy::too_many_lines)]
async fn derived_tokens_never_shown() {
    // sign-in endpoints whose tokens the listings echo back
    let mock = MockProvider::start().await.unwrap();
    mock.reply(
        "POST",
        "/sts/",
        MockReply::json(
            r#"{"AssumeRoleResponse": {"AssumeRoleResult": {"Credentials": {
                "AccessKeyId": "ASIAECHO", "SecretAccessKey": "derived-session-secret",
                "SessionToken": "derived-session-token"}}}}"#,
        ),
    );
    mock.reply(
        "GET",
        "/foundation-models",
        MockReply::status(
            403,
            r#"{"message": "denied derived-session-secret derived-session-token"}"#,
        ),
    );
    mock.reply(
        "POST",
        "/tenant-1/oauth2/v2.0/token",
        MockReply::json(r#"{"access_token": "derived-entra-token"}"#),
    );
    mock.reply(
        "GET",
        "/openai/v1/models",
        MockReply::status(401, r#"{"error": {"message": "bad derived-entra-token"}}"#),
    );
    let straddle = format!(
        r#"{{"error": {{"message": "{}gw-straddle-secret"}}}}"#,
        "x".repeat(505)
    );
    mock.reply("GET", "/long/models", MockReply::status(401, straddle));
    let client = listing_client().unwrap();
    let hosts = AuthHosts {
        sts: Some(format!("{}/sts/", mock.url())),
        entra: mock.url(),
        iam_credentials: mock.url(),
        gcp_token: format!("{}/token", mock.url()),
    };
    let lister = Lister::new(&client, &hosts);
    // a Google token and an impersonated account's token are kept out of the message
    mock.reply(
        "POST",
        "/token",
        MockReply::json(r#"{"access_token": "derived-google-token"}"#),
    );
    mock.reply(
        "POST",
        "/v1/projects/-/serviceAccounts/target%40thorium-test.iam.gserviceaccount.com:generateAccessToken",
        MockReply::json(r#"{"accessToken": "derived-impersonated-token"}"#),
    );
    let vertex = ModelProvider::Vertex {
        project_id: None,
        location: VertexLocation::Region("us-east5".to_owned()),
        endpoint_url: Some(mock.url()),
    };
    let file = service_account(&format!("{}/token", mock.url()));
    for (impersonate, token) in [
        (None, "derived-google-token"),
        (
            Some("target@thorium-test.iam.gserviceaccount.com".to_owned()),
            "derived-impersonated-token",
        ),
    ] {
        mock.reply(
            "GET",
            "/v1beta1/publishers/google/models",
            MockReply::status(403, format!(r#"{{"error": {{"message": "bad {token}"}}}}"#)),
        );
        let sa = credential(
            CredentialKind::GcpCredentialsJson {
                impersonate_service_account: impersonate,
            },
            &file,
        );
        let shown = format!("{:?}", lister.list(&vertex, Some(&sa)).await.unwrap_err());
        assert!(!shown.contains(token), "{shown}");
        assert!(shown.contains("<redacted>"), "{shown}");
    }
    // an assumed role's session keys are kept out of the message
    let bedrock = ModelProvider::Bedrock {
        region: "us-west-2".to_owned(),
        endpoint_kind: BedrockEndpointKind::Runtime,
        endpoint_url: Some(mock.url()),
        workspace_id: None,
    };
    let role = ListingCredential {
        kind: CredentialKind::AwsAssumeRole {
            role_arn: "arn:aws:iam::123456789012:role/lister".to_owned(),
            external_id: None,
            session_name: None,
            duration_seconds: None,
            source: AwsRoleSource::AccessKey(uuid::Uuid::new_v4()),
        },
        secrets: CredentialSecrets::default(),
        source: Some(Box::new(credential(
            CredentialKind::AwsAccessKey {
                access_key_id: "AKIATEST".to_owned(),
            },
            "source-secret",
        ))),
    };
    let shown = format!(
        "{:?}",
        lister.list(&bedrock, Some(&role)).await.unwrap_err()
    );
    assert!(!shown.contains("derived-session-secret"), "{shown}");
    assert!(!shown.contains("derived-session-token"), "{shown}");
    assert!(shown.contains("<redacted>"), "{shown}");
    // an Entra token is kept out of the message
    let azure = ModelProvider::AzureOpenAi {
        endpoint: mock.url(),
        api_mode: AzureApiMode::V1,
        entra_scope: None,
    };
    let entra = credential(
        CredentialKind::AzureEntraClient {
            tenant_id: "tenant-1".to_owned(),
            client_id: "client-1".to_owned(),
            auth: AzureClientAuth::ClientSecret,
        },
        "entra-secret",
    );
    let shown = format!("{:?}", lister.list(&azure, Some(&entra)).await.unwrap_err());
    assert!(!shown.contains("derived-entra-token"), "{shown}");
    // a key that straddles the message limit leaves no part of itself behind
    let gateway = ModelProvider::OpenAiCompatible {
        base_url: format!("{}/long", mock.url()),
        extra_headers: BTreeMap::new(),
        key_header: None,
    };
    let key = credential(CredentialKind::ApiKey, "gw-straddle-secret");
    let shown = format!("{:?}", lister.list(&gateway, Some(&key)).await.unwrap_err());
    assert!(!shown.contains("gw-str"), "{shown}");
}

/// A listing still paging at its deadline is an unreachable endpoint, and the default bounds
/// are two minutes and a hundred requests
#[tokio::test]
async fn listing_deadline() {
    // an Anthropic listing whose every page takes a second and names a next page
    let mock = MockProvider::start().await.unwrap();
    mock.reply(
        "GET",
        "/v1/models",
        MockReply::json(
            r#"{"data": [{"id": "claude-x"}], "has_more": true, "last_id": "claude-x"}"#,
        )
        .delayed(Duration::from_secs(1)),
    );
    let client = listing_client().unwrap();
    let hosts = AuthHosts::default();
    let lister = Lister::new(&client, &hosts).limits(ListingLimits {
        deadline: Duration::from_millis(2_500),
        ..ListingLimits::default()
    });
    let provider = ModelProvider::Anthropic {
        base_url: Some(mock.url()),
        version: "2023-06-01".to_owned(),
        betas: BTreeSet::new(),
        workspace_id: None,
    };
    let key = credential(CredentialKind::ApiKey, "sk-ant-slow");
    // the listing gives up at its deadline
    let started = std::time::Instant::now();
    let error = lister.list(&provider, Some(&key)).await.unwrap_err();
    let took = started.elapsed();
    assert!(
        matches!(&error, ListingError::Unreachable(message) if message.contains("longer than")),
        "{error:?}"
    );
    assert!(took < Duration::from_secs(4), "took {took:?}");
    assert!(mock.requests().len() <= 3);
    // the default bounds
    assert_eq!(ListingLimits::default().deadline, Duration::from_secs(120));
    assert_eq!(ListingLimits::default().requests, 100);
}

/// An Ollama server with more models than the request budget describes lists every model, and
/// the ones past the budget are listed without a description
#[tokio::test]
async fn ollama_lists_past_request_budget() {
    // an Ollama server with 150 models
    let mock = MockProvider::start().await.unwrap();
    let ids = (0..150)
        .map(|index| format!(r#"{{"id": "m{index}", "owned_by": "library"}}"#))
        .collect::<Vec<String>>()
        .join(", ");
    mock.reply(
        "GET",
        "/v1/models",
        MockReply::json(format!(r#"{{"data": [{ids}]}}"#)),
    );
    let tags = (0..150)
        .map(|index| format!(r#"{{"name": "m{index}"}}"#))
        .collect::<Vec<String>>()
        .join(", ");
    mock.reply(
        "GET",
        "/api/tags",
        MockReply::json(format!(r#"{{"models": [{tags}]}}"#)),
    );
    mock.reply(
        "POST",
        "/api/show",
        MockReply::json(
            r#"{"model_info": {"general.architecture": "llama", "llama.context_length": 8192}}"#,
        ),
    );
    let client = listing_client().unwrap();
    let hosts = AuthHosts::default();
    let lister = Lister::new(&client, &hosts);
    let provider = ModelProvider::OpenAiCompatible {
        base_url: format!("{}/v1", mock.url()),
        extra_headers: BTreeMap::new(),
        key_header: None,
    };
    // every model is listed within the budget, and the described ones carry their context
    let models = lister.list(&provider, None).await.unwrap();
    assert_eq!(models.len(), 150);
    assert_eq!(mock.requests().len(), 100);
    let described = models
        .iter()
        .filter(|model| model.context_window == Some(8192))
        .count();
    assert_eq!(described, 98);
}

/// One listing sends at most its request budget, and a listing that needs more is an
/// unreachable endpoint
#[tokio::test]
async fn listing_request_budget() {
    // an Anthropic listing whose every page names a next page
    let mock = MockProvider::start().await.unwrap();
    mock.reply(
        "GET",
        "/v1/models",
        MockReply::json(
            r#"{"data": [{"id": "claude-x"}], "has_more": true, "last_id": "claude-x"}"#,
        ),
    );
    let client = listing_client().unwrap();
    let hosts = AuthHosts::default();
    let lister = Lister::new(&client, &hosts).limits(ListingLimits {
        requests: 10,
        ..ListingLimits::default()
    });
    let provider = ModelProvider::Anthropic {
        base_url: Some(mock.url()),
        version: "2023-06-01".to_owned(),
        betas: BTreeSet::new(),
        workspace_id: None,
    };
    let key = credential(CredentialKind::ApiKey, "sk-ant-pages");
    // the listing stops at a budget of ten requests and fails
    let error = lister.list(&provider, Some(&key)).await.unwrap_err();
    assert!(
        matches!(&error, ListingError::Unreachable(message) if message.contains("requests")),
        "{error:?}"
    );
    assert_eq!(mock.requests().len(), 10);
}

/// A listing of more models than a report holds is an unreachable endpoint
#[tokio::test]
async fn listing_model_cap() {
    // a gateway that lists one model too many
    let mock = MockProvider::start().await.unwrap();
    let ids = (0..=10_000)
        .map(|index| format!(r#"{{"id": "m{index}"}}"#))
        .collect::<Vec<String>>()
        .join(", ");
    mock.reply(
        "GET",
        "/v1/models",
        MockReply::json(format!(r#"{{"data": [{ids}]}}"#)),
    );
    let client = listing_client().unwrap();
    let hosts = AuthHosts::default();
    let lister = Lister::new(&client, &hosts);
    let provider = ModelProvider::OpenAiCompatible {
        base_url: format!("{}/v1", mock.url()),
        extra_headers: BTreeMap::new(),
        key_header: None,
    };
    // the listing is refused rather than reported
    let error = lister.list(&provider, None).await.unwrap_err();
    assert!(
        matches!(&error, ListingError::Unreachable(message) if message.contains("models")),
        "{error:?}"
    );
}

/// Base64url encoding matches known vectors
#[test]
fn base64url_vectors() {
    // short and padded inputs drop their padding, and the url alphabet is used
    assert_eq!(auth::base64url(b""), "");
    assert_eq!(auth::base64url(b"f"), "Zg");
    assert_eq!(auth::base64url(b"fo"), "Zm8");
    assert_eq!(auth::base64url(b"foo"), "Zm9v");
    assert_eq!(auth::base64url(b"hello"), "aGVsbG8");
    assert_eq!(auth::base64url(&[0xfb, 0xff]), "-_8");
    assert_eq!(unbase64url("aGVsbG8"), b"hello");
}

/// Request signing matches the AWS Signature Version 4 test suite, with and without a session
/// token
#[test]
fn sigv4_matches_aws_vectors() {
    // the AWS test suite's keys, region, service, and time
    let keys = AwsKeys {
        access_key_id: "AKIDEXAMPLE".to_owned(),
        secret: ModelSecret::from("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"),
        session_token: None,
    };
    let scope = AwsScope {
        region: "us-east-1",
        service: "service",
        time: Utc.with_ymd_and_hms(2015, 8, 30, 12, 36, 0).unwrap(),
    };
    // get-vanilla-query-order-key-case sorts its query
    let url = Url::parse("https://example.amazonaws.com/?Param2=value2&Param1=value1").unwrap();
    let headers = auth::sigv4_headers("GET", &url, &[], b"", &keys, scope).unwrap();
    let authorization = headers
        .iter()
        .find(|(name, _)| name == "authorization")
        .map(|(_, value)| value.clone())
        .unwrap();
    assert_eq!(
        authorization,
        "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500"
    );
    assert!(headers.contains(&("x-amz-date".to_owned(), "20150830T123600Z".to_owned())));
    // get-vanilla-with-session-token signs and sends the token
    let token = "6e86291e8372ff2a2260956d9b8aae1d763fbf315fa00fa31553b73ebf194267";
    let with_token = AwsKeys {
        session_token: Some(ModelSecret::from(token)),
        ..keys
    };
    let url = Url::parse("https://example.amazonaws.com/").unwrap();
    let headers = auth::sigv4_headers("GET", &url, &[], b"", &with_token, scope).unwrap();
    assert!(headers.contains(&("x-amz-security-token".to_owned(), token.to_owned())));
    assert!(headers.iter().any(|(name, value)| name == "authorization"
        && value.ends_with("SignedHeaders=host;x-amz-date;x-amz-security-token, Signature=07ec1639c89043aa0e3e2de82b96708f198cceab042d4a97044c66dd9f74e7f8")));
}

/// An RS256 token verifies with the key's public half, and a certificate's thumbprint is its
/// SHA-1
#[test]
fn rs256_tokens_and_thumbprints() {
    // sign a token with the test key
    let key = auth::rsa_key(TEST_KEY).unwrap();
    let claims = json!({ "iss": "thorium", "exp": 10 });
    let token = auth::rs256_jwt(&[("kid", "key-1")], &claims, &key).unwrap();
    // the header and claims read back
    let parts = token.split('.').collect::<Vec<&str>>();
    assert_eq!(parts.len(), 3);
    let header = serde_json::from_slice::<Value>(&unbase64url(parts[0])).unwrap();
    assert_eq!(header.get("alg").and_then(Value::as_str), Some("RS256"));
    assert_eq!(header.get("kid").and_then(Value::as_str), Some("key-1"));
    let read = serde_json::from_slice::<Value>(&unbase64url(parts[1])).unwrap();
    assert_eq!(read, claims);
    // the signature verifies over the header and claims
    let public = UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, key.public().as_ref());
    let message = format!("{}.{}", parts[0], parts[1]);
    assert!(
        public
            .verify(message.as_bytes(), &unbase64url(parts[2]))
            .is_ok()
    );
    // a bundle of a key and a certificate reads both
    let bundle = format!("{TEST_KEY}\n{TEST_CERT}");
    assert!(auth::rsa_key(&bundle).is_ok());
    assert_eq!(
        auth::certificate_thumbprint(&bundle).unwrap(),
        TEST_CERT_X5T
    );
    // a document without a key or certificate is refused
    assert!(auth::rsa_key(TEST_CERT).is_err());
    assert!(auth::certificate_thumbprint(TEST_KEY).is_err());
}

/// Each provider's listing parser yields the ids and limits its listing reports
#[test]
#[allow(clippy::too_many_lines)]
fn listing_parsers() {
    // Anthropic reports input and output limits and capabilities, and pages by last id
    let page = parse_anthropic(&doc(
        r#"{"data": [{"id": "claude-a", "max_input_tokens": 200000, "max_tokens": 64000,
            "capabilities": {"image_input": {"supported": true}, "thinking": true}}],
            "has_more": true, "last_id": "claude-a"}"#,
    ))
    .unwrap();
    assert_eq!(page.next.as_deref(), Some("claude-a"));
    assert_eq!(page.models[0].context_window, Some(200_000));
    assert_eq!(page.models[0].max_output_tokens, Some(64_000));
    let caps = page.models[0].capabilities.clone().unwrap();
    assert!(caps.vision && caps.thinking && !caps.tool_calling);
    let last =
        parse_anthropic(&doc(r#"{"data": [{"id": "claude-b"}], "has_more": false}"#)).unwrap();
    assert_eq!(last.next, None);
    assert_eq!(last.models, vec![ListedModel::new("claude-b")]);
    // Gemini drops the models/ prefix and reports both limits
    let page = parse_gemini(&doc(
        r#"{"models": [{"name": "models/gemini-x", "inputTokenLimit": 1048576, "outputTokenLimit": 8192}],
            "nextPageToken": "t2"}"#,
    ))
    .unwrap();
    assert_eq!(page.models[0].model_id, "gemini-x");
    assert_eq!(page.models[0].context_window, Some(1_048_576));
    assert_eq!(page.next.as_deref(), Some("t2"));
    // LiteLLM prefers the input and output limits and falls back to max_tokens only without both
    let models = parse_litellm(&doc(
        r#"{"data": [
            {"model_name": "both", "model_info": {"max_input_tokens": 100, "max_output_tokens": 10, "max_tokens": 5}},
            {"model_name": "legacy", "model_info": {"max_tokens": 7}},
            {"model_name": "input", "model_info": {"max_input_tokens": 50, "max_tokens": 7}}]}"#,
    ))
    .unwrap();
    assert_eq!(
        (models[0].context_window, models[0].max_output_tokens),
        (Some(100), Some(10))
    );
    assert_eq!(
        (models[1].context_window, models[1].max_output_tokens),
        (None, Some(7))
    );
    assert_eq!(
        (models[2].context_window, models[2].max_output_tokens),
        (Some(50), None)
    );
    // OpenAI-compatible servers report limits under their own keys
    let (models, ollama) = parse_openai_compatible(&doc(
        r#"{"data": [
            {"id": "router", "context_length": 128000, "top_provider": {"max_completion_tokens": 4096}},
            {"id": "groq", "context_window": 131072, "max_completion_tokens": 32768},
            {"id": "vllm", "max_model_len": 32768},
            {"id": "plain"}]}"#,
    ))
    .unwrap();
    assert!(!ollama);
    let limits = models
        .iter()
        .map(|model| (model.context_window, model.max_output_tokens))
        .collect::<Vec<_>>();
    assert_eq!(
        limits,
        vec![
            (Some(128_000), Some(4_096)),
            (Some(131_072), Some(32_768)),
            (Some(32_768), None),
            (None, None)
        ]
    );
    // Ollama marks its models and reports each context through api/show
    let (_, ollama) = parse_openai_compatible(&doc(
        r#"{"data": [{"id": "llama3:8b", "owned_by": "library"}]}"#,
    ))
    .unwrap();
    assert!(ollama);
    assert_eq!(
        parse_ollama_tags(&doc(r#"{"models": [{"name": "llama3:8b"}]}"#)).unwrap(),
        vec!["llama3:8b".to_owned()]
    );
    assert_eq!(
        parse_ollama_show(&doc(
            r#"{"model_info": {"general.architecture": "llama", "llama.context_length": 8192}}"#
        )),
        Some(8192)
    );
    // providers that list only ids report no limits
    let openai = parse_ids(&doc(r#"{"data": [{"id": "gpt-x"}]}"#), "data", "id", None).unwrap();
    assert_eq!(openai.models, vec![ListedModel::new("gpt-x")]);
    let bedrock = parse_ids(
        &doc(r#"{"modelSummaries": [{"modelId": "anthropic.claude"}], "nextToken": "n"}"#),
        "modelSummaries",
        "modelId",
        Some("nextToken"),
    )
    .unwrap();
    assert_eq!(bedrock.models, vec![ListedModel::new("anthropic.claude")]);
    assert_eq!(bedrock.next.as_deref(), Some("n"));
    let vertex = parse_vertex(&doc(
        r#"{"publisherModels": [{"name": "publishers/google/models/gemini-2.0"}]}"#,
    ))
    .unwrap();
    assert_eq!(vertex.models, vec![ListedModel::new("gemini-2.0")]);
    // a listing with zero models is zero models, and a body of the wrong shape is an error
    assert_eq!(
        parse_ids(&doc(r#"{"data": []}"#), "data", "id", None)
            .unwrap()
            .models
            .len(),
        0
    );
    assert!(parse_ids(&doc(r#"{"foo": 1}"#), "data", "id", None).is_err());
    assert!(parse_anthropic(&doc(r#"{"data": {"id": "x"}}"#)).is_err());
    assert!(parse_litellm(&doc("[1, 2]")).is_err());
    assert_eq!(parse_gemini(&doc("{}")).unwrap().models.len(), 0);
}

/// Gemini puts its API version in the path, and only Azure's legacy API sends an api-version
#[test]
fn listing_urls() {
    // each Gemini version is its own path segment
    assert_eq!(
        gemini_url(None, GeminiApiVersion::V1),
        "https://generativelanguage.googleapis.com/v1/models"
    );
    assert_eq!(
        gemini_url(Some("http://gw/"), GeminiApiVersion::V1Beta),
        "http://gw/v1beta/models"
    );
    // Azure's v1 API takes no api-version and its legacy API does
    assert_eq!(
        azure_url("https://res.openai.azure.com", &AzureApiMode::V1),
        "https://res.openai.azure.com/openai/v1/models"
    );
    assert_eq!(
        azure_url(
            "https://res.openai.azure.com/",
            &AzureApiMode::Legacy {
                api_version: "2024-10-21".to_owned()
            }
        ),
        "https://res.openai.azure.com/openai/models?api-version=2024-10-21"
    );
}

/// Statuses map to rejected, expired, unreachable, or retried listings
#[test]
fn listing_failures() {
    // a refused credential keeps the status and message
    assert_eq!(
        classify(401, r#"{"error": {"message": "invalid x-api-key"}}"#),
        ListingError::Rejected {
            status: 401,
            message: "invalid x-api-key".to_owned()
        }
    );
    // an expired key is its own failure
    assert!(matches!(
        classify(
            403,
            r#"{"error": {"message": "The security token included in the request is expired"}}"#
        ),
        ListingError::Expired(_)
    ));
    // server errors and other statuses mean the endpoint can't list, and throttling is retried
    assert!(matches!(
        classify(503, "down"),
        ListingError::Unreachable(_)
    ));
    assert!(matches!(
        classify(404, "missing"),
        ListingError::Unreachable(_)
    ));
    assert!(matches!(classify(429, "slow down"), ListingError::Retry(_)));
    // each error becomes the failure its report records, and a retry records nothing
    assert_eq!(
        ListingError::Expired("old".to_owned()).failure(),
        Some(ModelBanKind::CredentialExpired)
    );
    assert_eq!(
        ListingError::Unreachable("refused".to_owned()).failure(),
        Some(ModelBanKind::EndpointUnreachable("refused".to_owned()))
    );
    assert_eq!(ListingError::Retry("later".to_owned()).failure(), None);
}

/// Build a Google service account credentials file whose token url is a mock
///
/// # Arguments
///
/// * `token_uri` - Where tokens are exchanged
fn service_account(token_uri: &str) -> String {
    json!({
        "type": "service_account",
        "client_email": "lister@thorium-test.iam.gserviceaccount.com",
        "private_key_id": "key-1",
        "private_key": TEST_KEY,
        "token_uri": token_uri,
    })
    .to_string()
}

/// Bedrock listings are signed with access keys, with session tokens, and with assumed roles'
/// session keys, and bearer tokens are sent as is
#[tokio::test]
async fn bedrock_sign_in() {
    // a mock Bedrock control plane and STS
    let mock = MockProvider::start().await.unwrap();
    mock.reply(
        "GET",
        "/foundation-models",
        MockReply::json(r#"{"modelSummaries": [{"modelId": "anthropic.claude"}]}"#),
    );
    mock.reply(
        "GET",
        "/inference-profiles",
        MockReply::json(
            r#"{"inferenceProfileSummaries": [{"inferenceProfileId": "us.anthropic.claude"}]}"#,
        ),
    );
    mock.reply(
        "POST",
        "/sts/",
        MockReply::json(
            r#"{"AssumeRoleResponse": {"AssumeRoleResult": {"Credentials": {
                "AccessKeyId": "ASIASESSION", "SecretAccessKey": "session-secret",
                "SessionToken": "session-token", "Expiration": 1.9e9}}}}"#,
        ),
    );
    let client = listing_client().unwrap();
    let hosts = AuthHosts {
        sts: Some(format!("{}/sts/", mock.url())),
        ..AuthHosts::default()
    };
    let lister = Lister::new(&client, &hosts);
    let provider = ModelProvider::Bedrock {
        region: "us-west-2".to_owned(),
        endpoint_kind: BedrockEndpointKind::Runtime,
        endpoint_url: Some(mock.url()),
        workspace_id: None,
    };
    // an access key with a session token signs both listings
    let mut keys = credential(
        CredentialKind::AwsAccessKey {
            access_key_id: "AKIATEST".to_owned(),
        },
        "secret-key",
    );
    keys.secrets.session_token = Some(ModelSecret::from("key-session"));
    let models = lister.list(&provider, Some(&keys)).await.unwrap();
    assert_eq!(
        models,
        vec![
            ListedModel::new("anthropic.claude"),
            ListedModel::new("us.anthropic.claude")
        ]
    );
    for request in mock.requests() {
        let authorization = &request.headers["authorization"];
        assert!(authorization.starts_with("AWS4-HMAC-SHA256 Credential=AKIATEST/"));
        assert!(authorization.contains("/us-west-2/bedrock/aws4_request"));
        assert_eq!(request.headers["x-amz-security-token"], "key-session");
    }
    // an assumed role calls STS with its source key as JSON and lists with the session keys
    mock.clear_requests();
    keys.secrets.session_token = None;
    let role = ListingCredential {
        kind: CredentialKind::AwsAssumeRole {
            role_arn: "arn:aws:iam::123456789012:role/lister".to_owned(),
            external_id: Some("ext-1".to_owned()),
            session_name: None,
            duration_seconds: Some(900),
            source: AwsRoleSource::AccessKey(uuid::Uuid::new_v4()),
        },
        secrets: CredentialSecrets::default(),
        source: Some(Box::new(keys.clone())),
    };
    lister.list(&provider, Some(&role)).await.unwrap();
    let sts = mock.requests_to("/sts/");
    assert_eq!(sts.len(), 1);
    assert_eq!(sts[0].headers["accept"], "application/json");
    assert!(sts[0].headers["authorization"].contains("Credential=AKIATEST/"));
    assert!(sts[0].headers["authorization"].contains("/us-west-2/sts/aws4_request"));
    let query = sts[0].query.clone().unwrap();
    assert!(query.contains("Action=AssumeRole") && query.contains("ExternalId=ext-1"));
    let listing = mock.requests_to("/foundation-models");
    assert!(listing[0].headers["authorization"].contains("Credential=ASIASESSION/"));
    assert_eq!(listing[0].headers["x-amz-security-token"], "session-token");
    // a bearer token is sent as is
    mock.clear_requests();
    let bearer = credential(CredentialKind::AwsBearerToken, "ABSKtoken");
    lister.list(&provider, Some(&bearer)).await.unwrap();
    assert_eq!(
        mock.requests_to("/foundation-models")[0].headers["authorization"],
        "Bearer ABSKtoken"
    );
    // a refused AssumeRole rejects the credential
    mock.reply(
        "POST",
        "/sts/",
        MockReply::status(403, r#"{"Error": {"Message": "not authorized"}}"#),
    );
    let error = lister.list(&provider, Some(&role)).await.unwrap_err();
    assert!(matches!(error, ListingError::Rejected { .. }), "{error:?}");
}

/// Vertex listings exchange a service account for a token, impersonate when asked, and refuse
/// external account files, and a refused exchange rejects the credential
#[tokio::test]
async fn vertex_sign_in() {
    // a mock token endpoint, IAM Credentials, and Vertex AI
    let mock = MockProvider::start().await.unwrap();
    mock.reply(
        "POST",
        "/token",
        MockReply::json(r#"{"access_token": "sa-token"}"#),
    );
    mock.reply(
        "POST",
        "/v1/projects/-/serviceAccounts/target%40thorium-test.iam.gserviceaccount.com:generateAccessToken",
        MockReply::json(r#"{"accessToken": "impersonated-token"}"#),
    );
    for publisher in ["google", "anthropic"] {
        mock.reply(
            "GET",
            &format!("/v1beta1/publishers/{publisher}/models"),
            MockReply::json(format!(
                r#"{{"publisherModels": [{{"name": "publishers/{publisher}/models/{publisher}-model"}}]}}"#
            )),
        );
    }
    let client = listing_client().unwrap();
    let hosts = AuthHosts {
        iam_credentials: mock.url(),
        gcp_token: format!("{}/token", mock.url()),
        ..AuthHosts::default()
    };
    let lister = Lister::new(&client, &hosts);
    let provider = ModelProvider::Vertex {
        project_id: Some("thorium-test".to_owned()),
        location: VertexLocation::Region("us-east5".to_owned()),
        endpoint_url: Some(mock.url()),
    };
    // a service account's signed assertion is exchanged and the token lists
    let file = service_account(&format!("{}/token", mock.url()));
    let sa = credential(
        CredentialKind::GcpCredentialsJson {
            impersonate_service_account: None,
        },
        &file,
    );
    let models = lister.list(&provider, Some(&sa)).await.unwrap();
    assert_eq!(
        models,
        vec![
            ListedModel::new("google-model"),
            ListedModel::new("anthropic-model")
        ]
    );
    let exchange = &mock.requests_to("/token")[0];
    assert!(
        exchange
            .body
            .contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer")
    );
    assert!(exchange.body.contains("assertion="));
    let listing = &mock.requests_to("/v1beta1/publishers/google/models")[0];
    assert_eq!(listing.headers["authorization"], "Bearer sa-token");
    assert_eq!(listing.headers["x-goog-user-project"], "thorium-test");
    // impersonation trades that token for the target account's
    mock.clear_requests();
    let impersonating = credential(
        CredentialKind::GcpCredentialsJson {
            impersonate_service_account: Some(
                "target@thorium-test.iam.gserviceaccount.com".to_owned(),
            ),
        },
        &file,
    );
    lister.list(&provider, Some(&impersonating)).await.unwrap();
    let listing = &mock.requests_to("/v1beta1/publishers/google/models")[0];
    assert_eq!(
        listing.headers["authorization"],
        "Bearer impersonated-token"
    );
    // an external account file is refused before any request
    mock.clear_requests();
    let external = credential(
        CredentialKind::GcpCredentialsJson {
            impersonate_service_account: None,
        },
        r#"{"type": "external_account", "audience": "x"}"#,
    );
    let error = lister.list(&provider, Some(&external)).await.unwrap_err();
    match error {
        ListingError::Rejected { status, message } => {
            assert_eq!(status, 0);
            assert!(message.contains("external_account"));
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
    assert!(mock.requests().is_empty());
    // a refused exchange rejects the credential and never shows the key
    mock.reply(
        "POST",
        "/token",
        MockReply::status(400, r#"{"error": "invalid_grant"}"#),
    );
    let error = lister.list(&provider, Some(&sa)).await.unwrap_err();
    assert!(
        matches!(error, ListingError::Rejected { status: 400, .. }),
        "{error:?}"
    );
    assert!(!format!("{error:?}").contains("PRIVATE KEY"));
}

/// Azure listings get an Entra token with a client secret or a certificate assertion, or send an
/// API key, and Gemini, Anthropic, and gateways send their keys where they belong
#[tokio::test]
// one scenario walks every provider's key placement against one mock
#[allow(clippy::too_many_lines)]
async fn key_placement_and_entra() {
    // a mock Entra ID and model listings
    let mock = MockProvider::start().await.unwrap();
    mock.reply(
        "POST",
        "/tenant-1/oauth2/v2.0/token",
        MockReply::json(r#"{"access_token": "entra-token"}"#),
    );
    mock.reply(
        "GET",
        "/openai/v1/models",
        MockReply::json(r#"{"data": [{"id": "gpt-x"}]}"#),
    );
    mock.reply(
        "GET",
        "/openai/models",
        MockReply::json(r#"{"data": [{"id": "gpt-y"}]}"#),
    );
    mock.reply(
        "GET",
        "/v1beta/models",
        MockReply::json(r#"{"models": []}"#),
    );
    mock.reply(
        "GET",
        "/v1/models",
        MockReply::json(
            r#"{"data": [{"id": "claude-a"}], "has_more": true, "last_id": "claude-a"}"#,
        ),
    );
    mock.reply(
        "GET",
        "/v1/models?limit=1000&after_id=claude-a",
        MockReply::json(r#"{"data": [{"id": "claude-b"}], "has_more": false}"#),
    );
    mock.reply(
        "GET",
        "/gw/models",
        MockReply::json(r#"{"data": [{"id": "local"}]}"#),
    );
    let client = listing_client().unwrap();
    let hosts = AuthHosts {
        entra: mock.url(),
        ..AuthHosts::default()
    };
    let lister = Lister::new(&client, &hosts);
    // a client secret is exchanged for a bearer token with the v1 default scope
    let azure = ModelProvider::AzureOpenAi {
        endpoint: mock.url(),
        api_mode: AzureApiMode::V1,
        entra_scope: None,
    };
    let entra = |auth: AzureClientAuth, secret: &str| {
        credential(
            CredentialKind::AzureEntraClient {
                tenant_id: "tenant-1".to_owned(),
                client_id: "client-1".to_owned(),
                auth,
            },
            secret,
        )
    };
    let models = lister
        .list(
            &azure,
            Some(&entra(AzureClientAuth::ClientSecret, "client-secret")),
        )
        .await
        .unwrap();
    assert_eq!(models, vec![ListedModel::new("gpt-x")]);
    let token = &mock.requests_to("/tenant-1/oauth2/v2.0/token")[0];
    assert!(token.body.contains("client_secret=client-secret"));
    assert!(
        token
            .body
            .contains("scope=https%3A%2F%2Fai.azure.com%2F.default")
    );
    assert_eq!(
        mock.requests_to("/openai/v1/models")[0].headers["authorization"],
        "Bearer entra-token"
    );
    // a certificate signs a client assertion naming its thumbprint
    mock.clear_requests();
    let bundle = format!("{TEST_KEY}\n{TEST_CERT}");
    lister
        .list(&azure, Some(&entra(AzureClientAuth::Certificate, &bundle)))
        .await
        .unwrap();
    let token = &mock.requests_to("/tenant-1/oauth2/v2.0/token")[0];
    assert!(!token.body.contains("client_secret"));
    let assertion = token
        .body
        .split('&')
        .find_map(|pair| pair.strip_prefix("client_assertion="))
        .unwrap();
    let header =
        serde_json::from_slice::<Value>(&unbase64url(assertion.split('.').next().unwrap()))
            .unwrap();
    assert_eq!(
        header.get("x5t").and_then(Value::as_str),
        Some(TEST_CERT_X5T)
    );
    // an API key goes in the api-key header, and the legacy API sends its version
    mock.clear_requests();
    let legacy = ModelProvider::AzureOpenAi {
        endpoint: mock.url(),
        api_mode: AzureApiMode::Legacy {
            api_version: "2024-10-21".to_owned(),
        },
        entra_scope: None,
    };
    lister
        .list(
            &legacy,
            Some(&credential(CredentialKind::ApiKey, "azure-key")),
        )
        .await
        .unwrap();
    let listing = &mock.requests_to("/openai/models")[0];
    assert_eq!(listing.headers["api-key"], "azure-key");
    assert_eq!(listing.query.as_deref(), Some("api-version=2024-10-21"));
    // a refused token request rejects the credential
    mock.reply(
        "POST",
        "/tenant-1/oauth2/v2.0/token",
        MockReply::status(
            401,
            r#"{"error": "invalid_client", "error_description": "bad secret"}"#,
        ),
    );
    let error = lister
        .list(
            &azure,
            Some(&entra(AzureClientAuth::ClientSecret, "client-secret")),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, ListingError::Rejected { status: 401, .. }),
        "{error:?}"
    );
    // Gemini sends x-goog-api-key in its version's path
    mock.clear_requests();
    let gemini = ModelProvider::Gemini {
        base_url: Some(mock.url()),
        api_version: GeminiApiVersion::V1Beta,
    };
    lister
        .list(
            &gemini,
            Some(&credential(CredentialKind::ApiKey, "gemini-key")),
        )
        .await
        .unwrap();
    assert_eq!(
        mock.requests_to("/v1beta/models")[0].headers["x-goog-api-key"],
        "gemini-key"
    );
    // Anthropic sends x-api-key and its version, and reads both pages
    let anthropic = ModelProvider::Anthropic {
        base_url: Some(mock.url()),
        version: "2023-06-01".to_owned(),
        betas: BTreeSet::new(),
        workspace_id: None,
    };
    let models = lister
        .list(
            &anthropic,
            Some(&credential(CredentialKind::ApiKey, "sk-ant")),
        )
        .await
        .unwrap();
    assert_eq!(
        models,
        vec![ListedModel::new("claude-a"), ListedModel::new("claude-b")]
    );
    let first = &mock.requests_to("/v1/models")[0];
    assert_eq!(first.headers["x-api-key"], "sk-ant");
    assert_eq!(first.headers["anthropic-version"], "2023-06-01");
    // a gateway sends its key in its own header, with its extra headers, or no key at all
    mock.clear_requests();
    let mut headers = BTreeMap::new();
    headers.insert("x-request-source".to_owned(), "thorium".to_owned());
    let gateway = ModelProvider::OpenAiCompatible {
        base_url: format!("{}/gw", mock.url()),
        extra_headers: headers,
        key_header: Some("x-gateway-key".to_owned()),
    };
    lister
        .list(
            &gateway,
            Some(&credential(CredentialKind::ApiKey, "gw-key")),
        )
        .await
        .unwrap();
    lister.list(&gateway, None).await.unwrap();
    let requests = mock.requests_to("/gw/models");
    assert_eq!(requests[0].headers["x-gateway-key"], "gw-key");
    assert_eq!(requests[0].headers["x-request-source"], "thorium");
    assert!(!requests[1].headers.contains_key("x-gateway-key"));
    assert!(!requests[1].headers.contains_key("authorization"));
}

/// Unreachable endpoints, server errors, and bodies that aren't listings fail without ever
/// echoing a secret
#[tokio::test]
async fn unreachable_listings() {
    // a server that errors, answers with the key in its body, or answers with garbage
    let mock = MockProvider::start().await.unwrap();
    mock.reply("GET", "/down/models", MockReply::status(500, "boom"));
    mock.reply(
        "GET",
        "/echo/models",
        MockReply::status(401, r#"{"error": {"message": "bad key gw-secret"}}"#),
    );
    mock.reply("GET", "/garbage/models", MockReply::status(200, "not json"));
    mock.reply("GET", "/shape/models", MockReply::json(r#"{"models": 3}"#));
    let deep = format!(
        r#"{{"data": [], "extra": {}{}}}"#,
        "[".repeat(1000),
        "]".repeat(1000)
    );
    mock.reply("GET", "/deep/models", MockReply::json(deep));
    let client = listing_client().unwrap();
    let hosts = AuthHosts::default();
    let lister = Lister::new(&client, &hosts);
    let key = credential(CredentialKind::ApiKey, "gw-secret");
    let gateway = |path: &str| ModelProvider::OpenAiCompatible {
        base_url: format!("{}/{path}", mock.url()),
        extra_headers: BTreeMap::new(),
        key_header: None,
    };
    // a server error, bodies that aren't listings, and bodies nested too deeply to read are
    // unreachable endpoints
    for path in ["down", "garbage", "shape", "deep"] {
        let error = lister.list(&gateway(path), Some(&key)).await.unwrap_err();
        assert!(
            matches!(error, ListingError::Unreachable(_)),
            "{path}: {error:?}"
        );
    }
    // a provider message holding the key never shows it
    let error = lister.list(&gateway("echo"), Some(&key)).await.unwrap_err();
    assert_eq!(
        error,
        ListingError::Rejected {
            status: 401,
            message: "bad key <redacted>".to_owned()
        }
    );
    // a refused connection is an unreachable endpoint
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let provider = ModelProvider::OpenAiCompatible {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        extra_headers: BTreeMap::new(),
        key_header: None,
    };
    let error = lister.list(&provider, None).await.unwrap_err();
    assert!(matches!(error, ListingError::Unreachable(_)), "{error:?}");
}
