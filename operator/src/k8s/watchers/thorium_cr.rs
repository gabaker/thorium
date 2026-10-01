//! The watcher for changes in Thorium CRs

use futures::StreamExt;
use k8s_openapi::api::core::v1::Secret;
use kube::api::{ListParams, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{Event as Finalizer, finalizer};
use kube::runtime::reflector::{self, ObjectRef};
use kube::runtime::watcher::{Config, watcher};
use kube::runtime::{Controller, WatchStreamExt};
use kube::{Api, Client};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;
use thorium::Error;

use crate::args::OperateCluster;
use crate::k8s::clusters::ClusterMeta;
use crate::k8s::controller::SharedInfo;
use crate::k8s::crds::{self, ThoriumCluster};
use crate::k8s::operate;

/// Controller state including kubeapi client and url
#[derive(Clone)]
pub struct State {
    /// kube API client
    client: Client,
    /// ingress route for Thorium API
    url: Option<String>,
    // The shared thorium operator info
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
/// This lets a `ThoriumCluster` whose config can no longer be resolved still be deleted.
///
/// # Arguments
///
/// * `cluster` - The `ThoriumCluster` to remove the finalizer from
/// * `client` - The kube client to patch the `ThoriumCluster` with
async fn remove_finalizer(cluster: &ThoriumCluster, client: &Client) -> Result<Action, Error> {
    // get this cluster's name and namespace
    let (Some(name), Some(namespace)) = (&cluster.metadata.name, &cluster.metadata.namespace)
    else {
        return Err(Error::new("ThoriumCluster is missing a name or namespace"));
    };
    // keep every finalizer except ours
    let finalizers = cluster
        .metadata
        .finalizers
        .iter()
        .flatten()
        .filter(|finalizer| finalizer.as_str() != crds::CRD_NAME)
        .cloned()
        .collect::<Vec<String>>();
    // patch the finalizers on this cluster
    let patch = serde_json::json!({"metadata": {"finalizers": finalizers}});
    let clusters_api: Api<ThoriumCluster> = Api::namespaced(client.clone(), namespace);
    clusters_api
        .patch(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .map_err(|error| {
            Error::new(format!(
                "Failed to remove finalizer from {namespace}/{name}: {error}"
            ))
        })?;
    Ok(Action::await_change())
}

/// Reconcile changes to ThoriumCluster
///
/// Arguments
///
/// * `cluster` - Thorium cluster being changed
/// * `state` - Controller context including client instance and optional URL
pub async fn reconcile(cluster: Arc<ThoriumCluster>, state: Arc<State>) -> Result<Action, Error> {
    // build cluster metadata
    let meta = match ClusterMeta::new(&cluster, &state.client).await {
        Ok(meta) => meta,
        Err(error) => {
            // a cluster being deleted with an unresolvable config is released without cleanup
            if cluster.metadata.deletion_timestamp.is_some() {
                println!("Skipping cleanup of ThoriumCluster with an invalid config: {error}");
                return remove_finalizer(&cluster, &state.client).await;
            }
            // record why we couldn't build this cluster's config
            crds::set_status(&state.client, &cluster, "Error", Some(error.to_string())).await;
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
                Finalizer::Cleanup(_cluster) => operate::cleanup(&meta).await,
            }
        },
    )
    .await
    .map_err(|e| Error::new(format!("Finalizer error: {e}")));
    // record any failure in this cluster's status
    if let Err(error) = &result {
        crds::set_status(&state.client, &cluster, "Error", Some(error.to_string())).await;
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

/// Map a labeled Secret to the `ThoriumCluster` it configures
///
/// # Arguments
///
/// * `secret` - The Secret that changed
// the controller hands mappers an owned Secret
#[allow(clippy::needless_pass_by_value)]
fn secret_to_cluster(secret: Secret) -> Option<ObjectRef<ThoriumCluster>> {
    // get the cluster this secret belongs to
    let cluster = secret
        .metadata
        .labels
        .as_ref()?
        .get(crds::CLUSTER_SECRET_LABEL)?;
    // get the namespace of this secret
    let namespace = secret.metadata.namespace.as_ref()?;
    // reconcile the cluster in the secret's namespace
    Some(ObjectRef::new(cluster).within(namespace))
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
    // only watch secrets that are linked to a ThoriumCluster
    let secrets_config = Config::default().labels(crds::CLUSTER_SECRET_LABEL);
    // create the ThoriumCluster controller to watch for resource changes
    Controller::for_stream(clusters, reader)
        .watches(secrets_api, secrets_config, secret_to_cluster)
        .shutdown_on_signal()
        .run(reconcile, error_policy, state.to_context())
        .filter_map(|x| async move { std::result::Result::ok(x) })
        .for_each(|_| futures::future::ready(()))
        .await;
}
