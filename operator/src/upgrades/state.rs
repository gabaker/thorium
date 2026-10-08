//! Reads and writes the upgrade state `ConfigMap` in a Thorium namespace

use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::{ObjectMeta, PostParams};
use kube::{Api, Client};
use std::collections::BTreeMap;
use thorium::Error;
use thorium::models::upgrades::{HASH_ANNOTATION, STATE_CONFIG_MAP, STATE_KEY, UpgradeState};

/// An upgrade state along with the `ConfigMap` version it was read from
#[derive(Debug, Clone)]
pub struct StoredState {
    /// The recorded upgrade state
    pub state: UpgradeState,
    /// The `resourceVersion` of the `ConfigMap` this state was read from, if it exists yet
    pub resource_version: Option<String>,
}

/// Parse the upgrade state out of its `ConfigMap`
///
/// # Arguments
///
/// * `cm` - The upgrade state `ConfigMap`
/// * `namespace` - The namespace the `ConfigMap` is in, for error messages
fn parse(cm: &ConfigMap, namespace: &str) -> Result<UpgradeState, Error> {
    // get the serialized state
    let raw = cm
        .data
        .as_ref()
        .and_then(|data| data.get(STATE_KEY))
        .ok_or_else(|| {
            Error::new(format!(
                "ConfigMap {namespace}/{STATE_CONFIG_MAP} has no {STATE_KEY} key"
            ))
        })?;
    // deserialize the state, filling in defaults for any field it doesn't set
    serde_json::from_str(raw).map_err(|error| {
        Error::new(format!(
            "ConfigMap {namespace}/{STATE_CONFIG_MAP} key {STATE_KEY} is not a valid upgrade \
             state: {error}"
        ))
    })
}

/// Load the upgrade state of a Thorium namespace, if one was recorded
///
/// # Arguments
///
/// * `client` - The kube client to read with
/// * `namespace` - The Thorium namespace
pub async fn load(client: &Client, namespace: &str) -> Result<Option<StoredState>, Error> {
    // read the state ConfigMap if it exists
    let api: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);
    let Some(cm) = api.get_opt(STATE_CONFIG_MAP).await? else {
        return Ok(None);
    };
    // parse the state it holds
    let state = parse(&cm, namespace)?;
    Ok(Some(StoredState {
        state,
        resource_version: cm.metadata.resource_version.clone(),
    }))
}

/// Build the `ConfigMap` that records an upgrade state
///
/// # Arguments
///
/// * `stored` - The state to record and the version of the `ConfigMap` it replaces
/// * `namespace` - The Thorium namespace
fn build(stored: &StoredState, namespace: &str) -> Result<ConfigMap, Error> {
    // label the state so its revision can be selected without parsing it
    let mut labels = stored.state.labels();
    labels.insert(
        "app.kubernetes.io/managed-by".to_owned(),
        "thorium-operator".to_owned(),
    );
    // keep the full hash in an annotation since labels are too short for it
    let annotations = BTreeMap::from([(HASH_ANNOTATION.to_owned(), stored.state.hash())]);
    // serialize the state readably for admins inspecting it
    let data = BTreeMap::from([(
        STATE_KEY.to_owned(),
        serde_json::to_string_pretty(&stored.state)?,
    )]);
    Ok(ConfigMap {
        metadata: ObjectMeta {
            name: Some(STATE_CONFIG_MAP.to_owned()),
            namespace: Some(namespace.to_owned()),
            labels: Some(labels),
            annotations: Some(annotations),
            resource_version: stored.resource_version.clone(),
            ..ObjectMeta::default()
        },
        data: Some(data),
        ..ConfigMap::default()
    })
}

/// Save an upgrade state, failing if the `ConfigMap` changed since it was read
///
/// The `ConfigMap` is created when the state has no `resourceVersion` and otherwise replaced
/// at that `resourceVersion`, so concurrent writers can't overwrite each other's progress.
///
/// # Arguments
///
/// * `client` - The kube client to write with
/// * `namespace` - The Thorium namespace
/// * `stored` - The state to save, whose `resourceVersion` is updated on success
pub async fn save(client: &Client, namespace: &str, stored: &mut StoredState) -> Result<(), Error> {
    // build the ConfigMap for this state
    let cm = build(stored, namespace)?;
    let api: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);
    // create the ConfigMap or replace the version we read
    let result = match &stored.resource_version {
        None => api.create(&PostParams::default(), &cm).await,
        Some(_) => {
            api.replace(STATE_CONFIG_MAP, &PostParams::default(), &cm)
                .await
        }
    };
    let written = result.map_err(|error| match error {
        // a conflict means someone else changed the state since we read it
        kube::Error::Api(response) if response.code == 409 => Error::new(format!(
            "ConfigMap {namespace}/{STATE_CONFIG_MAP} changed while the upgrade state was being \
             saved; retrying"
        )),
        other => Error::new(format!(
            "Failed to save ConfigMap {namespace}/{STATE_CONFIG_MAP}: {other}"
        )),
    })?;
    // remember the new version for the next write
    stored.resource_version = written.metadata.resource_version;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::clusters::tests::{FakeKube, kube_status};
    use crate::upgrades::tests::{CONFIG_MAPS_PATH, STATE_PATH, state_config_map};
    use thorium::models::upgrades::{HASH_LABEL, REVISION_LABEL, RevisionId, VERSION_LABEL};

    /// A saved state round trips through its `ConfigMap`
    #[test]
    fn state_round_trips() {
        // build a stored state
        let revision: RevisionId = "2026-10-v02".parse().expect("revision");
        let stored = StoredState {
            state: UpgradeState::new(revision.clone(), "1.8.1"),
            resource_version: Some("7".to_owned()),
        };
        // build its ConfigMap
        let cm = build(&stored, "thorium").expect("build");
        // the ConfigMap is labelled, annotated, and pinned to the version it replaces
        let labels = cm.metadata.labels.clone().expect("labels");
        assert_eq!(labels[REVISION_LABEL], "2026-10-v02");
        assert_eq!(
            cm.metadata.annotations.as_ref().expect("annotations")[HASH_ANNOTATION],
            stored.state.hash()
        );
        assert_eq!(cm.metadata.resource_version.as_deref(), Some("7"));
        // the state parses back out unchanged
        assert_eq!(parse(&cm, "thorium").expect("parse"), stored.state);
    }

    /// A `ConfigMap` without a valid state is an error naming it
    #[test]
    fn bad_state_is_reported() {
        // a ConfigMap without the state key fails
        let cm = ConfigMap::default();
        let error = parse(&cm, "thorium").expect_err("missing key");
        assert!(error.to_string().contains(STATE_KEY));
        // a ConfigMap with a malformed revision fails
        let cm = ConfigMap {
            data: Some(BTreeMap::from([(
                STATE_KEY.to_owned(),
                r#"{"revision":"latest"}"#.to_owned(),
            )])),
            ..ConfigMap::default()
        };
        assert!(parse(&cm, "thorium").is_err());
    }

    /// The `ConfigMap` convert-to-helm.sh creates parses and is relabelled when saved again
    #[test]
    fn script_seed_config_map_parses() {
        // the ConfigMap exactly as deploy/charts/scripts/convert-to-helm.sh builds it
        let seed: ConfigMap = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": STATE_CONFIG_MAP,
                "namespace": "thorium",
                "labels": {"thorium.sandia.gov/state-revision": "2026-10-v01"}
            },
            "data": {STATE_KEY: r#"{"schema_version":1,"revision":"2026-10-v01","applied":[{"revision":"2026-10-v01","step":"convert-to-helm","outcome":"Done","at":"2026-10-08T00:00:00Z","by":"convert-to-helm.sh"}]}"#}
        }))
        .expect("seed ConfigMap");
        // the seed parses with the script's history
        let state = parse(&seed, "thorium").expect("seed parses");
        assert!(state.finished("2026-10-v01", "convert-to-helm"));
        assert_eq!(state.applied_by_version, None);
        // saving it again adds the hash label and annotation the script leaves out
        let stored = StoredState {
            state,
            resource_version: Some("3".to_owned()),
        };
        let cm = build(&stored, "thorium").expect("build");
        let labels = cm.metadata.labels.expect("labels");
        assert_eq!(labels[REVISION_LABEL], "2026-10-v01");
        assert_eq!(labels[HASH_LABEL], stored.state.hash()[..32]);
        assert_eq!(labels["app.kubernetes.io/managed-by"], "thorium-operator");
        // the script records no Thorium version so there is no version label
        assert!(!labels.contains_key(VERSION_LABEL));
    }

    /// A state that was never saved builds a `ConfigMap` to create rather than replace
    #[test]
    fn new_state_builds_for_create() {
        // a state without a resourceVersion
        let revision: RevisionId = "2026-10-v02".parse().expect("revision");
        let stored = StoredState {
            state: UpgradeState::new(revision, "1.8.1"),
            resource_version: None,
        };
        let cm = build(&stored, "thorium").expect("build");
        // nothing pins the ConfigMap to an existing version
        assert_eq!(cm.metadata.resource_version, None);
        assert_eq!(cm.metadata.name.as_deref(), Some(STATE_CONFIG_MAP));
        assert_eq!(cm.metadata.namespace.as_deref(), Some("thorium"));
        // the state is written readably for admins
        let raw = &cm.data.as_ref().expect("data")[STATE_KEY];
        assert!(raw.contains('\n'));
        assert_eq!(
            labels_version(&cm),
            Some("1.8.1".to_owned()),
            "the version is labelled"
        );
    }

    /// Get the version label of a `ConfigMap`
    ///
    /// # Arguments
    ///
    /// * `cm` - The `ConfigMap` to read
    fn labels_version(cm: &ConfigMap) -> Option<String> {
        cm.metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get(VERSION_LABEL).cloned())
    }

    /// Loading reads the state and its version, and a missing `ConfigMap` is no state
    #[tokio::test]
    async fn load_reads_state_and_version() {
        // a namespace without a state has none
        let fake = FakeKube::default();
        assert!(
            load(&fake.client(), "thorium")
                .await
                .expect("load")
                .is_none()
        );
        // a recorded state is read with the version it was read at
        let revision: RevisionId = "2026-10-v01".parse().expect("revision");
        let state = UpgradeState::new(revision, "1.8.1");
        let fake = FakeKube::default().route("GET", STATE_PATH, 200, state_config_map(&state));
        let stored = load(&fake.client(), "thorium")
            .await
            .expect("load")
            .expect("state");
        assert_eq!(stored.state, state);
        assert_eq!(stored.resource_version.as_deref(), Some("5"));
    }

    /// Saving creates a new state, replaces a read one at its version, and remembers the
    /// version it wrote
    #[tokio::test]
    async fn save_creates_then_replaces() {
        // answer both writes with a new version
        let revision: RevisionId = "2026-10-v02".parse().expect("revision");
        let state = UpgradeState::new(revision, "1.8.1");
        let mut written = state_config_map(&state);
        written["metadata"]["resourceVersion"] = serde_json::json!("6");
        let fake = FakeKube::default()
            .route("POST", CONFIG_MAPS_PATH, 201, written.clone())
            .route("PUT", STATE_PATH, 200, written);
        let client = fake.client();
        // a state never saved is created
        let mut stored = StoredState {
            state,
            resource_version: None,
        };
        save(&client, "thorium", &mut stored).await.expect("create");
        assert_eq!(stored.resource_version.as_deref(), Some("6"));
        // saving it again replaces it at the version just written
        save(&client, "thorium", &mut stored)
            .await
            .expect("replace");
        let writes = fake.writes();
        assert_eq!(writes[0].method, "POST");
        assert_eq!(writes[1].method, "PUT");
        assert_eq!(writes[1].body["metadata"]["resourceVersion"], "6");
    }

    /// A state changed by someone else since it was read isn't overwritten
    #[tokio::test]
    async fn save_conflict_is_reported() {
        // the kube API rejects the replace as a conflict
        let fake = FakeKube::default().route("PUT", STATE_PATH, 409, kube_status(409, "Conflict"));
        let revision: RevisionId = "2026-10-v02".parse().expect("revision");
        let mut stored = StoredState {
            state: UpgradeState::new(revision, "1.8.1"),
            resource_version: Some("5".to_owned()),
        };
        // the error says the state changed and the version read is kept
        let error = save(&fake.client(), "thorium", &mut stored)
            .await
            .expect_err("conflict");
        assert!(
            error
                .to_string()
                .contains("changed while the upgrade state was being saved")
        );
        assert_eq!(stored.resource_version.as_deref(), Some("5"));
        // any other failure names the ConfigMap
        let fake = FakeKube::default().route("PUT", STATE_PATH, 403, kube_status(403, "Forbidden"));
        let error = save(&fake.client(), "thorium", &mut stored)
            .await
            .expect_err("forbidden");
        assert!(
            error
                .to_string()
                .contains("Failed to save ConfigMap thorium/thorium-upgrade-state")
        );
    }
}
