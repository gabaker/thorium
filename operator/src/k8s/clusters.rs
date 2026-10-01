use k8s_openapi::api::{
    apps::v1::Deployment,
    core::v1::{ConfigMap, Node, Pod, Secret, Service},
};
use kube::{Api, Client};
use std::sync::Arc;
use thorium::{Conf, Error};

use super::crds;

/// Wrapper for ThoriumCluster metadata
#[derive(Clone)]
pub struct ClusterMeta {
    /// namespace in k8s
    pub namespace: String,
    /// name of ThoriumCluster instance
    pub name: String,
    /// kube api client
    pub client: Client,
    /// thorium cluster custom resource spec
    pub cluster: Arc<crds::ThoriumCluster>,
    /// The Thorium config built from the spec's config merged with its config secrets
    pub conf: Conf,
    /// k8s api instance for ConfigMaps
    pub cm_api: Api<ConfigMap>,
    /// k8s api instance for Deployments
    pub deploy_api: Api<Deployment>,
    /// k8s api instance for Nodes
    pub node_api: Api<Node>,
    /// k8s api instance for Pods
    pub pod_api: Api<Pod>,
    /// k8s api instance for Secrets
    pub secret_api: Api<Secret>,
    /// k8s api instance for Services
    pub service_api: Api<Service>,
}

impl ClusterMeta {
    /// Build a new wrapper for k8s cluster metadata
    ///
    /// # Arguments
    ///
    /// * `cluster` - Thorium cluster definition
    pub async fn new(cluster: &Arc<crds::ThoriumCluster>, client: &Client) -> Result<Self, Error> {
        // grab cluster name from ThoriumCluster metadata
        let mut name = String::new();
        match cluster.metadata.name.as_ref() {
            Some(cluster_name) => name.push_str(cluster_name),
            None => {
                return Err(Error::new(format!(
                    "Could not get ThoriumCluster name from metadata"
                )));
            }
        }
        // grab namespace from ThoriumCluster metadata
        let mut namespace = String::new();
        match cluster.metadata.namespace.as_ref() {
            Some(cluster_namespace) => namespace.push_str(cluster_namespace),
            None => {
                return Err(Error::new(format!(
                    "Could not get ThoriumCluster namespace from metadata"
                )));
            }
        }
        // build kube api client
        let cm_api: Api<ConfigMap> = Api::namespaced(client.clone(), &namespace);
        let deploy_api: Api<Deployment> = Api::namespaced(client.clone(), &namespace);
        let node_api: Api<Node> = Api::all(client.clone());
        let pod_api: Api<Pod> = Api::namespaced(client.clone(), &namespace);
        let secret_api: Api<Secret> = Api::namespaced(client.clone(), &namespace);
        let service_api: Api<Service> = Api::namespaced(client.clone(), &namespace);
        // build our Thorium config from the spec and its config secrets
        let conf = resolve_config(cluster, &secret_api).await?;
        // return the built cluster
        Ok(ClusterMeta {
            name: name,
            client: client.clone(),
            namespace,
            cluster: cluster.clone(),
            conf,
            cm_api,
            deploy_api,
            node_api,
            pod_api,
            secret_api,
            service_api,
        })
    }
}

/// Deep merge one JSON value into another
///
/// Objects are merged key by key while any other value replaces the existing one.
///
/// # Arguments
///
/// * `base` - The value to merge into
/// * `overlay` - The value to merge over `base`
pub fn deep_merge(base: &mut serde_json::Value, overlay: serde_json::Value) {
    match (base, overlay) {
        // merge objects key by key
        (serde_json::Value::Object(base_map), serde_json::Value::Object(overlay_map)) => {
            // merge each overlay key into the base object
            for (key, value) in overlay_map {
                deep_merge(
                    base_map.entry(key).or_insert(serde_json::Value::Null),
                    value,
                );
            }
        }
        // any non-object value replaces the base value
        (base, overlay) => *base = overlay,
    }
}

/// Read a partial Thorium config from a Secret
///
/// # Arguments
///
/// * `secret_api` - The Secret API for the `ThoriumCluster`'s namespace
/// * `secret_ref` - The Secret and key holding the partial config
async fn read_config_secret(
    secret_api: &Api<Secret>,
    secret_ref: &crds::ConfigSecretRef,
) -> Result<serde_json::Value, Error> {
    // get the referenced secret
    let secret = secret_api.get(&secret_ref.name).await.map_err(|error| {
        Error::new(format!(
            "Failed to get config secret {}: {error}",
            secret_ref.name
        ))
    })?;
    // get the partial config from this secret
    let raw = secret
        .data
        .as_ref()
        .and_then(|data| data.get(&secret_ref.key))
        .ok_or_else(|| {
            Error::new(format!(
                "Config secret {} has no key {}",
                secret_ref.name, secret_ref.key
            ))
        })?;
    // parse the partial config as YAML
    serde_norway::from_slice(&raw.0).map_err(|error| {
        Error::new(format!(
            "Config secret {}/{} is not valid YAML: {error}",
            secret_ref.name, secret_ref.key
        ))
    })
}

/// Build the Thorium config for a `ThoriumCluster`
///
/// This starts with the spec's config and deep merges each config secret over it in order.
///
/// # Arguments
///
/// * `cluster` - The `ThoriumCluster` to build a config for
/// * `secret_api` - The Secret API for the `ThoriumCluster`'s namespace
pub async fn resolve_config(
    cluster: &crds::ThoriumCluster,
    secret_api: &Api<Secret>,
) -> Result<Conf, Error> {
    // start with the config in the spec
    let mut merged = cluster.spec.config.clone();
    // merge each config secret over our config in order
    for secret_ref in &cluster.spec.config_secrets {
        // read this secret's partial config
        let overlay = read_config_secret(secret_api, secret_ref).await?;
        // merge this partial config into our config
        deep_merge(&mut merged, overlay);
    }
    // cast our merged config into a full Thorium config
    serde_json::from_value(merged).map_err(|error| {
        // list the secrets we merged so a missing field is easy to track down
        let sources = cluster
            .spec
            .config_secrets
            .iter()
            .map(|secret_ref| format!("{}/{}", secret_ref.name, secret_ref.key))
            .collect::<Vec<String>>();
        Error::new(format!(
            "Invalid Thorium config from spec.config merged with config secrets {sources:?}: {error}"
        ))
    })
}
