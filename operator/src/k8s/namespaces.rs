//! Reading the Namespace a `ThoriumCluster` runs in
//!
//! The operator only reads Namespaces (the chart grants it `get` and nothing else), so the labels
//! on them are set by a cluster admin rather than by anyone who can change a `ThoriumCluster` or
//! the chart's values.

use k8s_openapi::api::core::v1::Namespace;
use kube::{Api, Client};
use thorium::Error;

use super::crds::UpgradeSpec;

/// The label a cluster admin puts on a `ThoriumCluster`'s Namespace to mark it a dev cluster
pub const DEV_LABEL: &str = "thorium.sandia.gov/dev";

/// Check whether a Namespace carries the dev label set to `true`
///
/// # Arguments
///
/// * `client` - The kube client to read the Namespace with
/// * `namespace` - The Namespace to check
pub async fn has_dev_label(client: &Client, namespace: &str) -> Result<bool, Error> {
    // Namespaces are cluster scoped
    let api: Api<Namespace> = Api::all(client.clone());
    // read the Namespace, which must exist for a ThoriumCluster to be in it
    let found = api.get(namespace).await.map_err(|error| {
        Error::new(format!(
            "Failed to read Namespace {namespace} for its {DEV_LABEL} label: {error}"
        ))
    })?;
    // only the exact value true marks a dev cluster
    Ok(found
        .metadata
        .labels
        .is_some_and(|labels| labels.get(DEV_LABEL).is_some_and(|value| value == "true")))
}

/// Check whether a `ThoriumCluster` is a dev cluster
///
/// A dev cluster follows the operator's latest revision (`auto_target_dev`) and its Namespace
/// carries [`DEV_LABEL`] set to `true`. The Namespace is only read when `auto_target_dev` is set.
///
/// # Arguments
///
/// * `client` - The kube client to read the Namespace with
/// * `namespace` - The Namespace the `ThoriumCluster` is in
/// * `upgrade` - The `ThoriumCluster`'s upgrade settings
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "no non-test caller reads the dev label")
)]
pub async fn is_dev_cluster(
    client: &Client,
    namespace: &str,
    upgrade: &UpgradeSpec,
) -> Result<bool, Error> {
    // a cluster that doesn't follow the latest revision is never a dev cluster
    if !upgrade.auto_target_dev {
        return Ok(false);
    }
    // the Namespace label confirms it
    has_dev_label(client, namespace).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::clusters::tests::{FakeKube, kube_status};

    /// The path the operator reads the test Namespace at
    const NAMESPACE_PATH: &str = "/api/v1/namespaces/thorium";

    /// Build a Namespace with labels
    ///
    /// # Arguments
    ///
    /// * `labels` - The Namespace's labels
    fn namespace(labels: &serde_json::Value) -> serde_json::Value {
        // a Namespace named thorium carrying these labels
        serde_json::json!({
            "apiVersion": "v1",
            "kind": "Namespace",
            "metadata": {"name": "thorium", "labels": labels}
        })
    }

    /// Build upgrade settings that do or don't follow the latest revision
    ///
    /// # Arguments
    ///
    /// * `auto_target_dev` - Whether the cluster follows the latest revision
    fn upgrade(auto_target_dev: bool) -> UpgradeSpec {
        // default upgrade settings apart from auto_target_dev
        UpgradeSpec {
            auto_target_dev,
            ..UpgradeSpec::default()
        }
    }

    /// Only the dev label set to true on the Namespace counts, together with `auto_target_dev`
    #[tokio::test]
    async fn dev_cluster_needs_label_and_auto_target() {
        // each Namespace's labels and whether a cluster following the latest revision is dev
        let cases = [
            (serde_json::json!({DEV_LABEL: "true"}), true),
            (serde_json::json!({DEV_LABEL: "false"}), false),
            (serde_json::json!({DEV_LABEL: "yes"}), false),
            (serde_json::json!({"app": "thorium"}), false),
            (serde_json::Value::Null, false),
        ];
        for (labels, dev) in cases {
            let fake = FakeKube::default().route("GET", NAMESPACE_PATH, 200, namespace(&labels));
            let client = fake.client();
            assert_eq!(
                is_dev_cluster(&client, "thorium", &upgrade(true))
                    .await
                    .expect("read"),
                dev
            );
        }
    }

    /// A cluster without `auto_target_dev` isn't dev and its Namespace isn't read
    #[tokio::test]
    async fn dev_cluster_without_auto_target_skips_namespace() {
        // a labelled Namespace
        let fake = FakeKube::default().route(
            "GET",
            NAMESPACE_PATH,
            200,
            namespace(&serde_json::json!({DEV_LABEL: "true"})),
        );
        let client = fake.client();
        // the label alone doesn't make a dev cluster
        assert!(
            !is_dev_cluster(&client, "thorium", &upgrade(false))
                .await
                .expect("no read")
        );
        assert!(fake.requests().is_empty(), "{:?}", fake.requests());
    }

    /// A Namespace the operator isn't allowed to read is an error naming the label, not a
    /// silent "not dev"
    #[tokio::test]
    async fn unreadable_namespace_is_an_error() {
        // the kube API refuses the read
        let fake =
            FakeKube::default().route("GET", NAMESPACE_PATH, 403, kube_status(403, "Forbidden"));
        let client = fake.client();
        let error = has_dev_label(&client, "thorium")
            .await
            .expect_err("forbidden");
        assert!(error.to_string().contains(DEV_LABEL), "{error}");
    }
}
