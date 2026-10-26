//! Tests for the model endpoint and profile documents

use chrono::Utc;
use serde_json::json;
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use thorium::Thorium;
use thorium::models::{
    CatalogEntrySource, Entity, EntityKinds, EntityMetadata, EntityMetadataUpdate, EntityUpdate,
    LimitsSource, ModelApi, ModelCapabilities, ModelCatalogEntry, ModelEndpointEntity, ModelLimits,
    ModelParameters, ModelProfileEntity, ModelProvider, ModelRefreshStatus,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

use super::{
    CatalogRow, Endpoint, EndpointData, EndpointView, Profile, ProfileData, ProfileView,
    ProviderDoc,
};
use crate::handlers::edit::Editable;
use crate::utils;
use crate::utils::patch::{merge_patch, parse_set, parse_unset};

/// The id of the credential the test endpoint uses
const CREDENTIAL: &str = "11111111-1111-1111-1111-111111111111";

/// The id of the endpoint a test profile calls
const ENDPOINT: &str = "22222222-2222-2222-2222-222222222222";

/// A catalog entry added by hand
///
/// # Arguments
///
/// * `model_id` - The model's id
/// * `window` - The model's context window
fn entry(model_id: &str, window: u64) -> ModelCatalogEntry {
    ModelCatalogEntry {
        model_id: model_id.to_owned(),
        limits: ModelLimits {
            context_window: Some(window),
            max_output_tokens: Some(1024),
            source: LimitsSource::Manual,
        },
        capabilities: ModelCapabilities {
            tool_calling: true,
            ..ModelCapabilities::default()
        },
        bans: HashMap::new(),
        source: CatalogEntrySource::Manual,
    }
}

/// An entity of any model kind
///
/// # Arguments
///
/// * `id` - The entity's id
/// * `name` - The entity's name
/// * `kind` - The entity's kind
/// * `metadata` - The entity's metadata
fn entity(id: &str, name: &str, kind: EntityKinds, metadata: EntityMetadata) -> Entity {
    Entity {
        id: Uuid::parse_str(id).unwrap(),
        name: name.to_owned(),
        kind,
        metadata,
        description: Some("old".to_owned()),
        submitter: "owner".to_owned(),
        groups: vec!["g1".to_owned()],
        tags: HashMap::new(),
        image: None,
        created: Utc::now(),
    }
}

/// An endpoint with a credential, two catalog models and one disabled model
fn endpoint_data() -> EndpointData {
    let meta = ModelEndpointEntity {
        provider: ModelProvider::LiteLlm {
            base_url: "https://gw.local/v1".to_owned(),
            key_header: None,
        },
        credential: Some(Uuid::parse_str(CREDENTIAL).unwrap()),
        catalog: vec![entry("a", 100), entry("b", 200)],
        bans: HashMap::new(),
        refresh: ModelRefreshStatus::default(),
        disabled_models: BTreeSet::from(["b".to_owned()]),
        modified: Utc::now(),
    };
    EndpointData {
        entity: entity(
            ENDPOINT,
            "gateway",
            EntityKinds::ModelEndpoint,
            EntityMetadata::ModelEndpoint(meta.clone()),
        ),
        meta,
        credential: Some("gw-key".to_owned()),
    }
}

/// A profile that calls the test endpoint's model `a`
fn profile_data() -> ProfileData {
    let meta = ModelProfileEntity {
        endpoint: Uuid::parse_str(ENDPOINT).unwrap(),
        model_id: "a".to_owned(),
        api: ModelApi::OpenAiChat,
        parameters: ModelParameters {
            temperature: Some(0.5),
            ..ModelParameters::default()
        },
        limits: None,
        capabilities: None,
        bans: HashMap::new(),
        modified: Utc::now(),
    };
    ProfileData {
        entity: entity(
            "33333333-3333-3333-3333-333333333333",
            "fast",
            EntityKinds::ModelProfile,
            EntityMetadata::ModelProfile(meta.clone()),
        ),
        meta,
        endpoint: "gateway".to_owned(),
    }
}

/// Start a mock API that answers each request line with a body, never needing to match one
///
/// # Arguments
///
/// * `router` - Picks the status and body of each answer from its request line
async fn mock<F>(router: F) -> (String, Arc<Mutex<Vec<String>>>)
where
    F: Fn(&str) -> (u16, String) + Send + Sync + 'static,
{
    // bind to a random local port and answer in the background
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = format!("http://{}", listener.local_addr().unwrap());
    let lines = Arc::new(Mutex::new(Vec::new()));
    let inner = lines.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            // read the request head, which is all the mock needs
            let mut buf = vec![0_u8; 8192];
            let read = stream.read(&mut buf).await.unwrap_or(0);
            let text = String::from_utf8_lossy(&buf[..read]).to_string();
            let line = text.lines().next().unwrap_or_default().to_owned();
            inner.lock().unwrap().push(line.clone());
            // answer with whatever the router picks
            let (code, body) = router(&line);
            let resp = format!(
                "HTTP/1.1 {code} Mock\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });
    (host, lines)
}

/// A client that talks to a mock API which answers every request with 404
async fn offline() -> Thorium {
    let (host, _) = mock(|_| (404, r#"{"error":"nothing here"}"#.to_owned())).await;
    Thorium::build(&host).token("token").build().await.unwrap()
}

/// Compute the update an edited endpoint view asks for
///
/// # Arguments
///
/// * `change` - Edits the view before it is diffed
async fn endpoint_update(change: impl FnOnce(&mut EndpointView)) -> Option<EntityUpdate> {
    let data = endpoint_data();
    let mut view = Endpoint::to_view(&data);
    change(&mut view);
    Endpoint::validate(&view, &data).unwrap();
    Endpoint::calculate_update(&offline().await, &data, view)
        .await
        .unwrap()
}

/// Compute the update an edited profile view asks for
///
/// # Arguments
///
/// * `change` - Edits the view before it is diffed
async fn profile_update(change: impl FnOnce(&mut ProfileView)) -> Option<EntityUpdate> {
    let data = profile_data();
    let mut view = Profile::to_view(&data);
    change(&mut view);
    Profile::validate(&view, &data).unwrap();
    Profile::calculate_update(&offline().await, &data, view)
        .await
        .unwrap()
}

/// The endpoint metadata update inside an update
///
/// # Arguments
///
/// * `update` - The update to read
fn endpoint_meta(update: &EntityUpdate) -> &EntityMetadataUpdate {
    update.metadata.as_ref().expect("no metadata update")
}

/// An endpoint's view shows its settings and credential name and nothing secret
#[test]
fn endpoint_view_shows_the_settings_by_name() {
    // build the view the editor shows
    let view = Endpoint::to_view(&endpoint_data());
    let doc = serde_json::to_value(&view).unwrap();
    // the read-only keys are marked and the credential is a name
    assert_eq!(doc["*id*"], ENDPOINT);
    assert_eq!(doc["*groups*"], json!(["g1"]));
    assert_eq!(doc["credential"], "gw-key");
    assert_eq!(doc["disabled_models"], json!(["b"]));
    assert_eq!(doc["catalog"][0]["model_id"], "a");
    assert_eq!(doc["catalog"][0]["context_window"], 100);
    // no key or value names a secret, the credential's id, or a fingerprint
    let text = doc.to_string();
    assert!(!text.contains("secret"), "view mentions a secret: {text}");
    assert!(
        !text.contains("fingerprint"),
        "view mentions a fingerprint: {text}"
    );
    assert!(
        !text.contains(CREDENTIAL),
        "view carries the credential id: {text}"
    );
}

/// The YAML the editor shows reads back as the same view
#[test]
fn endpoint_view_round_trips_through_yaml() {
    // render the editor text and parse it the way the flow does
    let view = Endpoint::to_view(&endpoint_data());
    let yaml = utils::curated_yaml(&view, Endpoint::FIELD_ORDER).unwrap();
    let back: EndpointView = serde_norway::from_str(&yaml).unwrap();
    assert_eq!(back, view);
    // the curated order puts the read-only id first and the catalog last
    let keys: Vec<&str> = yaml
        .lines()
        .filter(|line| !line.starts_with([' ', '-']) && line.contains(':'))
        .map(|line| line.split(':').next().unwrap().trim_matches(['\'', '"']))
        .collect();
    assert_eq!(
        keys,
        [
            "*id*",
            "name",
            "description",
            "*groups*",
            "provider",
            "credential",
            "disabled_models",
            "catalog"
        ]
    );
}

/// A profile's view round trips through YAML too
#[test]
fn profile_view_round_trips_through_yaml() {
    // render the editor text and parse it the way the flow does
    let mut data = profile_data();
    data.meta.limits = Some(ModelLimits {
        context_window: Some(5),
        max_output_tokens: None,
        source: LimitsSource::Manual,
    });
    let view = Profile::to_view(&data);
    let yaml = utils::curated_yaml(&view, Profile::FIELD_ORDER).unwrap();
    let back: ProfileView = serde_norway::from_str(&yaml).unwrap();
    assert_eq!(back, view);
    assert_eq!(back.endpoint, "gateway");
    assert_eq!(back.api, ModelApi::OpenAiChat);
}

/// A misspelled key anywhere in a document is refused rather than ignored
#[test]
fn views_refuse_unknown_keys() {
    // an unknown top-level key
    let mut doc = serde_json::to_value(Endpoint::to_view(&endpoint_data())).unwrap();
    doc["nmae"] = json!("x");
    assert!(serde_json::from_value::<EndpointView>(doc).is_err());
    // an unknown capability in a catalog row
    let mut doc = serde_json::to_value(Endpoint::to_view(&endpoint_data())).unwrap();
    doc["catalog"][0]["capabilities"]["vison"] = json!(true);
    assert!(serde_json::from_value::<EndpointView>(doc).is_err());
    // an unknown parameter in a profile
    let mut doc = serde_json::to_value(Profile::to_view(&profile_data())).unwrap();
    doc["parameters"]["temprature"] = json!(1.0);
    assert!(serde_json::from_value::<ProfileView>(doc).is_err());
    // a protocol that doesn't exist
    let mut doc = serde_json::to_value(Profile::to_view(&profile_data())).unwrap();
    doc["api"] = json!("Telepathy");
    assert!(serde_json::from_value::<ProfileView>(doc).is_err());
}

/// An unchanged endpoint view asks for no update
#[tokio::test]
async fn endpoint_without_edits_has_no_update() {
    assert!(endpoint_update(|_| {}).await.is_none());
}

/// Reordering catalog entries or listing the same models differently is not a change
#[tokio::test]
async fn endpoint_catalog_order_is_not_a_change() {
    assert!(
        endpoint_update(|view| view.catalog.reverse())
            .await
            .is_none()
    );
}

/// A new name and description reach the update, and no description clears it
#[tokio::test]
async fn endpoint_root_fields_map_to_the_update() {
    // a rename and a new description
    let update = endpoint_update(|view| {
        view.name = "gateway2".to_owned();
        view.description = Some("new".to_owned());
    })
    .await
    .unwrap();
    assert_eq!(update.name.as_deref(), Some("gateway2"));
    assert_eq!(update.description.as_deref(), Some("new"));
    assert!(update.metadata.is_none());
    // removing the description clears it
    let update = endpoint_update(|view| view.description = None)
        .await
        .unwrap();
    assert!(update.clear_description);
    assert!(update.description.is_none());
}

/// Changing the provider sends the whole new provider
#[tokio::test]
async fn endpoint_provider_change_is_sent() {
    // point the gateway somewhere else
    let provider = ModelProvider::LiteLlm {
        base_url: "https://other.local/v1".to_owned(),
        key_header: None,
    };
    let new = provider.clone();
    let update = endpoint_update(move |view| view.provider = ProviderDoc(new))
        .await
        .unwrap();
    let EntityMetadataUpdate::ModelEndpoint { provider: sent, .. } = endpoint_meta(&update) else {
        panic!("not an endpoint update");
    };
    assert_eq!(sent.as_ref(), Some(&provider));
}

/// Clearing the credential name clears the credential
#[tokio::test]
async fn endpoint_credential_can_be_cleared() {
    let update = endpoint_update(|view| view.credential = None)
        .await
        .unwrap();
    let EntityMetadataUpdate::ModelEndpoint {
        credential,
        clear_credential,
        ..
    } = endpoint_meta(&update)
    else {
        panic!("not an endpoint update");
    };
    assert_eq!(*clear_credential, Some(true));
    assert!(credential.is_none());
}

/// A new credential is found by its id or name before it is sent
#[tokio::test]
async fn endpoint_credential_is_resolved_before_it_is_sent() {
    // a mock API that knows one credential
    let id = "44444444-4444-4444-4444-444444444444";
    let secretless = entity(
        id,
        "other-key",
        EntityKinds::ModelCredential,
        EntityMetadata::ModelEndpoint(endpoint_data().meta),
    );
    let body = serde_json::to_string(&secretless).unwrap();
    let (host, lines) = mock(move |line| {
        if line.starts_with(&format!("GET /api/entities/{id}")) {
            (200, body.clone())
        } else {
            (404, r#"{"error":"nothing here"}"#.to_owned())
        }
    })
    .await;
    let thorium = Thorium::build(&host).token("token").build().await.unwrap();
    // name the credential by id in the view
    let data = endpoint_data();
    let mut view = Endpoint::to_view(&data);
    view.credential = Some(id.to_owned());
    let update = Endpoint::calculate_update(&thorium, &data, view)
        .await
        .unwrap()
        .unwrap();
    let EntityMetadataUpdate::ModelEndpoint { credential, .. } = endpoint_meta(&update) else {
        panic!("not an endpoint update");
    };
    assert_eq!(*credential, Some(Uuid::parse_str(id).unwrap()));
    assert_eq!(lines.lock().unwrap().len(), 1);
    // a credential that doesn't exist is an error and sends nothing
    let mut view = Endpoint::to_view(&data);
    view.credential = Some("missing-key".to_owned());
    let failed = Endpoint::calculate_update(&offline().await, &data, view).await;
    assert!(failed.is_err());
}

/// Disabled models are diffed into disables and enables
#[tokio::test]
async fn endpoint_disabled_models_map_to_disables_and_enables() {
    // disable a and enable b
    let update = endpoint_update(|view| view.disabled_models = vec!["a".to_owned()])
        .await
        .unwrap();
    let EntityMetadataUpdate::ModelEndpoint {
        disable_models,
        enable_models,
        ..
    } = endpoint_meta(&update)
    else {
        panic!("not an endpoint update");
    };
    assert_eq!(*disable_models, BTreeSet::from(["a".to_owned()]));
    assert_eq!(*enable_models, BTreeSet::from(["b".to_owned()]));
}

/// Catalog rows are diffed into additions, replacements and removals
#[tokio::test]
async fn endpoint_catalog_changes_map_to_adds_and_removes() {
    // change a, drop b and add c
    let update = endpoint_update(|view| {
        view.catalog[0].context_window = Some(999);
        view.catalog.remove(1);
        view.catalog.push(CatalogRow {
            model_id: "c".to_owned(),
            context_window: Some(7),
            max_output_tokens: None,
            capabilities: super::CapabilitiesView::default(),
        });
    })
    .await
    .unwrap();
    let EntityMetadataUpdate::ModelEndpoint {
        add_models,
        remove_models,
        ..
    } = endpoint_meta(&update)
    else {
        panic!("not an endpoint update");
    };
    let added: Vec<(&str, Option<u64>)> = add_models
        .iter()
        .map(|model| (model.model_id.as_str(), model.context_window))
        .collect();
    assert_eq!(added, [("a", Some(999)), ("c", Some(7))]);
    assert_eq!(remove_models, &vec!["b".to_owned()]);
}

/// An edited view the API would refuse is caught before any request
#[test]
fn endpoint_validation_names_the_problem() {
    let data = endpoint_data();
    let check = |change: &dyn Fn(&mut EndpointView)| {
        let mut view = Endpoint::to_view(&data);
        change(&mut view);
        Endpoint::validate(&view, &data).unwrap_err().to_string()
    };
    // an empty name
    assert!(check(&|view| view.name = " ".to_owned()).contains("name"));
    // a repeated catalog model
    let message = check(&|view| view.catalog.push(view.catalog[0].clone()));
    assert!(message.contains("more than once"), "{message}");
    // an empty catalog model id
    assert!(check(&|view| view.catalog[0].model_id.clear()).contains("model_id"));
    // a provider url that isn't a url
    let message = check(&|view| {
        view.provider = ProviderDoc(ModelProvider::LiteLlm {
            base_url: "not a url".to_owned(),
            key_header: None,
        });
    });
    assert!(message.contains("url"), "{message}");
    // an empty disabled model
    assert!(check(&|view| view.disabled_models.push(String::new())).contains("disabled"));
}

/// `--set`, `--unset` and `--patch` reach the catalog, lists and nested settings of a view
#[test]
fn edit_flags_change_the_endpoint_document() {
    // the document as the flow starts from it
    let mut doc = serde_json::to_value(Endpoint::to_view(&endpoint_data())).unwrap();
    // set a nested value, a list, and append a catalog row
    let ops = [
        parse_set("catalog[0].context_window=500").unwrap(),
        parse_set("disabled_models=[x,y]").unwrap(),
        parse_set("catalog[2]={model_id: c}").unwrap(),
        parse_unset("catalog[1]").unwrap(),
        parse_unset("credential").unwrap(),
    ];
    for op in &ops {
        op.apply(&mut doc).unwrap();
    }
    // a merge patch changes the description
    merge_patch(&mut doc, &json!({"description": null}));
    let view: EndpointView = serde_json::from_value(doc).unwrap();
    assert_eq!(view.catalog[0].context_window, Some(500));
    assert_eq!(view.disabled_models, ["x", "y"]);
    assert_eq!(view.catalog.last().unwrap().model_id, "c");
    assert_eq!(view.catalog.len(), 2);
    assert_eq!(view.credential, None);
    assert_eq!(view.description, None);
}

/// A bad `--set` value for the document's types is refused when the view is read
#[test]
fn edit_flags_with_the_wrong_type_are_refused() {
    let mut doc = serde_json::to_value(Endpoint::to_view(&endpoint_data())).unwrap();
    parse_set("catalog[0].context_window=lots")
        .unwrap()
        .apply(&mut doc)
        .unwrap();
    assert!(serde_json::from_value::<EndpointView>(doc).is_err());
}

/// An unchanged profile view asks for no update
#[tokio::test]
async fn profile_without_edits_has_no_update() {
    assert!(profile_update(|_| {}).await.is_none());
}

/// A profile's model, protocol and parameters are each sent when they change
#[tokio::test]
async fn profile_settings_map_to_the_update() {
    // change the model, the protocol and a parameter
    let update = profile_update(|view| {
        view.model_id = "b".to_owned();
        view.api = ModelApi::OpenAiResponses;
        view.parameters.temperature = Some(0.9);
        view.parameters.stop_sequences = vec!["END".to_owned()];
    })
    .await
    .unwrap();
    let EntityMetadataUpdate::ModelProfile {
        model_id,
        api,
        parameters,
        endpoint,
        ..
    } = update.metadata.as_ref().unwrap()
    else {
        panic!("not a profile update");
    };
    assert_eq!(model_id.as_deref(), Some("b"));
    assert_eq!(*api, Some(ModelApi::OpenAiResponses));
    let parameters = parameters.as_ref().unwrap();
    assert_eq!(parameters.temperature, Some(0.9));
    assert_eq!(parameters.stop_sequences, ["END"]);
    // the endpoint didn't change so it isn't looked up or sent
    assert!(endpoint.is_none());
}

/// Setting and clearing limits and capabilities map to replacements and clears
#[tokio::test]
async fn profile_limits_and_capabilities_set_and_clear() {
    // set both
    let update = profile_update(|view| {
        view.limits = Some(super::LimitsView {
            context_window: Some(10),
            max_output_tokens: Some(5),
        });
        view.capabilities = Some(super::CapabilitiesView {
            vision: true,
            ..super::CapabilitiesView::default()
        });
    })
    .await
    .unwrap();
    let EntityMetadataUpdate::ModelProfile {
        limits,
        capabilities,
        clear_limits,
        ..
    } = update.metadata.as_ref().unwrap()
    else {
        panic!("not a profile update");
    };
    let limits = limits.as_ref().unwrap();
    assert_eq!(
        (limits.context_window, limits.max_output_tokens),
        (Some(10), Some(5))
    );
    assert_eq!(limits.source, LimitsSource::Manual);
    assert!(capabilities.as_ref().unwrap().vision);
    assert!(clear_limits.is_none());
    // clear what a stored profile overrides
    let mut data = profile_data();
    data.meta.limits = Some(ModelLimits::default());
    data.meta.capabilities = Some(ModelCapabilities::default());
    let mut view = Profile::to_view(&data);
    view.limits = None;
    view.capabilities = None;
    let update = Profile::calculate_update(&offline().await, &data, view)
        .await
        .unwrap()
        .unwrap();
    let EntityMetadataUpdate::ModelProfile {
        clear_limits,
        clear_capabilities,
        ..
    } = update.metadata.as_ref().unwrap()
    else {
        panic!("not a profile update");
    };
    assert_eq!(*clear_limits, Some(true));
    assert_eq!(*clear_capabilities, Some(true));
}

/// A new endpoint name is looked up and sent as its id
#[tokio::test]
async fn profile_endpoint_is_resolved_before_it_is_sent() {
    // a mock API that knows another endpoint
    let id = "55555555-5555-5555-5555-555555555555";
    let other = entity(
        id,
        "other",
        EntityKinds::ModelEndpoint,
        EntityMetadata::ModelEndpoint(endpoint_data().meta),
    );
    let body = serde_json::to_string(&other).unwrap();
    let (host, _) = mock(move |line| {
        if line.starts_with(&format!("GET /api/entities/{id}")) {
            (200, body.clone())
        } else {
            (404, r#"{"error":"nothing here"}"#.to_owned())
        }
    })
    .await;
    let thorium = Thorium::build(&host).token("token").build().await.unwrap();
    // call the other endpoint by id
    let data = profile_data();
    let mut view = Profile::to_view(&data);
    view.endpoint = id.to_owned();
    let update = Profile::calculate_update(&thorium, &data, view)
        .await
        .unwrap()
        .unwrap();
    let EntityMetadataUpdate::ModelProfile { endpoint, .. } = update.metadata.as_ref().unwrap()
    else {
        panic!("not a profile update");
    };
    assert_eq!(*endpoint, Some(Uuid::parse_str(id).unwrap()));
}

/// A profile view the API would refuse is caught before any request
#[test]
fn profile_validation_names_the_problem() {
    let data = profile_data();
    let check = |change: &dyn Fn(&mut ProfileView)| {
        let mut view = Profile::to_view(&data);
        change(&mut view);
        Profile::validate(&view, &data).unwrap_err().to_string()
    };
    assert!(check(&|view| view.name.clear()).contains("name"));
    assert!(check(&|view| view.endpoint.clear()).contains("endpoint"));
    assert!(check(&|view| view.model_id.clear()).contains("model_id"));
}
