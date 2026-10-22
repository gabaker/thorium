//! Tests for a user's own model settings and how they are redacted

use chrono::Utc;
use std::collections::{BTreeSet, HashMap, HashSet};
use uuid::Uuid;

use super::UserModelFacts;
use crate::models::{
    AiEndpoint, AiSettings, AiSettingsUpdate, AllowedEndpoint, AwsRoleSource, CredentialKind,
    ModelBan, ModelBanKind, ModelBanUpdate, ModelCatalogEntryRequest, ModelProvider,
    ModelProviderKind, ModelSecret, ModelsConf, ScrubbedUser, User, UserCredentialRef,
    UserModelEndpoint, UserModelSettings, UserModelSettingsUpdate, UserRole, UserSettings,
};
use crate::utils::ApiError;

/// The gateway url the deployment allows in these tests
const GATEWAY: &str = "https://gw.example/v1";

/// Build a gateway provider at a url
///
/// # Arguments
///
/// * `url` - The gateway's url
fn gateway(url: &str) -> ModelProvider {
    ModelProvider::LiteLlm {
        base_url: url.to_owned(),
        key_header: None,
    }
}

/// Build facts that allow the test gateway and Bedrock and know one visible group endpoint
///
/// # Arguments
///
/// * `group_endpoint` - The visible group endpoint
fn facts(group_endpoint: Uuid) -> UserModelFacts {
    // allow the gateway and Bedrock's default urls
    let allowed = ModelsConf {
        allowed_endpoints: BTreeSet::from([
            AllowedEndpoint::new(ModelProviderKind::LiteLlm, Some(GATEWAY.to_owned())),
            AllowedEndpoint::new(ModelProviderKind::Bedrock, None),
        ]),
    };
    // the group endpoint is a gateway too
    let group_endpoints = HashMap::from([(group_endpoint, gateway(GATEWAY))]);
    UserModelFacts {
        allowed,
        group_endpoints,
        profiles: HashSet::new(),
    }
}

/// Build settings with one own gateway endpoint that has a catalog model
///
/// # Arguments
///
/// * `endpoint` - The id of the own endpoint
fn settings_with_endpoint(endpoint: Uuid) -> UserModelSettings {
    // apply an update adding the endpoint to empty settings
    let mut settings = UserModelSettings::default();
    let own = UserModelEndpoint::new("own", gateway(GATEWAY))
        .model(ModelCatalogEntryRequest::new("model-a"));
    UserModelSettingsUpdate::default()
        .endpoint(endpoint, own)
        .apply(&mut settings, &facts(Uuid::new_v4()))
        .unwrap();
    settings
}

/// Get the message of a refusal
///
/// # Arguments
///
/// * `err` - The refusal
fn msg(err: &ApiError) -> String {
    // only Thorium errors carry a message
    match err {
        ApiError::Thorium { msg, .. } => msg.clone().unwrap_or_default(),
        _ => String::new(),
    }
}

/// Build a user holding some settings
///
/// # Arguments
///
/// * `settings` - The user's settings
fn user_with(settings: UserSettings) -> User {
    User {
        username: "corn".to_owned(),
        password: None,
        email: "corn@example.com".to_owned(),
        role: UserRole::User,
        groups: Vec::new(),
        actual_groups: Vec::new(),
        token: "token".to_owned(),
        token_expiration: Utc::now(),
        unix: None,
        settings,
        verified: true,
        verification_token: None,
        verification_sent: None,
        aliases: HashMap::new(),
        image: None,
    }
}

/// User model settings round-trip inside user settings, and missing fields load empty
#[test]
fn user_model_settings_round_trip() {
    // settings with an endpoint, a credential, and a default profile
    let endpoint = Uuid::new_v4();
    let mut models = settings_with_endpoint(endpoint);
    models.credentials.insert(
        endpoint,
        UserCredentialRef::new(CredentialKind::ApiKey, Some("sk-round".into())),
    );
    models.default_profiles.push(Uuid::new_v4());
    models
        .endpoints
        .get_mut(&endpoint)
        .unwrap()
        .disabled_models
        .insert("old".to_owned());
    let settings = UserSettings {
        ai: Some(AiSettings {
            models: models.clone(),
            ..AiSettings::default()
        }),
        ..UserSettings::default()
    };
    // they serialize and deserialize unchanged
    let raw = serde_json::to_string(&settings).unwrap();
    let back: UserSettings = serde_json::from_str(&raw).unwrap();
    assert_eq!(back, settings);
    // ai settings without models load with empty model settings
    let legacy = r#"{"theme":"Dark","ai":{"endpoints":{},"default_endpoint":""}}"#;
    let loaded: UserSettings = serde_json::from_str(legacy).unwrap();
    assert_eq!(loaded.ai.unwrap().models, UserModelSettings::default());
    // an own endpoint without disabled models loads with none
    let raw = r#"{"name":"e","provider":{"LiteLlm":{"base_url":"https://gw.example/v1","key_header":null}}}"#;
    let own: UserModelEndpoint = serde_json::from_str(raw).unwrap();
    assert!(own.disabled_models.is_empty());
    assert_eq!(own.catalog, Vec::new());
    assert!(own.bans.is_empty());
}

/// An AI settings update with only model settings creates AI settings with no legacy endpoints
#[test]
fn models_only_update_creates_ai_settings() {
    // a user with no AI settings gets an own endpoint
    let mut existing: Option<AiSettings> = None;
    let endpoint = Uuid::new_v4();
    let update = AiSettingsUpdate::default().models(
        UserModelSettingsUpdate::default()
            .endpoint(endpoint, UserModelEndpoint::new("own", gateway(GATEWAY))),
    );
    update.apply(&mut existing, &facts(Uuid::new_v4())).unwrap();
    let ai = existing.clone().expect("no ai settings were created");
    assert!(ai.endpoints.is_empty());
    assert_eq!(ai.default_endpoint, "");
    assert!(ai.models.endpoints.contains_key(&endpoint));
    // a legacy-only update without a default endpoint is still refused on empty settings
    let mut empty: Option<AiSettings> = None;
    let legacy = AiSettingsUpdate::default().remove_endpoint("missing");
    assert!(
        legacy
            .apply(&mut empty, &UserModelFacts::default())
            .is_err()
    );
    // removing the last model endpoint with no legacy endpoints left clears the AI settings
    let remove = AiSettingsUpdate::default()
        .models(UserModelSettingsUpdate::default().remove_endpoint(endpoint));
    remove
        .apply(&mut existing, &UserModelFacts::default())
        .unwrap();
    assert_eq!(existing, None);
}

/// Scrubbing a user clears every credential secret, keeps fingerprints, and blanks legacy keys,
/// and `Debug` never prints a secret
#[test]
fn scrubbed_user_redacts_model_secrets() {
    // a user with a credential secret, a session token, and a legacy key
    let endpoint = Uuid::new_v4();
    let mut models = settings_with_endpoint(endpoint);
    let mut cred = UserCredentialRef::new(
        CredentialKind::AwsAccessKey {
            access_key_id: "AKIA".to_owned(),
        },
        Some("primary-secret-value".into()),
    );
    cred.secrets.session_token = Some("session-secret-value".into());
    cred.secrets.fingerprint = Some(ModelSecret::from("primary-secret-value").fingerprint());
    models.credentials.insert(endpoint, cred);
    let legacy = AiEndpoint {
        url: "https://llm.local".to_owned(),
        api_key: "legacy-secret-value".to_owned(),
        model: "m".to_owned(),
    };
    let settings = UserSettings {
        ai: Some(AiSettings {
            endpoints: HashMap::from([("main".to_owned(), legacy)]),
            default_endpoint: "main".to_owned(),
            models,
        }),
        ..UserSettings::default()
    };
    let user = user_with(settings);
    // Debug output of the user never holds a model secret or the legacy key
    let debug = format!("{user:?}");
    for secret in [
        "primary-secret-value",
        "session-secret-value",
        "legacy-secret-value",
    ] {
        assert!(!debug.contains(secret), "Debug printed a secret");
    }
    // the scrubbed user holds fingerprints and empty legacy keys
    let scrubbed = ScrubbedUser::from(user);
    let ai = scrubbed.settings.ai.clone().unwrap();
    let cred = &ai.models.credentials[&endpoint];
    assert_eq!(cred.secrets.primary, None);
    assert_eq!(cred.secrets.session_token, None);
    assert_eq!(
        cred.secrets.fingerprint,
        Some(ModelSecret::from("primary-secret-value").fingerprint())
    );
    let legacy = &ai.endpoints["main"];
    assert_eq!(legacy.api_key, "");
    assert_eq!(legacy.url, "https://llm.local");
    assert_eq!(legacy.model, "m");
    // its JSON holds no secret
    let body = serde_json::to_string(&scrubbed).unwrap();
    for secret in [
        "primary-secret-value",
        "session-secret-value",
        "legacy-secret-value",
    ] {
        assert!(!body.contains(secret), "the scrubbed user held a secret");
    }
}

/// Own endpoints disable and enable model ids, and only own endpoints may
#[test]
fn user_disabled_models() {
    // settings with one own endpoint
    let endpoint = Uuid::new_v4();
    let mut settings = settings_with_endpoint(endpoint);
    let facts = facts(Uuid::new_v4());
    // disabling ids adds them and enabling one removes it
    UserModelSettingsUpdate::default()
        .disable_model(endpoint, "a")
        .disable_model(endpoint, "b")
        .apply(&mut settings, &facts)
        .unwrap();
    UserModelSettingsUpdate::default()
        .enable_model(endpoint, "a")
        .apply(&mut settings, &facts)
        .unwrap();
    assert_eq!(
        settings.endpoints[&endpoint].disabled_models,
        BTreeSet::from(["b".to_owned()])
    );
    // one id can't be disabled and enabled at once
    let both = UserModelSettingsUpdate::default()
        .disable_model(endpoint, "c")
        .enable_model(endpoint, "c")
        .apply(&mut settings, &facts);
    assert_eq!(both.unwrap_err().status().as_u16(), 400);
    // an endpoint that isn't the user's own can't disable models
    let other = UserModelSettingsUpdate::default()
        .disable_model(Uuid::new_v4(), "c")
        .apply(&mut settings, &facts);
    assert_eq!(other.unwrap_err().status().as_u16(), 400);
    // a refused update changed nothing
    assert_eq!(
        settings.endpoints[&endpoint].disabled_models,
        BTreeSet::from(["b".to_owned()])
    );
}

/// User credentials store their secret, keep it across updates without one, rotate with a new
/// one, and assumed roles must name another of the user's access keys
#[test]
// one scenario builds the credentials every later step checks
#[allow(clippy::too_many_lines)]
fn user_credentials() {
    // settings with a Bedrock endpoint of the user's own
    let endpoint = Uuid::new_v4();
    let second = Uuid::new_v4();
    let facts = facts(Uuid::new_v4());
    let bedrock = ModelProvider::Bedrock {
        region: "us-east-1".to_owned(),
        endpoint_kind: crate::models::BedrockEndpointKind::Runtime,
        endpoint_url: None,
        workspace_id: None,
    };
    let mut settings = UserModelSettings::default();
    UserModelSettingsUpdate::default()
        .endpoint(endpoint, UserModelEndpoint::new("bedrock", bedrock.clone()))
        .endpoint(second, UserModelEndpoint::new("bedrock-2", bedrock))
        .apply(&mut settings, &facts)
        .unwrap();
    // adding a credential stores its secret, fingerprint, and rotation time
    let access = UserCredentialRef::new(
        CredentialKind::AwsAccessKey {
            access_key_id: "AKIA".to_owned(),
        },
        Some("first-secret".into()),
    );
    UserModelSettingsUpdate::default()
        .credential(endpoint, access.clone())
        .apply(&mut settings, &facts)
        .unwrap();
    let stored = settings.credentials[&endpoint].clone();
    assert!(
        stored.secrets.primary.as_ref().map(ModelSecret::expose) == Some("first-secret"),
        "the secret wasn't stored"
    );
    assert_eq!(
        stored.secrets.fingerprint,
        Some(ModelSecret::from("first-secret").fingerprint())
    );
    assert!(stored.rotated.is_some());
    // an update without a secret keeps the stored one
    let mut keep = access.clone();
    keep.secrets.primary = None;
    UserModelSettingsUpdate::default()
        .credential(endpoint, keep)
        .apply(&mut settings, &facts)
        .unwrap();
    assert_eq!(settings.credentials[&endpoint].secrets, stored.secrets);
    assert_eq!(settings.credentials[&endpoint].rotated, stored.rotated);
    // a new secret rotates it and clears rejection and expiry bans
    let rejected = ModelBan::new(ModelBanKind::CredentialRejected {
        status: 401,
        message: "no".to_owned(),
    });
    let expired = ModelBan::new(ModelBanKind::CredentialExpired);
    let generic = ModelBan::generic("kept");
    let bans = &mut settings.credentials.get_mut(&endpoint).unwrap().bans;
    for ban in [&rejected, &expired, &generic] {
        bans.insert(ban.id, ban.clone());
    }
    let mut rotate = access.clone();
    rotate.secrets.primary = Some("second-secret".into());
    UserModelSettingsUpdate::default()
        .credential(endpoint, rotate)
        .apply(&mut settings, &facts)
        .unwrap();
    let rotated = &settings.credentials[&endpoint];
    assert_eq!(
        rotated.secrets.fingerprint,
        Some(ModelSecret::from("second-secret").fingerprint())
    );
    assert!(rotated.rotated >= stored.rotated);
    assert_eq!(rotated.bans.keys().collect::<Vec<_>>(), vec![&generic.id]);
    // an assumed role naming the user's access key is accepted
    let role = |source| {
        UserCredentialRef::new(
            CredentialKind::AwsAssumeRole {
                role_arn: "arn:aws:iam::1:role/r".to_owned(),
                external_id: None,
                session_name: None,
                duration_seconds: None,
                source: AwsRoleSource::AccessKey(source),
            },
            None,
        )
    };
    UserModelSettingsUpdate::default()
        .credential(second, role(access.id))
        .apply(&mut settings, &facts)
        .unwrap();
    // one naming a missing credential or itself is refused
    let missing = UserModelSettingsUpdate::default()
        .credential(second, role(Uuid::new_v4()))
        .apply(&mut settings, &facts);
    assert_eq!(missing.unwrap_err().status().as_u16(), 400);
    let own_role = settings.credentials[&second].id;
    let looped = UserModelSettingsUpdate::default()
        .credential(second, role(own_role))
        .apply(&mut settings, &facts);
    assert_eq!(looped.unwrap_err().status().as_u16(), 400);
    // a kind that needs a secret is refused without one, and an empty secret counts as none
    for secret in [None, Some(ModelSecret::from(""))] {
        let empty = UserModelSettingsUpdate::default()
            .credential(
                second,
                UserCredentialRef::new(CredentialKind::AwsBearerToken, secret),
            )
            .apply(&mut settings, &facts);
        assert_eq!(empty.unwrap_err().status().as_u16(), 400);
    }
    // a credential for an endpoint that isn't own or a visible group endpoint is refused
    let unknown = UserModelSettingsUpdate::default()
        .credential(
            Uuid::new_v4(),
            UserCredentialRef::new(CredentialKind::ApiKey, Some("x".into())),
        )
        .apply(&mut settings, &facts);
    assert_eq!(unknown.unwrap_err().status().as_u16(), 400);
}

/// Ban updates keyed by endpoint id add Generic bans and remove bans found on the endpoint, its
/// catalog, or its credential, with ids unique across all three
#[test]
fn user_ban_updates() {
    // settings with an own endpoint, a catalog model, and a credential, each already banned
    let endpoint = Uuid::new_v4();
    let mut settings = settings_with_endpoint(endpoint);
    let facts = facts(Uuid::new_v4());
    UserModelSettingsUpdate::default()
        .credential(
            endpoint,
            UserCredentialRef::new(CredentialKind::ApiKey, Some("sk-bans".into())),
        )
        .apply(&mut settings, &facts)
        .unwrap();
    let model_ban = ModelBan::generic("model");
    let cred_ban = ModelBan::generic("credential");
    settings.endpoints.get_mut(&endpoint).unwrap().catalog[0]
        .bans
        .insert(model_ban.id, model_ban.clone());
    settings
        .credentials
        .get_mut(&endpoint)
        .unwrap()
        .bans
        .insert(cred_ban.id, cred_ban.clone());
    // one update adds a ban to the endpoint and removes the catalog and credential bans
    let endpoint_ban = ModelBan::generic("endpoint");
    UserModelSettingsUpdate::default()
        .bans(
            endpoint,
            ModelBanUpdate::default()
                .add_ban(endpoint_ban.clone())
                .remove_ban(model_ban.id)
                .remove_ban(cred_ban.id),
        )
        .apply(&mut settings, &facts)
        .unwrap();
    assert_eq!(
        settings.endpoints[&endpoint]
            .bans
            .keys()
            .collect::<Vec<_>>(),
        vec![&endpoint_ban.id]
    );
    assert!(settings.endpoints[&endpoint].catalog[0].bans.is_empty());
    assert!(settings.credentials[&endpoint].bans.is_empty());
    // the ban index sees every ban once
    let index = settings.ban_index();
    assert_eq!(index.len(), 1);
    assert!(index.contains_key(&endpoint_ban.id));
    // a ban id already used on the endpoint can't be added to its credential
    let reused = ModelBan {
        id: endpoint_ban.id,
        ..ModelBan::generic("again")
    };
    let dup = UserModelSettingsUpdate::default()
        .credential_bans(endpoint, ModelBanUpdate::default().add_ban(reused))
        .apply(&mut settings, &facts);
    assert_eq!(dup.unwrap_err().status().as_u16(), 400);
    // only Generic bans can be added
    let kind = UserModelSettingsUpdate::default()
        .bans(
            endpoint,
            ModelBanUpdate::default().add_ban(ModelBan::new(ModelBanKind::NotAllowedByDeployment)),
        )
        .apply(&mut settings, &facts);
    let err = kind.unwrap_err();
    assert_eq!(err.status().as_u16(), 400);
    assert!(
        msg(&err).contains("NotAllowedByDeployment"),
        "the refusal didn't name the ban kind"
    );
    // removing a ban that doesn't exist is a 404
    let missing = UserModelSettingsUpdate::default()
        .bans(
            endpoint,
            ModelBanUpdate::default().remove_ban(Uuid::new_v4()),
        )
        .apply(&mut settings, &facts);
    assert_eq!(missing.unwrap_err().status().as_u16(), 404);
    // a catalog ban is added by model id
    let added = ModelBan::generic("model again");
    UserModelSettingsUpdate::default()
        .model_bans(
            endpoint,
            "model-a",
            ModelBanUpdate::default().add_ban(added.clone()),
        )
        .apply(&mut settings, &facts)
        .unwrap();
    assert!(
        settings.endpoints[&endpoint].catalog[0]
            .bans
            .contains_key(&added.id)
    );
}

/// Own endpoints must be on the deployment's list, and an endpoint's removal takes its credential
#[test]
fn user_endpoint_checks() {
    // an endpoint outside the list is refused naming it
    let mut settings = UserModelSettings::default();
    let facts = facts(Uuid::new_v4());
    let outside = UserModelSettingsUpdate::default()
        .endpoint(
            Uuid::new_v4(),
            UserModelEndpoint::new("outside", gateway("https://other.example/v1")),
        )
        .apply(&mut settings, &facts);
    let err = outside.unwrap_err();
    assert_eq!(err.status().as_u16(), 400);
    assert!(msg(&err).contains("outside"));
    assert_eq!(settings, UserModelSettings::default());
    // removing an endpoint removes its credential in the same update
    let endpoint = Uuid::new_v4();
    let mut settings = settings_with_endpoint(endpoint);
    UserModelSettingsUpdate::default()
        .credential(
            endpoint,
            UserCredentialRef::new(CredentialKind::ApiKey, Some("sk-x".into())),
        )
        .apply(&mut settings, &facts)
        .unwrap();
    UserModelSettingsUpdate::default()
        .remove_endpoint(endpoint)
        .apply(&mut settings, &facts)
        .unwrap();
    assert!(settings.endpoints.is_empty());
    assert!(settings.credentials.is_empty());
}

/// Replacing an own endpoint with another provider re-checks the credential it keeps
#[test]
fn provider_change_checks_credential() {
    // an own gateway endpoint with an API key
    let endpoint = Uuid::new_v4();
    let facts = facts(Uuid::new_v4());
    let mut settings = settings_with_endpoint(endpoint);
    UserModelSettingsUpdate::default()
        .credential(
            endpoint,
            UserCredentialRef::new(CredentialKind::ApiKey, Some("sk-kept".into())),
        )
        .apply(&mut settings, &facts)
        .unwrap();
    let before = settings.clone();
    // a Bedrock endpoint can't use the API key it would keep, so the change is refused
    let bedrock = ModelProvider::Bedrock {
        region: "us-east-1".to_owned(),
        endpoint_kind: crate::models::BedrockEndpointKind::Runtime,
        endpoint_url: None,
        workspace_id: None,
    };
    let changed = UserModelSettingsUpdate::default()
        .endpoint(endpoint, UserModelEndpoint::new("own", bedrock.clone()))
        .apply(&mut settings, &facts);
    assert_eq!(changed.unwrap_err().status().as_u16(), 400);
    assert_eq!(settings, before);
    // replacing the credential in the same update is accepted
    let access = UserCredentialRef::new(
        CredentialKind::AwsAccessKey {
            access_key_id: "AKIA".to_owned(),
        },
        Some("aws-secret".into()),
    );
    UserModelSettingsUpdate::default()
        .endpoint(endpoint, UserModelEndpoint::new("own", bedrock))
        .credential(endpoint, access)
        .apply(&mut settings, &facts)
        .unwrap();
}

/// A ban id is unique across all of a user's endpoints, since bans are indexed by id alone
#[test]
fn ban_ids_unique_across_endpoints() {
    // two own endpoints
    let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
    let facts = facts(Uuid::new_v4());
    let mut settings = settings_with_endpoint(first);
    // add the second one next to the first
    UserModelSettingsUpdate::default()
        .endpoint(second, UserModelEndpoint::new("second", gateway(GATEWAY)))
        .apply(&mut settings, &facts)
        .unwrap();
    // a ban on the first endpoint
    let ban = ModelBan::generic("x");
    UserModelSettingsUpdate::default()
        .bans(first, ModelBanUpdate::default().add_ban(ban.clone()))
        .apply(&mut settings, &facts)
        .unwrap();
    // a ban on the second endpoint reusing that ban's id
    let before = settings.clone();
    let reused = ModelBan {
        id: ban.id,
        ..ModelBan::generic("y")
    };
    let dup = UserModelSettingsUpdate::default()
        .bans(second, ModelBanUpdate::default().add_ban(reused))
        .apply(&mut settings, &facts);
    // is refused and changes nothing
    assert_eq!(dup.unwrap_err().status().as_u16(), 400);
    assert_eq!(settings, before);
    assert_eq!(settings.ban_index().len(), 1);
}
