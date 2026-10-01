//! The watcher for changes in Thorium CRs

use futures::StreamExt;
use k8s_openapi::api::core::v1::{ConfigMap, Secret};
use kube::api::{ListParams, Patch, PatchParams};
use kube::core::PartialObjectMeta;
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{self, Event as Finalizer, finalizer};
use kube::runtime::reflector::{self, ObjectRef, Store};
use kube::runtime::watcher::{Config, metadata_watcher, watcher};
use kube::runtime::{Controller, WatchStreamExt};
use kube::{Api, Client};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;
use thorium::Error;

use crate::args::OperateCluster;
use crate::k8s::clusters::{ClusterMeta, cluster_name_and_namespace, is_permanent_config_error};
use crate::k8s::config_maps::BANNER_CONFIG_MAP;
use crate::k8s::controller::SharedInfo;
use crate::k8s::crds::{self, ClusterPhase, ThoriumCluster};
use crate::k8s::operate;

/// Controller state including kubeapi client and url
#[derive(Clone)]
pub struct State {
    /// kube API client
    client: Client,
    /// ingress route for Thorium API
    url: Option<String>,
    /// The shared thorium operator info
    shared: Arc<SharedInfo>,
}

/// Methods operating on controller state
impl State {
    /// Wrap state in Arc
    pub fn to_context(&self) -> Arc<State> {
        Arc::new(self.clone())
    }
}

/// Handle errors in the reconcile process
pub fn error_policy(_cluster: Arc<ThoriumCluster>, error: &Error, _state: Arc<State>) -> Action {
    println!("Controller error:\n\t{}", error);
    println!(
        "Requeuing ThoriumCluster reconciliation in {} seconds",
        super::RECONCILE_ERROR_REQUEUE_SECS
    );
    Action::requeue(Duration::from_secs(super::RECONCILE_ERROR_REQUEUE_SECS))
}

/// Remove this operator's finalizer from a `ThoriumCluster`
///
/// This lets a `ThoriumCluster` whose config can no longer be resolved still be deleted. The
/// JSON patch first tests that our finalizer is still at the index we found it at, so a
/// concurrent change to the finalizer list fails the patch instead of removing the wrong one.
///
/// # Arguments
///
/// * `cluster` - The `ThoriumCluster` to remove the finalizer from
/// * `client` - The kube client to patch the `ThoriumCluster` with
async fn remove_finalizer(cluster: &ThoriumCluster, client: &Client) -> Result<Action, Error> {
    // get this cluster's name and namespace
    let (name, namespace) = cluster_name_and_namespace(cluster)?;
    // find our finalizer on this cluster
    let Some(index) = cluster
        .metadata
        .finalizers
        .iter()
        .flatten()
        .position(|finalizer| finalizer == crds::CRD_NAME)
    else {
        return Ok(Action::await_change());
    };
    // test that our finalizer is still at this index and then remove it
    let pointer = format!("/metadata/finalizers/{index}");
    let patch: json_patch::Patch = serde_json::from_value(serde_json::json!([
        {"op": "test", "path": pointer, "value": crds::CRD_NAME},
        {"op": "remove", "path": pointer},
    ]))?;
    // patch the finalizers on this cluster
    let clusters_api: Api<ThoriumCluster> = Api::namespaced(client.clone(), &namespace);
    clusters_api
        .patch(&name, &PatchParams::default(), &Patch::Json::<()>(patch))
        .await
        .map_err(|error| {
            Error::new(format!(
                "Failed to remove finalizer from {namespace}/{name}: {error}"
            ))
        })?;
    Ok(Action::await_change())
}

/// Release a `ThoriumCluster` being deleted whose config can't be resolved
///
/// Only permanent config errors release the cluster; anything else is returned so the
/// deletion is retried once the config can be resolved again.
///
/// # Arguments
///
/// * `cluster` - The `ThoriumCluster` being deleted
/// * `client` - The kube client to clean up with
/// * `shared` - Data shared across watchers
/// * `error` - The error from resolving this cluster's config
async fn release_unresolvable(
    cluster: &ThoriumCluster,
    client: &Client,
    shared: &SharedInfo,
    error: Error,
) -> Result<Action, Error> {
    // retry anything that might resolve on its own
    if !is_permanent_config_error(&error) {
        return Err(error);
    }
    println!(
        "ThoriumCluster config can't be resolved, skipping node cleanup before deletion: {error}"
    );
    // stop the watchers from acting on this cluster before cleaning up
    let (name, namespace) = cluster_name_and_namespace(cluster)?;
    shared
        .info
        .pin()
        .remove(&SharedInfo::key(&namespace, &name));
    // remove the resources that don't depend on this cluster's config
    operate::cleanup_namespace(cluster, client).await?;
    // let k8s finish deleting this cluster
    remove_finalizer(cluster, client).await
}

/// Get the error our apply or cleanup returned out of a finalizer error
///
/// Errors from the finalizer itself (such as a failed finalizer patch) are kept with a
/// short prefix since they don't come from our own code.
///
/// # Arguments
///
/// * `error` - The error the finalizer returned
fn unwrap_finalizer_error(error: finalizer::Error<Error>) -> Error {
    match error {
        // our own errors already describe what failed
        finalizer::Error::ApplyFailed(error) | finalizer::Error::CleanupFailed(error) => error,
        // anything else came from managing the finalizer
        other => Error::new(format!("Finalizer error: {other}")),
    }
}

/// Reconcile changes to `ThoriumCluster`
///
/// # Arguments
///
/// * `cluster` - Thorium cluster being changed
/// * `state` - Controller context including client instance and optional URL
pub async fn reconcile(cluster: Arc<ThoriumCluster>, state: Arc<State>) -> Result<Action, Error> {
    // build cluster metadata
    let meta = match ClusterMeta::new(&cluster, &state.client).await {
        Ok(meta) => meta,
        Err(error) => {
            // a cluster being deleted with a permanently broken config is released
            if cluster.metadata.deletion_timestamp.is_some() {
                return release_unresolvable(&cluster, &state.client, &state.shared, error).await;
            }
            // record why we couldn't build this cluster's config
            crds::set_status(
                &state.client,
                &cluster,
                ClusterPhase::Error,
                Some(error.to_string()),
            )
            .await;
            return Err(error);
        }
    };
    let clusters_api: Api<ThoriumCluster> = Api::namespaced(meta.client.clone(), &meta.namespace);
    println!(
        "Reconciling ThoriumCluster changes for {} in namespace {}",
        meta.name, meta.namespace
    );
    // apply or clean up this cluster
    let result = finalizer(
        &clusters_api,
        crds::CRD_NAME,
        cluster.clone(),
        |event| async {
            match event {
                Finalizer::Apply(_cluster) => {
                    operate::apply(&meta, state.url.clone(), &state.shared).await
                }
                Finalizer::Cleanup(_cluster) => operate::cleanup(&meta, &state.shared).await,
            }
        },
    )
    .await
    .map_err(unwrap_finalizer_error);
    // record any failure in this cluster's status
    if let Err(error) = &result {
        crds::set_status(
            &state.client,
            &cluster,
            ClusterPhase::Error,
            Some(error.to_string()),
        )
        .await;
    }
    result
}

/// Hash the parts of a `ThoriumCluster` that should trigger a reconcile
///
/// Status updates are excluded so the operator's own status patches don't retrigger it.
///
/// # Arguments
///
/// * `cluster` - The `ThoriumCluster` to hash
// predicates must return an Option even though this one always has a hash
#[allow(clippy::unnecessary_wraps)]
fn reconcile_predicate(cluster: &ThoriumCluster) -> Option<u64> {
    // build a hasher
    let mut hasher = DefaultHasher::new();
    // hash the spec generation, finalizers, and deletion state
    cluster.metadata.generation.hash(&mut hasher);
    cluster.metadata.finalizers.hash(&mut hasher);
    cluster
        .metadata
        .deletion_timestamp
        .as_ref()
        .map(|time| time.0.to_string())
        .hash(&mut hasher);
    Some(hasher.finish())
}

/// Map a Secret to every `ThoriumCluster` in its namespace that references it
///
/// # Arguments
///
/// * `clusters` - The store of `ThoriumCluster`s we watch
/// * `secret` - The metadata of the Secret that changed
fn secret_to_clusters(
    clusters: &Store<ThoriumCluster>,
    secret: &PartialObjectMeta<Secret>,
) -> Vec<ObjectRef<ThoriumCluster>> {
    // get the name and namespace of this secret
    let (Some(name), Some(namespace)) = (&secret.metadata.name, &secret.metadata.namespace) else {
        return Vec::new();
    };
    // find the clusters in this namespace whose spec names this secret
    clusters
        .state()
        .iter()
        .filter(|cluster| cluster.metadata.namespace.as_ref() == Some(namespace))
        .filter(|cluster| cluster.references_secret(name))
        .map(|cluster| ObjectRef::from_obj(cluster.as_ref()))
        .collect()
}

/// Map the banner `ConfigMap` to every `ThoriumCluster` in its namespace
///
/// The API only reads the banner at startup, so a changed banner reconciles the clusters
/// whose API mounts it to roll the API out.
///
/// # Arguments
///
/// * `clusters` - The store of `ThoriumCluster`s we watch
/// * `cm` - The metadata of the `ConfigMap` that changed
fn banner_to_clusters(
    clusters: &Store<ThoriumCluster>,
    cm: &PartialObjectMeta<ConfigMap>,
) -> Vec<ObjectRef<ThoriumCluster>> {
    // only the banner ConfigMap is mounted by the API
    let (Some(name), Some(namespace)) = (&cm.metadata.name, &cm.metadata.namespace) else {
        return Vec::new();
    };
    if name != BANNER_CONFIG_MAP {
        return Vec::new();
    }
    // find every cluster in this namespace
    clusters
        .state()
        .iter()
        .filter(|cluster| cluster.metadata.namespace.as_ref() == Some(namespace))
        .map(|cluster| ObjectRef::from_obj(cluster.as_ref()))
        .collect()
}

/// Watch our `ThoriumCluster`s for any changes
///
/// # Arguments
///
/// * `client` - The kube client to watch with
/// * `args` - The command line args passed to the operator
/// * `shared` - Data shared across watchers
pub async fn start(client: Client, args: OperateCluster, shared: Arc<SharedInfo>) {
    // get the ThoriumCluster api for the namespaces we watch
    let clusters_api: Api<ThoriumCluster> = super::scoped_api(&client, args.namespace.as_deref());
    // make sure we can list ThoriumClusters
    if let Err(e) = clusters_api.list(&ListParams::default().limit(1)).await {
        println!("Failed to list ThoriumCluster API: {}", e);
        std::process::exit(1);
    }
    // get the Secret api for the namespaces we watch
    let secrets_api: Api<Secret> = super::scoped_api(&client, args.namespace.as_deref());
    // get the ConfigMap api for the namespaces we watch
    let cms_api: Api<ConfigMap> = super::scoped_api(&client, args.namespace.as_deref());
    // build our controller state
    let state = State {
        client: client.clone(),
        url: args.url.clone(),
        shared,
    };
    // build a store for our ThoriumClusters
    let (reader, writer) = reflector::store();
    // watch ThoriumClusters, ignoring changes that only touch their status
    let clusters = watcher(clusters_api, Config::default().any_semantic())
        .default_backoff()
        .reflect(writer)
        .applied_objects()
        .predicate_filter(reconcile_predicate);
    // watch just the metadata of secrets since we only need their names
    let secrets = metadata_watcher(secrets_api, Config::default())
        .default_backoff()
        .touched_objects();
    // map secret changes to the clusters referencing them using our cluster store
    let store = reader.clone();
    let mapper = move |secret: PartialObjectMeta<Secret>| secret_to_clusters(&store, &secret);
    // watch just the metadata of the banner ConfigMap the API mounts
    let banner_config = Config::default().fields(&format!("metadata.name={BANNER_CONFIG_MAP}"));
    let banners = metadata_watcher(cms_api, banner_config)
        .default_backoff()
        .touched_objects();
    // map banner changes to the clusters in the same namespace
    let store = reader.clone();
    let banner_mapper = move |cm: PartialObjectMeta<ConfigMap>| banner_to_clusters(&store, &cm);
    // create the ThoriumCluster controller to watch for resource changes
    Controller::for_stream(clusters, reader)
        .watches_stream(secrets, mapper)
        .watches_stream(banners, banner_mapper)
        .shutdown_on_signal()
        .run(reconcile, error_policy, state.to_context())
        .filter_map(|x| async move { std::result::Result::ok(x) })
        .for_each(|_| futures::future::ready(()))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::clusters::tests::cluster_from_spec;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
    use kube::runtime::watcher::Event;

    /// Build a `ThoriumCluster` in a namespace that references a config secret
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the cluster
    /// * `namespace` - The namespace of the cluster
    /// * `config_secret` - The config secret the cluster references
    fn cluster(name: &str, namespace: &str, config_secret: &str) -> ThoriumCluster {
        // build a cluster that references this secret
        let mut cluster = cluster_from_spec(serde_json::json!({
            "components": {"api": {}},
            "registry": "registry/thorium",
            "config": {},
            "config_secrets": [{"name": config_secret}]
        }));
        cluster.metadata.name = Some(name.to_owned());
        cluster.metadata.namespace = Some(namespace.to_owned());
        cluster
    }

    /// Build the metadata of a secret
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the secret
    /// * `namespace` - The namespace of the secret
    fn secret(name: &str, namespace: &str) -> PartialObjectMeta<Secret> {
        // build just the metadata of a secret
        let mut secret = PartialObjectMeta::<Secret>::default();
        secret.metadata.name = Some(name.to_owned());
        secret.metadata.namespace = Some(namespace.to_owned());
        secret
    }

    /// The banner `ConfigMap` maps to every cluster in its namespace and nothing else does
    #[test]
    fn banner_maps_to_namespace_clusters() {
        // fill a store with clusters in two namespaces
        let (reader, mut writer) = reflector::store::<ThoriumCluster>();
        writer.apply_watcher_event(&Event::Apply(cluster("a", "thorium", "config")));
        writer.apply_watcher_event(&Event::Apply(cluster("b", "dev-thorium", "config")));
        // build the metadata of a ConfigMap
        let cm = |name: &str, namespace: &str| {
            let mut cm = PartialObjectMeta::<ConfigMap>::default();
            cm.metadata.name = Some(name.to_owned());
            cm.metadata.namespace = Some(namespace.to_owned());
            cm
        };
        // the banner maps only to the cluster in its own namespace
        let refs = banner_to_clusters(&reader, &cm("banner", "dev-thorium"));
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].name, "b");
        // any other ConfigMap maps to nothing
        assert_eq!(
            banner_to_clusters(&reader, &cm("tracing-conf", "thorium")),
            Vec::new()
        );
    }

    /// The Elastic CA and a scaler's kube config map to the cluster that mounts them
    #[test]
    fn mounted_secrets_map_to_clusters() {
        // build a cluster mounting an Elastic CA and a kube config
        let mut mounting = cluster_from_spec(serde_json::json!({
            "components": {"api": {}, "scaler": {"service_account": false}},
            "registry": "registry/thorium",
            "config": {},
            "elastic_ca_secret": {"name": "elastic-ca"}
        }));
        mounting.metadata.name = Some("a".to_owned());
        mounting.metadata.namespace = Some("thorium".to_owned());
        let (reader, mut writer) = reflector::store::<ThoriumCluster>();
        writer.apply_watcher_event(&Event::Apply(mounting));
        // both mounted Secrets map to the cluster
        assert_eq!(
            secret_to_clusters(&reader, &secret("elastic-ca", "thorium")).len(),
            1
        );
        assert_eq!(
            secret_to_clusters(&reader, &secret("kube-config", "thorium")).len(),
            1
        );
        // a scaler using its service account doesn't mount the kube config
        let mut service_account = cluster_from_spec(serde_json::json!({
            "components": {"api": {}, "scaler": {"service_account": true}},
            "registry": "registry/thorium",
            "config": {}
        }));
        service_account.metadata.name = Some("b".to_owned());
        service_account.metadata.namespace = Some("dev-thorium".to_owned());
        writer.apply_watcher_event(&Event::Apply(service_account));
        assert_eq!(
            secret_to_clusters(&reader, &secret("kube-config", "dev-thorium")),
            Vec::new()
        );
    }

    /// Status changes don't retrigger a reconcile but spec and deletion changes do
    #[test]
    fn predicate_ignores_status() {
        // hash a cluster
        let mut cluster = cluster("thorium", "thorium", "config");
        cluster.metadata.generation = Some(1);
        let base = reconcile_predicate(&cluster);
        // a status update doesn't change the hash
        cluster.status = Some(crds::ThoriumClusterStatus {
            phase: Some(ClusterPhase::Ready),
            message: Some("ready".to_owned()),
            ..Default::default()
        });
        assert_eq!(reconcile_predicate(&cluster), base);
        // a new spec generation does
        cluster.metadata.generation = Some(2);
        let spec_changed = reconcile_predicate(&cluster);
        assert_ne!(spec_changed, base);
        // adding a finalizer does
        cluster.metadata.finalizers = Some(vec![crds::CRD_NAME.to_owned()]);
        let finalized = reconcile_predicate(&cluster);
        assert_ne!(finalized, spec_changed);
        // starting deletion does
        cluster.metadata.deletion_timestamp = Some(Time(chrono::Utc::now()));
        assert_ne!(reconcile_predicate(&cluster), finalized);
    }

    /// A secret maps to the clusters in its namespace that reference it
    #[test]
    fn secrets_map_to_referencing_clusters() {
        // fill a store with clusters in two namespaces
        let (reader, mut writer) = reflector::store::<ThoriumCluster>();
        writer.apply_watcher_event(&Event::Apply(cluster("a", "thorium", "config")));
        writer.apply_watcher_event(&Event::Apply(cluster("b", "thorium", "other")));
        writer.apply_watcher_event(&Event::Apply(cluster("c", "b-thorium", "config")));
        // a referenced secret maps only to the cluster in its own namespace
        let refs = secret_to_clusters(&reader, &secret("config", "thorium"));
        let names = refs
            .iter()
            .map(|obj| obj.name.as_str())
            .collect::<Vec<&str>>();
        assert_eq!(names, vec!["a"]);
        // the same name in another namespace maps to that namespace's cluster
        let refs = secret_to_clusters(&reader, &secret("config", "b-thorium"));
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].namespace.as_deref(), Some("b-thorium"));
        // an unreferenced secret maps to nothing
        assert_eq!(
            secret_to_clusters(&reader, &secret("unused", "thorium")).len(),
            0
        );
        // a secret without a namespace maps to nothing
        let mut no_namespace = secret("config", "thorium");
        no_namespace.metadata.namespace = None;
        assert_eq!(secret_to_clusters(&reader, &no_namespace).len(), 0);
    }
}
