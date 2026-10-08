//! Recognizes unconverted pre-Helm deployments and works out where a namespace without a
//! recorded upgrade state starts

use k8s_openapi::api::core::v1::Secret;
use kube::{Api, Client};
use thorium::Error;

use crate::k8s::crds::ThoriumCluster;

/// The Secret an operator creates for its own Thorium user the first time it runs
const OPERATOR_PASS_SECRET: &str = "thorium-operator-pass";

/// The Secret `convert-to-helm.sh` saves a converted cluster's pre-conversion `ThoriumCluster` in
const LEGACY_SNAPSHOT_SECRET: &str = "thorium-legacy-cluster";

/// The annotation that lets a `ThoriumCluster` keep its credentials inline in `spec.config`
pub const ALLOW_INLINE_CONFIG: &str = "thorium.sandia.gov/allow-inline-config";

/// Where a namespace without a recorded upgrade state starts
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detected {
    /// No operator has run here, so this is a fresh install at the latest revision
    Fresh,
    /// An operator already ran here before upgrade states were recorded
    BeforeTracking,
    /// A pre-Helm deployment, which starts at the baseline revision, and why it is one
    PreHelm(&'static str),
}

/// List what identifies a `ThoriumCluster` as an unconverted pre-Helm deployment
///
/// The minithor and megathor scripts deployed Thorium before the Helm charts with every
/// credential inline in `spec.config` and no `config_secrets`, which the charts always render.
/// A cluster written that way on purpose opts out with the [`ALLOW_INLINE_CONFIG`] annotation.
/// Returns nothing when the cluster isn't one.
///
/// # Arguments
///
/// * `cluster` - The `ThoriumCluster` being reconciled
#[must_use]
pub fn legacy_signals(cluster: &ThoriumCluster) -> Option<Vec<String>> {
    // clusters that keep inline credentials on purpose are left alone
    let allowed = allows_inline(cluster);
    // pre-Helm clusters hold Thorium's secret key inline and name no config secrets
    let inline = cluster.spec.config.pointer("/thorium/secret_key").is_some();
    if allowed || !inline || !cluster.spec.config_secrets.is_empty() {
        return None;
    }
    Some(vec![
        "spec.config holds thorium.secret_key inline".to_owned(),
        "the ThoriumCluster has no config_secrets".to_owned(),
    ])
}

/// Check whether a Secret exists without reading its data
///
/// # Arguments
///
/// * `secrets` - The Secret API of the namespace
/// * `namespace` - The namespace, for error messages
/// * `name` - The name of the Secret
async fn secret_exists(secrets: &Api<Secret>, namespace: &str, name: &str) -> Result<bool, Error> {
    // only the metadata is needed to know the Secret is there
    let found = secrets.get_metadata_opt(name).await.map_err(|error| {
        Error::new(format!(
            "Failed to check for Secret {namespace}/{name}: {error}"
        ))
    })?;
    Ok(found.is_some())
}

/// Check whether a `ThoriumCluster` opted into inline config with the [`ALLOW_INLINE_CONFIG`]
/// annotation, which only counts when set to exactly `true`
///
/// # Arguments
///
/// * `cluster` - The `ThoriumCluster` to check
fn allows_inline(cluster: &ThoriumCluster) -> bool {
    cluster
        .metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(ALLOW_INLINE_CONFIG))
        .is_some_and(|value| value == "true")
}

/// Check whether a `ThoriumCluster` keeps its config inline on purpose without config secrets
///
/// # Arguments
///
/// * `cluster` - The `ThoriumCluster` being reconciled
fn inline_by_choice(cluster: &ThoriumCluster) -> bool {
    allows_inline(cluster) && cluster.spec.config_secrets.is_empty()
}

/// Work out where a namespace without a recorded upgrade state starts
///
/// A namespace converted by `convert-to-helm.sh` keeps the pre-conversion `ThoriumCluster` in
/// [`LEGACY_SNAPSHOT_SECRET`], so one that lost its state is still pre-Helm. Every operator
/// creates [`OPERATOR_PASS_SECRET`] the first time it runs, so a namespace without it is a
/// fresh install. Where an operator did run, a cluster that keeps its config inline on purpose
/// with no config secrets is a pre-Helm deployment kept outside Helm, while one with config
/// secrets was deployed by the Helm charts before states were recorded.
///
/// # Arguments
///
/// * `client` - The kube client to read with
/// * `namespace` - The namespace of the `ThoriumCluster`
/// * `cluster` - The `ThoriumCluster` being reconciled
pub async fn detect(
    client: &Client,
    namespace: &str,
    cluster: &ThoriumCluster,
) -> Result<Detected, Error> {
    // build a Secret API for this namespace
    let secrets: Api<Secret> = Api::namespaced(client.clone(), namespace);
    // a converted deployment that lost its state must still run every upgrade step
    if secret_exists(&secrets, namespace, LEGACY_SNAPSHOT_SECRET).await? {
        return Ok(Detected::PreHelm(
            "pre-Helm deployment converted by convert-to-helm.sh (Secret thorium-legacy-cluster \
             exists) whose upgrade state was lost",
        ));
    }
    // a namespace no operator ran in is a fresh install
    if !secret_exists(&secrets, namespace, OPERATOR_PASS_SECRET).await? {
        return Ok(Detected::Fresh);
    }
    // an operator ran here, so tell a pre-Helm deployment kept outside Helm from a Helm one
    if inline_by_choice(cluster) {
        Ok(Detected::PreHelm(
            "pre-Helm deployment kept outside Helm (inline config allowed by annotation and no \
             config_secrets)",
        ))
    } else {
        Ok(Detected::BeforeTracking)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::clusters::tests::{FakeKube, cluster_from_spec, kube_status, pre_helm_spec};
    use crate::upgrades::tests::{OPERATOR_PASS_PATH, SNAPSHOT_PATH, secret_metadata};
    use std::collections::BTreeMap;

    /// Build a cluster from its config and config secrets
    ///
    /// # Arguments
    ///
    /// * `config` - The cluster's `spec.config`
    /// * `config_secrets` - The cluster's `spec.config_secrets`
    fn cluster(config: &serde_json::Value, config_secrets: &serde_json::Value) -> ThoriumCluster {
        cluster_from_spec(serde_json::json!({
            "components": {"api": {}},
            "registry": "registry/thorium",
            "config": config,
            "config_secrets": config_secrets
        }))
    }

    /// Build a chart shaped cluster with config secrets
    fn chart_cluster() -> ThoriumCluster {
        cluster(
            &serde_json::json!({}),
            &serde_json::json!([{"name": "thorium-config-secrets"}]),
        )
    }

    /// Build a pre-Helm cluster that opted into inline config with no config secrets
    fn inline_cluster() -> ThoriumCluster {
        // annotate a pre-Helm cluster to keep its inline config
        let mut inline = cluster_from_spec(pre_helm_spec());
        inline.metadata.annotations = Some(BTreeMap::from([(
            ALLOW_INLINE_CONFIG.to_owned(),
            "true".to_owned(),
        )]));
        inline
    }

    /// Pre-Helm clusters are recognized unless they opt out
    #[test]
    fn recognizes_legacy_clusters() {
        // a chart rendered cluster is not legacy
        assert_eq!(legacy_signals(&chart_cluster()), None);
        // a cluster deployed by the pre-Helm minithor or megathor scripts is
        let legacy = cluster_from_spec(pre_helm_spec());
        assert_eq!(
            legacy_signals(&legacy).map(|signals| signals.len()),
            Some(2)
        );
        // the annotation keeps inline credentials on purpose
        assert_eq!(legacy_signals(&inline_cluster()), None);
    }

    /// Only an annotation of exactly `true` opts a cluster out
    #[test]
    fn opt_out_needs_exact_true() {
        // a pre-Helm cluster with every other annotation value is still legacy
        let mut legacy = cluster_from_spec(pre_helm_spec());
        for value in ["false", "True", "yes", ""] {
            legacy.metadata.annotations = Some(BTreeMap::from([(
                ALLOW_INLINE_CONFIG.to_owned(),
                value.to_owned(),
            )]));
            assert!(
                legacy_signals(&legacy).is_some(),
                "{value:?} should not opt out"
            );
        }
    }

    /// Clusters that name config secrets or hold no inline secret key aren't legacy
    #[test]
    fn converted_shapes_are_not_legacy() {
        // inline credentials alongside config secrets are a chart cluster with overrides
        let mixed = cluster(
            &serde_json::json!({"thorium": {"secret_key": "k"}}),
            &serde_json::json!([{"name": "thorium-config-secrets"}]),
        );
        assert_eq!(legacy_signals(&mixed), None);
        // a cluster with neither has nothing identifying it as pre-Helm
        let bare = cluster(&serde_json::json!({"thorium": {}}), &serde_json::json!([]));
        assert_eq!(legacy_signals(&bare), None);
        // a null secret key still counts as inline, since only the key's presence is checked
        let null_key = cluster(
            &serde_json::json!({"thorium": {"secret_key": null}}),
            &serde_json::json!([]),
        );
        assert!(legacy_signals(&null_key).is_some());
    }

    /// A full `ThoriumCluster` as the pre-Helm operator wrote it is recognized
    #[test]
    fn pre_helm_operator_cr_is_legacy() {
        // the pre-Helm operator stored a fully defaulted config inline and none of the fields
        // the charts render
        let mut config = crate::k8s::clusters::tests::sample_config();
        config["thorium"]["secret_key"] = serde_json::json!("legacy-key");
        let legacy = cluster_from_spec(serde_json::json!({
            "components": {
                "api": {"replicas": 1, "env": [], "cmd": ["/app/thorium"], "args": [], "resources": {"cpu": 1000, "memory": 1024}},
                "scaler": {"service_account": true},
                "search_streamer": {},
                "event_handler": {}
            },
            "registry": "registry/thorium",
            "version": "1.8.1",
            "image_pull_policy": "Always",
            "config": config
        }));
        // every field the charts render takes its default
        assert!(legacy.spec.config_secrets.is_empty());
        assert!(legacy.spec.bootstrap.is_none());
        assert!(legacy.spec.upgrade.target_revision.is_none());
        // and the cluster is recognized as unconverted
        assert_eq!(
            legacy_signals(&legacy).map(|signals| signals.len()),
            Some(2)
        );
    }

    /// A namespace holding the operator's user Secret was deployed before states were recorded
    #[tokio::test]
    async fn detect_by_operator_secret() {
        // a namespace without the Secret is a fresh install
        let fake = FakeKube::default();
        assert_eq!(
            detect(&fake.client(), "thorium", &chart_cluster())
                .await
                .expect("detect"),
            Detected::Fresh
        );
        // a namespace with it was deployed before
        let fake = FakeKube::default().route(
            "GET",
            OPERATOR_PASS_PATH,
            200,
            secret_metadata(OPERATOR_PASS_SECRET),
        );
        assert_eq!(
            detect(&fake.client(), "thorium", &chart_cluster())
                .await
                .expect("detect"),
            Detected::BeforeTracking
        );
        // a failed lookup is an error rather than a guess
        let fake = FakeKube::default().route(
            "GET",
            OPERATOR_PASS_PATH,
            403,
            kube_status(403, "Forbidden"),
        );
        let error = detect(&fake.client(), "thorium", &chart_cluster())
            .await
            .expect_err("forbidden");
        assert!(error.to_string().contains(OPERATOR_PASS_SECRET));
    }

    /// A converted namespace that lost its state starts at the baseline whatever its cluster
    #[tokio::test]
    async fn detect_converted_by_snapshot_secret() {
        // a converted chart cluster whose operator already ran
        let fake = FakeKube::default()
            .route(
                "GET",
                SNAPSHOT_PATH,
                200,
                secret_metadata(LEGACY_SNAPSHOT_SECRET),
            )
            .route(
                "GET",
                OPERATOR_PASS_PATH,
                200,
                secret_metadata(OPERATOR_PASS_SECRET),
            );
        let Detected::PreHelm(reason) = detect(&fake.client(), "thorium", &chart_cluster())
            .await
            .expect("detect")
        else {
            panic!("a converted namespace should be pre-Helm");
        };
        assert!(reason.contains("convert-to-helm.sh"));
        // a failed snapshot lookup is an error naming the Secret
        let fake =
            FakeKube::default().route("GET", SNAPSHOT_PATH, 403, kube_status(403, "Forbidden"));
        let error = detect(&fake.client(), "thorium", &chart_cluster())
            .await
            .expect_err("forbidden");
        assert!(error.to_string().contains(LEGACY_SNAPSHOT_SECRET));
    }

    /// A pre-Helm cluster kept outside Helm with inline config starts at the baseline once an
    /// operator ran, while a fresh one is still a fresh install
    #[tokio::test]
    async fn detect_inline_cluster_kept_outside_helm() {
        // an operator already ran next to an inline cluster without config secrets
        let fake = FakeKube::default().route(
            "GET",
            OPERATOR_PASS_PATH,
            200,
            secret_metadata(OPERATOR_PASS_SECRET),
        );
        let Detected::PreHelm(reason) = detect(&fake.client(), "thorium", &inline_cluster())
            .await
            .expect("detect")
        else {
            panic!("an inline cluster an operator ran for should be pre-Helm");
        };
        assert!(reason.contains("kept outside Helm"));
        // the same annotation alongside config secrets is a Helm cluster
        let mut mixed = inline_cluster();
        mixed.spec.config_secrets = chart_cluster().spec.config_secrets;
        assert_eq!(
            detect(&fake.client(), "thorium", &mixed)
                .await
                .expect("detect"),
            Detected::BeforeTracking
        );
        // an inline cluster in a namespace no operator ran in is a fresh install
        assert_eq!(
            detect(&FakeKube::default().client(), "thorium", &inline_cluster())
                .await
                .expect("detect"),
            Detected::Fresh
        );
    }
}
