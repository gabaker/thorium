//! The watcher for changes in Thorium CRs

use futures::StreamExt;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{ConfigMap, Secret};
use kube::api::{ListParams, Patch, PatchParams};
use kube::core::PartialObjectMeta;
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{self, Event as Finalizer, finalizer};
use kube::runtime::reflector::{self, ObjectRef, Store};
use kube::runtime::watcher::{self, Config, metadata_watcher, watcher};
use kube::runtime::{Controller, WatchStreamExt};
use kube::{Api, Client};
use std::collections::{HashMap, HashSet};
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
use crate::upgrades::{self, Gate};

/// How long in seconds to wait before rechecking a `ThoriumCluster` left alone because another
/// one is already deployed, which lets it take over once the deployed one is deleted
const DUPLICATE_REQUEUE_SECS: u64 = 300;

/// The names (and `app` labels) of the component Deployments the operator deploys, whose
/// availability is reflected in a `ThoriumCluster`'s status
const COMPONENT_DEPLOYMENTS: [&str; 5] = [
    "api",
    "scaler",
    "baremetal-scaler",
    "event-handler",
    "search-streamer",
];

/// The Deployment conditions whose status describes a component's availability
const AVAILABILITY_CONDITIONS: [&str; 3] = ["Available", "Progressing", "ReplicaFailure"];

/// Controller state including kubeapi client and url
#[derive(Clone)]
pub struct State {
    /// kube API client
    client: Client,
    /// The name of the kube context this controller's k8s cluster is reached through
    context: String,
    /// The namespace the operator watches `ThoriumCluster`s in (all namespaces if unset)
    namespace: Option<String>,
    /// An override for the url the operator reaches the Thorium API at
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
///
/// # Arguments
///
/// * `_cluster` - The `ThoriumCluster` whose reconcile failed
/// * `error` - The error the reconcile failed with
/// * `_state` - The controller state
pub fn error_policy(_cluster: Arc<ThoriumCluster>, error: &Error, _state: Arc<State>) -> Action {
    // log the failure and when the cluster is retried
    println!("Controller error:\n\t{error}");
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
/// * `context` - The name of the kube context the cluster is in
/// * `shared` - Data shared across watchers
/// * `error` - The error from resolving this cluster's config
async fn release_unresolvable(
    cluster: &ThoriumCluster,
    client: &Client,
    context: &str,
    shared: &SharedInfo,
    error: Error,
) -> Result<Action, Error> {
    // retry anything that might resolve on its own
    if !is_permanent_config_error(&error) {
        return Err(error);
    }
    // log why the node cleanup is skipped
    println!(
        "ThoriumCluster config can't be resolved, skipping node cleanup before deletion: {error}"
    );
    // stop the watchers from acting on this cluster before cleaning up
    let (name, namespace) = cluster_name_and_namespace(cluster)?;
    shared
        .info
        .pin()
        .remove(&SharedInfo::key(context, &namespace, &name));
    // remove the resources that don't depend on this cluster's config
    operate::cleanup_namespace(cluster, client).await?;
    // let k8s finish deleting this cluster
    remove_finalizer(cluster, client).await
}

/// Stop the node and MCP watchers from acting on a cluster the operator applied earlier
///
/// The shared info is a snapshot of the last successful apply, so a cluster that is later held
/// by the upgrade gate or whose config stops resolving would otherwise keep having its nodes
/// provisioned and labelled (and its api pods labelled) from that old snapshot. The next
/// successful apply adds the cluster back.
///
/// # Arguments
///
/// * `state` - The controller state
/// * `cluster` - The cluster to stop acting on
fn forget(state: &State, cluster: &ThoriumCluster) {
    // a cluster without a name or namespace was never added to the shared info
    if let Ok((name, namespace)) = cluster_name_and_namespace(cluster) {
        state
            .shared
            .info
            .pin()
            .remove(&SharedInfo::key(&state.context, &namespace, &name));
    }
}

/// Sort key ordering `ThoriumCluster`s from oldest to newest
///
/// Clusters created in the same second are ordered by namespace and then name so every
/// reconcile picks the same one. A cluster without a creation time sorts last.
///
/// # Arguments
///
/// * `cluster` - The cluster to build a sort key for
fn age_key(
    cluster: &ThoriumCluster,
) -> (
    bool,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<&str>,
    Option<&str>,
) {
    // get when this cluster was created
    let created = cluster
        .metadata
        .creation_timestamp
        .as_ref()
        .map(|time| time.0);
    (
        created.is_none(),
        created,
        cluster.metadata.namespace.as_deref(),
        cluster.metadata.name.as_deref(),
    )
}

/// Pick the `ThoriumCluster` the operator manages: the oldest one it can see
///
/// Thorium supports one deployment per Kubernetes cluster, since its node labels, node
/// provisioning, and cluster-scoped resources are shared. This only sees the `ThoriumCluster`s
/// this operator can list, so operators each scoped to their own namespace (`--namespace`)
/// can't enforce it between each other; running more than one operator per Kubernetes cluster
/// is unsupported, and the chart's install guard is what stops a second Helm install in
/// another namespace.
///
/// # Arguments
///
/// * `clusters` - Every `ThoriumCluster` the operator can see
fn pick_managed(clusters: &[ThoriumCluster]) -> Option<&ThoriumCluster> {
    // the oldest cluster wins
    clusters.iter().min_by(|a, b| age_key(a).cmp(&age_key(b)))
}

/// Describe why a `ThoriumCluster` is left alone because another one is already deployed
///
/// # Arguments
///
/// * `managed` - The `namespace/name` of the cluster the operator manages
fn duplicate_message(managed: &str) -> String {
    format!(
        "Thorium supports one deployment per Kubernetes cluster; ThoriumCluster {managed} is \
         already deployed; namespace prefixes only rename namespaces. The operator changes \
         nothing for this ThoriumCluster; delete it or delete {managed} first (see \
         \"Namespaces and the namespace prefix\" in the Thorium docs)"
    )
}

/// What listing every `ThoriumCluster` found out about the one being reconciled
struct Listing {
    /// The `namespace/name` and namespace of the managed cluster when it is another one
    managed: Option<(String, String)>,
    /// The cluster being reconciled as the API server holds it now, if it still exists
    latest: Option<ThoriumCluster>,
}

/// List every `ThoriumCluster` to find the one the operator manages and the latest copy of the
/// one being reconciled
///
/// # Arguments
///
/// * `state` - The controller state
/// * `cluster` - The cluster being reconciled
async fn list_clusters(state: &State, cluster: &ThoriumCluster) -> Result<Listing, Error> {
    // list every cluster the operator can see
    let api: Api<ThoriumCluster> = super::scoped_api(&state.client, state.namespace.as_deref());
    let clusters = api
        .list(&ListParams::default())
        .await
        .map_err(|error| Error::new(format!("Failed to list ThoriumClusters: {error}")))?;
    // name the managed cluster unless it is this one
    let (name, namespace) = cluster_name_and_namespace(cluster)?;
    let is_this = |other: &ThoriumCluster| {
        other.metadata.name.as_deref() == Some(name.as_str())
            && other.metadata.namespace.as_deref() == Some(namespace.as_str())
    };
    let managed = pick_managed(&clusters.items)
        .filter(|managed| !is_this(managed))
        .and_then(|managed| {
            // get the managed cluster's name and namespace
            let managed_name = managed.metadata.name.as_deref()?;
            let managed_namespace = managed.metadata.namespace.as_deref()?;
            Some((
                format!("{managed_namespace}/{managed_name}"),
                managed_namespace.to_owned(),
            ))
        });
    // keep the listed copy of this cluster, which has every status this operator wrote
    let latest = clusters.items.into_iter().find(|other| is_this(other));
    Ok(Listing { managed, latest })
}

/// Release a `ThoriumCluster` that the operator doesn't manage when it is deleted
///
/// Node labels and node provision pods belong to the managed cluster, so they are never
/// cleaned up here. Resources in this cluster's own namespace (left by an operator that still
/// managed several clusters) are only removed when the managed cluster lives elsewhere.
///
/// # Arguments
///
/// * `cluster` - The `ThoriumCluster` being deleted
/// * `client` - The kube client to clean up with
/// * `managed_namespace` - The namespace of the cluster the operator manages
async fn release_duplicate(
    cluster: &ThoriumCluster,
    client: &Client,
    managed_namespace: &str,
) -> Result<Action, Error> {
    // a cluster that never got our finalizer has nothing to clean up
    let finalized = cluster
        .metadata
        .finalizers
        .iter()
        .flatten()
        .any(|finalizer| finalizer == crds::CRD_NAME);
    if !finalized {
        return Ok(Action::await_change());
    }
    // only clean up a namespace the managed cluster doesn't share
    let (_, namespace) = cluster_name_and_namespace(cluster)?;
    if namespace != managed_namespace {
        operate::cleanup_namespace(cluster, client).await?;
    }
    // let k8s finish deleting this cluster
    remove_finalizer(cluster, client).await
}

/// Leave a `ThoriumCluster` alone unless it is the one the operator manages
///
/// Returns the outcome of the reconcile when the cluster isn't managed (or whether it is
/// couldn't be told), or None when it should be reconciled normally.
///
/// # Arguments
///
/// * `state` - The controller state
/// * `cluster` - The cluster being reconciled
/// * `tracker` - The cluster's status as last written by this reconcile
/// * `listing` - The other cluster listing found to be managed, or why none could be listed
async fn skip_unmanaged(
    state: &State,
    cluster: &ThoriumCluster,
    tracker: &crds::StatusTracker,
    listing: Result<Option<(String, String)>, Error>,
) -> Option<Result<Action, Error>> {
    match listing {
        Ok(None) => None,
        // a duplicate being deleted is released without touching the deployed cluster
        Ok(Some((_, managed_namespace))) if cluster.metadata.deletion_timestamp.is_some() => {
            Some(release_duplicate(cluster, &state.client, &managed_namespace).await)
        }
        Ok(Some((managed, _))) => {
            // record why nothing is deployed for this cluster
            let message = duplicate_message(&managed);
            eprintln!("Error: {message}");
            crds::set_status(&state.client, tracker, ClusterPhase::Error, Some(message)).await;
            Some(Ok(Action::requeue(Duration::from_secs(
                DUPLICATE_REQUEUE_SECS,
            ))))
        }
        Err(error) => {
            // record why we couldn't tell whether this cluster is the deployed one
            crds::set_status(
                &state.client,
                tracker,
                ClusterPhase::Error,
                Some(crds::error_message(&error)),
            )
            .await;
            Some(Err(error))
        }
    }
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
    // list every cluster to find the one the operator manages
    let listing = list_clusters(&state, &cluster).await;
    // work from the listed copy of this cluster since the watch cache may not have the status
    // the last reconcile wrote yet, and comparing against an older status would rewrite an
    // unchanged one and move its transition time
    let (cluster, listing) = match listing {
        Ok(Listing { managed, latest }) => (latest.map_or(cluster, Arc::new), Ok(managed)),
        Err(error) => (cluster, Err(error)),
    };
    // every status update in this reconcile goes through one tracker so each is compared
    // against the status last written rather than the copy this reconcile started from
    let tracker = Arc::new(crds::StatusTracker::new(cluster.clone()));
    // leave every cluster but the one already deployed alone
    if let Some(outcome) = skip_unmanaged(&state, &cluster, &tracker, listing).await {
        return outcome;
    }
    // decide whether this cluster must be upgraded first, which needs only the cluster itself
    // so an unconverted or half-converted cluster is caught before its config is resolved
    let pending = if cluster.metadata.deletion_timestamp.is_none() {
        match upgrades::gate(&state.client, &tracker).await {
            Ok(Gate::Current) => None,
            Ok(Gate::Upgrade(pending)) => {
                // the watchers wait for the apply that follows the upgrade
                forget(&state, &cluster);
                Some(pending)
            }
            Ok(Gate::Hold(action)) => {
                // a held cluster's nodes and pods are left alone until it is applied again
                forget(&state, &cluster);
                return Ok(action);
            }
            Err(error) => {
                // a cluster that couldn't be gated isn't known to be current
                forget(&state, &cluster);
                // record why the cluster couldn't be gated
                crds::set_status(
                    &state.client,
                    &tracker,
                    ClusterPhase::Error,
                    Some(crds::error_message(&error)),
                )
                .await;
                return Err(error);
            }
        }
    } else {
        None
    };
    // build cluster metadata
    let meta = match ClusterMeta::new(&tracker, &state.client, &state.context).await {
        Ok(meta) => meta,
        Err(error) => {
            // a cluster being deleted with a permanently broken config is released
            if cluster.metadata.deletion_timestamp.is_some() {
                return release_unresolvable(
                    &cluster,
                    &state.client,
                    &state.context,
                    &state.shared,
                    error,
                )
                .await;
            }
            // the watchers can't act on a cluster whose config doesn't resolve
            forget(&state, &cluster);
            // record why we couldn't build this cluster's config
            crds::set_status(
                &state.client,
                &tracker,
                ClusterPhase::Error,
                Some(crds::error_message(&error)),
            )
            .await;
            return Err(error);
        }
    };
    let clusters_api: Api<ThoriumCluster> = Api::namespaced(meta.client.clone(), &meta.namespace);
    // log which cluster we are reconciling
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
                    // bring the cluster to its target revision before reconciling it
                    if let Some(pending) = pending
                        && let Some(action) = upgrades::run(&meta, pending).await?
                    {
                        return Ok(action);
                    }
                    // box the apply since its future is too large to keep on the stack
                    Box::pin(operate::apply(&meta, state.url.clone(), &state.shared)).await
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
            &tracker,
            ClusterPhase::Error,
            Some(crds::error_message(error)),
        )
        .await;
    }
    result
}

/// Hash the parts of a `ThoriumCluster` that should trigger a reconcile
///
/// Status updates are excluded so the operator's own status patches don't retrigger it. The
/// inline config annotation is included since setting it releases a held cluster.
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
    // hash the inline config opt out, which isn't part of the spec generation
    cluster
        .metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(upgrades::ALLOW_INLINE_CONFIG))
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

/// Hash the parts of a component Deployment that describe its availability
///
/// Only status fields the Deployment controller writes are hashed (plus the uid, so a
/// recreated Deployment counts as changed), so the operator's own spec and annotation patches
/// never trigger a reconcile by themselves; the rollout they start does, and it converges.
///
/// # Arguments
///
/// * `deployment` - The Deployment to hash
fn availability_hash(deployment: &Deployment) -> u64 {
    // build a hasher
    let mut hasher = DefaultHasher::new();
    // a recreated Deployment is a new object
    deployment.metadata.uid.hash(&mut hasher);
    // hash the replica counts and the spec generation the controller has seen
    let status = deployment.status.as_ref();
    status
        .and_then(|status| status.observed_generation)
        .hash(&mut hasher);
    status.and_then(|status| status.replicas).hash(&mut hasher);
    status
        .and_then(|status| status.ready_replicas)
        .hash(&mut hasher);
    status
        .and_then(|status| status.available_replicas)
        .hash(&mut hasher);
    status
        .and_then(|status| status.updated_replicas)
        .hash(&mut hasher);
    status
        .and_then(|status| status.unavailable_replicas)
        .hash(&mut hasher);
    // hash the status of the availability conditions but not their timestamps or messages
    for kind in AVAILABILITY_CONDITIONS {
        status
            .and_then(|status| status.conditions.as_ref())
            .and_then(|conditions| conditions.iter().find(|condition| condition.type_ == kind))
            .map(|condition| condition.status.as_str())
            .hash(&mut hasher);
    }
    hasher.finish()
}

/// Passes on only the component Deployment events that change a component's availability
///
/// Unlike a plain predicate filter this also passes on deletions, including Deployments that
/// disappeared while the watch was relisting, so a deleted component is redeployed.
#[derive(Default)]
struct AvailabilityFilter {
    /// The availability hash of every Deployment last passed on
    seen: HashMap<ObjectRef<Deployment>, u64>,
    /// The Deployments listed so far while the watch relists, if it is relisting
    relisted: Option<HashSet<ObjectRef<Deployment>>>,
}

impl AvailabilityFilter {
    /// Get the Deployments from a watch event whose availability changed
    ///
    /// # Arguments
    ///
    /// * `event` - The watch event for component Deployments
    fn changes(&mut self, event: watcher::Event<Deployment>) -> Vec<Deployment> {
        match event {
            // pass on an added or changed Deployment only when its availability changed
            watcher::Event::Apply(deployment) | watcher::Event::InitApply(deployment) => {
                // get this Deployment's key and hash
                let key = ObjectRef::from_obj(&deployment);
                let hash = availability_hash(&deployment);
                // remember that the relist still holds this Deployment
                if let Some(relisted) = &mut self.relisted {
                    relisted.insert(key.clone());
                }
                // skip a Deployment whose availability is what we last passed on
                if self.seen.insert(key, hash) == Some(hash) {
                    Vec::new()
                } else {
                    vec![deployment]
                }
            }
            // always pass on a deleted Deployment
            watcher::Event::Delete(deployment) => {
                // forget the deleted Deployment so a recreated one is passed on
                self.seen.remove(&ObjectRef::from_obj(&deployment));
                vec![deployment]
            }
            // start tracking which Deployments the relist holds
            watcher::Event::Init => {
                self.relisted = Some(HashSet::new());
                Vec::new()
            }
            // a Deployment the relist no longer holds was deleted while we weren't watching
            watcher::Event::InitDone => {
                // stop tracking the relist
                let relisted = self.relisted.take().unwrap_or_default();
                // find the Deployments we passed on before that the relist didn't hold
                let gone = self
                    .seen
                    .keys()
                    .filter(|key| !relisted.contains(*key))
                    .cloned()
                    .collect::<Vec<ObjectRef<Deployment>>>();
                gone.into_iter()
                    .map(|key| {
                        // forget the gone Deployment and pass on just its name and namespace
                        self.seen.remove(&key);
                        let mut deployment = Deployment::default();
                        deployment.metadata.name = Some(key.name);
                        deployment.metadata.namespace = key.namespace;
                        deployment
                    })
                    .collect()
            }
        }
    }
}

/// Map a component Deployment to every `ThoriumCluster` in its namespace that deploys it
///
/// # Arguments
///
/// * `clusters` - The store of `ThoriumCluster`s we watch
/// * `deployment` - The Deployment whose availability changed
fn deployment_to_clusters(
    clusters: &Store<ThoriumCluster>,
    deployment: &Deployment,
) -> Vec<ObjectRef<ThoriumCluster>> {
    // get the name and namespace of this deployment
    let (Some(name), Some(namespace)) = (&deployment.metadata.name, &deployment.metadata.namespace)
    else {
        return Vec::new();
    };
    // find the clusters in this namespace whose spec has this component
    clusters
        .state()
        .iter()
        .filter(|cluster| cluster.metadata.namespace.as_ref() == Some(namespace))
        .filter(|cluster| cluster.list_component_names().contains(name))
        .map(|cluster| ObjectRef::from_obj(cluster.as_ref()))
        .collect()
}

/// Watch our `ThoriumCluster`s for any changes
///
/// # Arguments
///
/// * `client` - The kube client to watch with
/// * `context` - The name of the kube context `client` reaches its k8s cluster through
/// * `args` - The command line args passed to the operator
/// * `shared` - Data shared across watchers
pub async fn start(client: Client, context: String, args: OperateCluster, shared: Arc<SharedInfo>) {
    // get the ThoriumCluster api for the namespaces we watch, which the operator already
    // checked it can list before starting its watchers
    let clusters_api: Api<ThoriumCluster> = super::scoped_api(&client, args.namespace.as_deref());
    // get the Secret api for the namespaces we watch
    let secrets_api: Api<Secret> = super::scoped_api(&client, args.namespace.as_deref());
    // get the ConfigMap api for the namespaces we watch
    let cms_api: Api<ConfigMap> = super::scoped_api(&client, args.namespace.as_deref());
    // get the Deployment api for the namespaces we watch
    let deploys_api: Api<Deployment> = super::scoped_api(&client, args.namespace.as_deref());
    // build our controller state
    let state = State {
        client: client.clone(),
        context,
        namespace: args.namespace.clone(),
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
    // watch the component Deployments, passing on only changes to their availability so a
    // component that becomes unavailable (such as when its node goes down) or recovers
    // updates the cluster's status without waiting for the daily reconcile
    let component_config =
        Config::default().labels(&format!("app in ({})", COMPONENT_DEPLOYMENTS.join(",")));
    let mut availability = AvailabilityFilter::default();
    let deployments = watcher(deploys_api, component_config)
        .default_backoff()
        .flat_map(move |event| {
            // pass on watch errors and the Deployments whose availability changed
            let changed = match event {
                Ok(event) => availability.changes(event).into_iter().map(Ok).collect(),
                Err(error) => vec![Err(error)],
            };
            futures::stream::iter(changed)
        });
    // map component changes to the clusters deploying them
    let store = reader.clone();
    let deployment_mapper =
        move |deployment: Deployment| deployment_to_clusters(&store, &deployment);
    // create the ThoriumCluster controller to watch for resource changes, draining its results
    // since reconcile and error_policy already log and record every outcome
    Controller::for_stream(clusters, reader)
        .watches_stream(secrets, mapper)
        .watches_stream(banners, banner_mapper)
        .watches_stream(deployments, deployment_mapper)
        .shutdown_on_signal()
        .run(reconcile, error_policy, state.to_context())
        .for_each(|_| futures::future::ready(()))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::clusters::tests::{FakeKube, TEST_CONTEXT, cluster_from_spec};
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
        let deleting = reconcile_predicate(&cluster);
        assert_ne!(deleting, finalized);
        // unrelated annotations don't
        cluster.metadata.annotations = Some(std::collections::BTreeMap::from([(
            "note".to_owned(),
            "x".to_owned(),
        )]));
        assert_eq!(reconcile_predicate(&cluster), deleting);
        // setting the inline config opt out does, since it releases a held cluster
        cluster
            .metadata
            .annotations
            .get_or_insert_default()
            .insert(upgrades::ALLOW_INLINE_CONFIG.to_owned(), "true".to_owned());
        let allowed = reconcile_predicate(&cluster);
        assert_ne!(allowed, deleting);
        // and so does changing its value
        cluster
            .metadata
            .annotations
            .get_or_insert_default()
            .insert(upgrades::ALLOW_INLINE_CONFIG.to_owned(), "false".to_owned());
        assert_ne!(reconcile_predicate(&cluster), allowed);
    }

    /// Build a cluster created at a time
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the cluster
    /// * `namespace` - The namespace of the cluster
    /// * `created` - When the cluster was created (RFC3339)
    fn created_cluster(name: &str, namespace: &str, created: &str) -> ThoriumCluster {
        // build a cluster and stamp its creation time
        let mut cluster = cluster(name, namespace, "config");
        let created = chrono::DateTime::parse_from_rfc3339(created)
            .expect("valid time")
            .with_timezone(&chrono::Utc);
        cluster.metadata.creation_timestamp = Some(Time(created));
        cluster
    }

    /// The oldest cluster is managed, with ties broken by namespace and then name
    #[test]
    fn oldest_cluster_is_managed() {
        // a newer cluster listed before an older one
        let newer = created_cluster("thorium", "a-thorium", "2026-10-02T00:00:00Z");
        let older = created_cluster("thorium", "z-thorium", "2026-10-01T00:00:00Z");
        let clusters = vec![newer.clone(), older];
        let managed = pick_managed(&clusters).expect("a cluster");
        assert_eq!(managed.metadata.namespace.as_deref(), Some("z-thorium"));
        // clusters created in the same second are ordered by namespace and then name
        let tied = vec![
            created_cluster("b", "thorium", "2026-10-02T00:00:00Z"),
            created_cluster("a", "thorium", "2026-10-02T00:00:00Z"),
            newer,
        ];
        let managed = pick_managed(&tied).expect("a cluster");
        assert_eq!(managed.metadata.namespace.as_deref(), Some("a-thorium"));
        let same_namespace = &tied[..2];
        let managed = pick_managed(same_namespace).expect("a cluster");
        assert_eq!(managed.metadata.name.as_deref(), Some("a"));
        // a cluster without a creation time never wins over one with
        let mut unstamped = cluster("a", "a", "config");
        unstamped.metadata.creation_timestamp = None;
        let clusters = vec![unstamped, created_cluster("z", "z", "2026-10-02T00:00:00Z")];
        assert_eq!(
            pick_managed(&clusters).and_then(|managed| managed.metadata.name.as_deref()),
            Some("z")
        );
        // nothing is managed without clusters
        assert!(pick_managed(&[]).is_none());
    }

    /// Build controller state against a fake kube API
    ///
    /// # Arguments
    ///
    /// * `fake` - The fake kube API the state's client talks to
    fn fake_state(fake: &FakeKube) -> Arc<State> {
        Arc::new(State {
            client: fake.client(),
            context: TEST_CONTEXT.to_owned(),
            namespace: None,
            url: None,
            shared: Arc::new(SharedInfo::default()),
        })
    }

    /// Build controller state whose shared info holds a cluster applied earlier, along with
    /// an unrelated cluster in another namespace
    ///
    /// # Arguments
    ///
    /// * `fake` - The fake kube API the state's client talks to
    /// * `cluster` - The cluster an earlier apply added to the shared info
    async fn applied_state(fake: &FakeKube, cluster: &ThoriumCluster) -> Arc<State> {
        // add the cluster like an earlier apply in this operator process did
        let client = fake.client();
        let shared = crate::k8s::controller::tests::shared_with(
            crate::k8s::clusters::tests::meta_for(cluster.clone(), &client),
        )
        .await;
        // add an unrelated cluster that must be kept
        let mut other = cluster.clone();
        other.metadata.namespace = Some("other".to_owned());
        let other = crate::k8s::controller::tests::shared_with(
            crate::k8s::clusters::tests::meta_for(other, &client),
        )
        .await;
        for (key, info) in &other.info.pin() {
            shared.info.pin().insert(key.clone(), info.clone());
        }
        Arc::new(State {
            client,
            context: TEST_CONTEXT.to_owned(),
            namespace: None,
            url: None,
            shared: Arc::new(shared),
        })
    }

    /// Get the keys of the clusters left in the shared info, sorted
    ///
    /// # Arguments
    ///
    /// * `state` - The controller state to check
    fn shared_keys(state: &State) -> Vec<String> {
        let mut keys = state
            .shared
            .info
            .pin()
            .keys()
            .cloned()
            .collect::<Vec<String>>();
        keys.sort();
        keys
    }

    /// The shared info key of the unrelated cluster [`applied_state`] adds
    fn other_key() -> Vec<String> {
        vec![SharedInfo::key(TEST_CONTEXT, "other", "thorium")]
    }

    /// Build a fake kube API listing clusters and accepting patches to the newer one
    ///
    /// # Arguments
    ///
    /// * `clusters` - The clusters every namespace holds
    /// * `duplicate` - The cluster whose status and finalizers are patched
    fn fake_listing(clusters: &[ThoriumCluster], duplicate: &ThoriumCluster) -> FakeKube {
        // get where the duplicate lives
        let (name, namespace) = cluster_name_and_namespace(duplicate).expect("placed");
        let path = format!("/apis/sandia.gov/v1/namespaces/{namespace}/thoriumclusters/{name}");
        let body = serde_json::to_value(duplicate).expect("cluster serializes");
        FakeKube::default()
            .route(
                "GET",
                "/apis/sandia.gov/v1/thoriumclusters",
                200,
                serde_json::json!({
                    "apiVersion": "sandia.gov/v1",
                    "kind": "ThoriumClusterList",
                    "metadata": {},
                    "items": clusters
                }),
            )
            .route("PATCH", &format!("{path}/status"), 200, body.clone())
            .route("PATCH", &path, 200, body)
    }

    /// A second cluster is marked as errored and nothing is deployed for it
    #[tokio::test]
    async fn second_cluster_left_alone() {
        // a deployed cluster and a newer one in another namespace
        let managed = created_cluster("thorium", "thorium", "2026-10-01T00:00:00Z");
        let duplicate = created_cluster("thorium", "dev-thorium", "2026-10-02T00:00:00Z");
        let fake = fake_listing(&[managed, duplicate.clone()], &duplicate);
        // the newer cluster is rechecked later instead of reconciled
        let action = Box::pin(reconcile(Arc::new(duplicate), fake_state(&fake)))
            .await
            .expect("reconcile");
        assert_eq!(
            action,
            Action::requeue(Duration::from_secs(DUPLICATE_REQUEUE_SECS))
        );
        // the only write is its error status naming the deployed cluster
        let writes = fake.writes();
        assert_eq!(writes.len(), 1, "{writes:?}");
        assert!(
            writes[0]
                .path
                .ends_with("/dev-thorium/thoriumclusters/thorium/status")
        );
        let status = &writes[0].body["status"];
        assert_eq!(status["phase"], "Error");
        let message = status["message"].as_str().expect("message");
        assert!(message.contains("one deployment per Kubernetes cluster"));
        assert!(message.contains("ThoriumCluster thorium/thorium is already deployed"));
        assert!(message.contains("namespace prefixes only rename namespaces"));
        // nothing was read from the newer cluster's namespace either
        assert!(
            fake.requests()
                .iter()
                .all(|request| !request.path.contains("/namespaces/dev-thorium/configmaps"))
        );
    }

    /// A reconcile compares against the listed status rather than its possibly stale copy, so a
    /// status the last reconcile already wrote isn't rewritten
    #[tokio::test]
    async fn reconcile_compares_against_listed_status() {
        // a deployed cluster and a newer one whose listed status already reports the duplicate
        let managed = created_cluster("thorium", "thorium", "2026-10-01T00:00:00Z");
        let stale = created_cluster("thorium", "dev-thorium", "2026-10-02T00:00:00Z");
        let mut listed = stale.clone();
        listed.status = Some(crds::ThoriumClusterStatus {
            phase: Some(ClusterPhase::Error),
            message: Some(duplicate_message("thorium/thorium")),
            last_transition: Some("2026-10-02T00:00:00Z".to_owned()),
            ..Default::default()
        });
        let fake = fake_listing(&[managed.clone(), listed], &stale);
        // reconciling the copy without a status writes nothing
        Box::pin(reconcile(Arc::new(stale.clone()), fake_state(&fake)))
            .await
            .expect("reconcile");
        assert_eq!(fake.writes().len(), 0, "{:?}", fake.writes());
        // a listed status that differs is still replaced
        let fake = fake_listing(&[managed, stale.clone()], &stale);
        Box::pin(reconcile(Arc::new(stale), fake_state(&fake)))
            .await
            .expect("reconcile");
        assert_eq!(fake.writes().len(), 1, "{:?}", fake.writes());
    }

    /// Mark a cluster as being deleted with this operator's finalizer
    ///
    /// # Arguments
    ///
    /// * `cluster` - The cluster to mark
    fn deleting(mut cluster: ThoriumCluster) -> ThoriumCluster {
        cluster.metadata.finalizers = Some(vec![crds::CRD_NAME.to_owned()]);
        cluster.metadata.deletion_timestamp = Some(Time(chrono::Utc::now()));
        cluster
    }

    /// Deleting a second cluster in the deployed cluster's namespace only releases its
    /// finalizer and never cleans up the deployed cluster's resources or node labels
    #[tokio::test]
    async fn deleting_duplicate_in_same_namespace_keeps_resources() {
        // a deployed cluster and a newer one being deleted in the same namespace
        let managed = created_cluster("thorium", "thorium", "2026-10-01T00:00:00Z");
        let duplicate = deleting(created_cluster("second", "thorium", "2026-10-02T00:00:00Z"));
        let fake = fake_listing(&[managed, duplicate.clone()], &duplicate);
        // the duplicate is released
        Box::pin(reconcile(Arc::new(duplicate), fake_state(&fake)))
            .await
            .expect("reconcile");
        // the only write removes the finalizer from the duplicate
        let writes = fake.writes();
        assert_eq!(writes.len(), 1, "{writes:?}");
        assert_eq!(writes[0].method, "PATCH");
        assert_eq!(
            writes[0].path,
            "/apis/sandia.gov/v1/namespaces/thorium/thoriumclusters/second"
        );
        assert_eq!(writes[0].body[1]["op"], "remove");
    }

    /// Deleting a second cluster in its own namespace cleans up only that namespace and never
    /// touches nodes or the deployed cluster's namespace
    #[tokio::test]
    async fn deleting_duplicate_elsewhere_cleans_own_namespace() {
        // a deployed cluster and a newer one being deleted in another namespace
        let managed = created_cluster("thorium", "thorium", "2026-10-01T00:00:00Z");
        let duplicate = deleting(created_cluster(
            "thorium",
            "dev-thorium",
            "2026-10-02T00:00:00Z",
        ));
        let fake = fake_listing(&[managed, duplicate.clone()], &duplicate).route(
            "DELETE",
            "/api/v1/namespaces/dev-thorium/pods",
            200,
            serde_json::json!({"apiVersion": "v1", "kind": "PodList", "metadata": {}, "items": []}),
        );
        // the duplicate is released
        Box::pin(reconcile(Arc::new(duplicate), fake_state(&fake)))
            .await
            .expect("reconcile");
        // every write stays in the duplicate's namespace and none touch nodes
        let writes = fake.writes();
        assert!(writes.iter().any(|request| request.method == "DELETE"));
        for request in &writes {
            assert!(
                request.path.contains("/namespaces/dev-thorium/"),
                "{request:?}"
            );
        }
        // the finalizer is removed last
        let last = writes.last().expect("a write");
        assert_eq!(
            last.path,
            "/apis/sandia.gov/v1/namespaces/dev-thorium/thoriumclusters/thorium"
        );
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

    /// Build a component Deployment from its status
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the Deployment
    /// * `namespace` - The namespace of the Deployment
    /// * `available` - How many of its replicas are available
    fn component(name: &str, namespace: &str, available: i32) -> Deployment {
        // build a Deployment whose single replica is or isn't available
        serde_json::from_value(serde_json::json!({
            "metadata": {"name": name, "namespace": namespace, "uid": "uid-1", "generation": 1},
            "spec": {"replicas": 1, "selector": {"matchLabels": {"app": name}}, "template": {}},
            "status": {
                "observedGeneration": 1,
                "replicas": 1,
                "updatedReplicas": 1,
                "readyReplicas": available,
                "availableReplicas": available,
                "conditions": [
                    {
                        "type": "Available",
                        "status": if available > 0 { "True" } else { "False" },
                        "lastUpdateTime": "2026-10-01T00:00:00Z"
                    },
                    {"type": "Progressing", "status": "True", "reason": "NewReplicaSetAvailable"}
                ]
            }
        }))
        .expect("deployment")
    }

    /// Only the controller-written availability of a Deployment changes its hash, so the
    /// operator's own spec and annotation patches never retrigger a reconcile
    #[test]
    fn availability_hash_ignores_operator_patches() {
        // hash an available Deployment
        let available = component("scaler", "thorium", 1);
        let base = availability_hash(&available);
        // a new pod template annotation, spec generation, or label doesn't change it
        let mut patched = available.clone();
        patched.metadata.generation = Some(2);
        patched.metadata.annotations = Some(std::collections::BTreeMap::from([(
            "thorium.sandia.gov/config-hash".to_owned(),
            "new".to_owned(),
        )]));
        patched.metadata.labels = Some(std::collections::BTreeMap::from([(
            "version".to_owned(),
            "1.9.0".to_owned(),
        )]));
        patched.spec.as_mut().expect("spec").replicas = Some(3);
        assert_eq!(availability_hash(&patched), base);
        // neither do condition timestamps or messages
        let mut touched = available.clone();
        let conditions = touched
            .status
            .as_mut()
            .and_then(|status| status.conditions.as_mut())
            .expect("conditions");
        conditions[0].last_update_time = None;
        conditions[1].message = Some("progressing".to_owned());
        assert_eq!(availability_hash(&touched), base);
        // losing an available replica does
        assert_ne!(availability_hash(&component("scaler", "thorium", 0)), base);
        // the controller seeing a new spec generation does
        let mut observed = available.clone();
        observed
            .status
            .as_mut()
            .expect("status")
            .observed_generation = Some(2);
        assert_ne!(availability_hash(&observed), base);
        // a stalled rollout does
        let mut stalled = available.clone();
        let progressing = &mut stalled
            .status
            .as_mut()
            .and_then(|status| status.conditions.as_mut())
            .expect("conditions")[1];
        progressing.status = "False".to_owned();
        progressing.reason = Some("ProgressDeadlineExceeded".to_owned());
        assert_ne!(availability_hash(&stalled), base);
        // a recreated Deployment does
        let mut recreated = available;
        recreated.metadata.uid = Some("uid-2".to_owned());
        assert_ne!(availability_hash(&recreated), base);
    }

    /// The availability filter passes on new Deployments, availability changes, and deletions
    /// (including ones missed during a relist) and drops everything else
    #[test]
    fn availability_filter_passes_relevant_changes() {
        // get the names of the Deployments the filter passes on for an event
        let names = |filter: &mut AvailabilityFilter, event| {
            filter
                .changes(event)
                .into_iter()
                .map(|deployment| deployment.metadata.name.unwrap_or_default())
                .collect::<Vec<String>>()
        };
        let mut filter = AvailabilityFilter::default();
        // a Deployment seen for the first time is passed on
        let scaler = component("scaler", "thorium", 1);
        assert_eq!(
            names(&mut filter, watcher::Event::Apply(scaler.clone())),
            ["scaler"]
        );
        // the same availability again isn't, even after an operator spec patch
        let mut patched = scaler.clone();
        patched.metadata.generation = Some(2);
        assert_eq!(
            names(&mut filter, watcher::Event::Apply(patched)),
            Vec::<String>::new()
        );
        // becoming unavailable is, and so is recovering
        let down = component("scaler", "thorium", 0);
        assert_eq!(names(&mut filter, watcher::Event::Apply(down)), ["scaler"]);
        assert_eq!(
            names(&mut filter, watcher::Event::Apply(scaler.clone())),
            ["scaler"]
        );
        // a deletion always is
        assert_eq!(
            names(&mut filter, watcher::Event::Delete(scaler.clone())),
            ["scaler"]
        );
        // a relist passes on new Deployments and the ones that disappeared in between
        let api = component("api", "thorium", 1);
        assert_eq!(names(&mut filter, watcher::Event::Apply(api)), ["api"]);
        assert_eq!(
            names(&mut filter, watcher::Event::Init),
            Vec::<String>::new()
        );
        assert_eq!(
            names(&mut filter, watcher::Event::InitApply(scaler.clone())),
            ["scaler"]
        );
        let gone = filter.changes(watcher::Event::InitDone);
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].metadata.name.as_deref(), Some("api"));
        assert_eq!(gone[0].metadata.namespace.as_deref(), Some("thorium"));
        // an unchanged Deployment relisted again isn't passed on
        assert_eq!(
            names(&mut filter, watcher::Event::Init),
            Vec::<String>::new()
        );
        assert_eq!(
            names(&mut filter, watcher::Event::InitApply(scaler)),
            Vec::<String>::new()
        );
        assert_eq!(
            filter.changes(watcher::Event::InitDone),
            Vec::<Deployment>::new()
        );
    }

    /// A component Deployment maps to the clusters in its namespace that deploy it
    #[test]
    fn deployments_map_to_deploying_clusters() {
        // fill a store with a cluster that has a scaler and one that doesn't
        let (reader, mut writer) = reflector::store::<ThoriumCluster>();
        let mut scaling = cluster_from_spec(serde_json::json!({
            "components": {"api": {}, "scaler": {}},
            "registry": "registry/thorium",
            "config": {}
        }));
        scaling.metadata.name = Some("a".to_owned());
        scaling.metadata.namespace = Some("thorium".to_owned());
        writer.apply_watcher_event(&Event::Apply(scaling));
        writer.apply_watcher_event(&Event::Apply(cluster("b", "dev-thorium", "config")));
        // the scaler maps to the cluster deploying it
        let refs = deployment_to_clusters(&reader, &component("scaler", "thorium", 0));
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].name, "a");
        // the API maps to the cluster in its own namespace
        let refs = deployment_to_clusters(&reader, &component("api", "dev-thorium", 0));
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].name, "b");
        // a component the cluster in that namespace doesn't deploy maps to nothing
        assert_eq!(
            deployment_to_clusters(&reader, &component("scaler", "dev-thorium", 0)),
            Vec::new()
        );
        // a Deployment in a namespace without clusters maps to nothing
        assert_eq!(
            deployment_to_clusters(&reader, &component("api", "other", 0)),
            Vec::new()
        );
    }

    /// Errors from our own apply or cleanup are kept as is while finalizer errors are prefixed
    #[test]
    fn finalizer_errors_unwrapped() {
        // an apply failure is our own error
        let error = unwrap_finalizer_error(finalizer::Error::ApplyFailed(Error::new("apply")));
        assert_eq!(error.to_string(), Error::new("apply").to_string());
        // a cleanup failure is too
        let error = unwrap_finalizer_error(finalizer::Error::CleanupFailed(Error::new("cleanup")));
        assert_eq!(error.to_string(), Error::new("cleanup").to_string());
        // a failure managing the finalizer itself is marked as such
        let error = unwrap_finalizer_error(finalizer::Error::UnnamedObject);
        assert!(error.to_string().contains("Finalizer error"));
    }

    /// Get the writes to a cluster's own resource (not its status), such as finalizer patches
    ///
    /// # Arguments
    ///
    /// * `fake` - The fake kube API that received the writes
    fn cluster_writes(fake: &FakeKube) -> Vec<String> {
        fake.writes()
            .into_iter()
            .filter(|request| request.path.ends_with("/thoriumclusters/thorium"))
            .map(|request| request.method)
            .collect()
    }

    /// An unconverted pre-Helm cluster is only marked as errored: no finalizer, no state
    #[tokio::test]
    async fn reconcile_refuses_unconverted_cluster() {
        // the only cluster is one the pre-Helm scripts deployed
        let mut legacy = created_cluster("thorium", "thorium", "2026-10-01T00:00:00Z");
        legacy.spec.config_secrets.clear();
        legacy.spec.config = serde_json::json!({"thorium": {"secret_key": "inline"}});
        let fake = fake_listing(std::slice::from_ref(&legacy), &legacy);
        let state = applied_state(&fake, &legacy).await;
        // the cluster is rechecked slowly
        let action = Box::pin(reconcile(Arc::new(legacy), state.clone()))
            .await
            .expect("reconcile");
        assert_eq!(action, Action::requeue(Duration::from_secs(600)));
        // the watchers stop acting on it while leaving other clusters alone
        assert_eq!(shared_keys(&state), other_key());
        // the only write is the error status, so no finalizer was added and no state recorded
        let writes = fake.writes();
        assert_eq!(writes.len(), 1, "{writes:?}");
        assert!(writes[0].path.ends_with("/status"));
        assert_eq!(writes[0].body["status"]["phase"], "Error");
        assert_eq!(cluster_writes(&fake), Vec::<String>::new());
    }

    /// A cluster held for a target revision gets its status and nothing else, not even the
    /// finalizer, so deleting it while held removes nothing
    #[tokio::test]
    async fn reconcile_holds_without_finalizer() {
        // a converted cluster at the baseline revision without a target
        let cluster = created_cluster("thorium", "thorium", "2026-10-01T00:00:00Z");
        let state = thorium::models::upgrades::UpgradeState::new(
            "2026-10-v01".parse().expect("revision"),
            "1.8.1",
        );
        let fake = fake_listing(std::slice::from_ref(&cluster), &cluster).route(
            "GET",
            crate::upgrades::tests::STATE_PATH,
            200,
            crate::upgrades::tests::state_config_map(&state),
        );
        let state = applied_state(&fake, &cluster).await;
        // the cluster is held
        Box::pin(reconcile(Arc::new(cluster), state.clone()))
            .await
            .expect("reconcile");
        // the watchers stop acting on it while leaving other clusters alone
        assert_eq!(shared_keys(&state), other_key());
        // the only write is the held status
        let writes = fake.writes();
        assert_eq!(writes.len(), 1, "{writes:?}");
        assert_eq!(writes[0].body["status"]["phase"], "UpgradeRequired");
        assert_eq!(cluster_writes(&fake), Vec::<String>::new());
    }

    /// A malformed upgrade state fails the reconcile with an error status naming the
    /// `ConfigMap`, without writing anything else
    #[tokio::test]
    async fn reconcile_reports_corrupt_state() {
        // a cluster whose state ConfigMap holds a malformed document
        let cluster = created_cluster("thorium", "thorium", "2026-10-01T00:00:00Z");
        let fake = fake_listing(std::slice::from_ref(&cluster), &cluster).route(
            "GET",
            crate::upgrades::tests::STATE_PATH,
            200,
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "metadata": {"name": "thorium-upgrade-state"},
                "data": {"state.json": "{"}
            }),
        );
        let state = applied_state(&fake, &cluster).await;
        // the reconcile fails
        assert!(
            Box::pin(reconcile(Arc::new(cluster), state.clone()))
                .await
                .is_err()
        );
        // the watchers stop acting on it while leaving other clusters alone
        assert_eq!(shared_keys(&state), other_key());
        // the error status names the ConfigMap and nothing else was written
        let writes = fake.writes();
        assert_eq!(writes.len(), 1, "{writes:?}");
        assert_eq!(writes[0].body["status"]["phase"], "Error");
        let message = writes[0].body["status"]["message"]
            .as_str()
            .unwrap_or_default();
        assert!(message.contains("thorium-upgrade-state"), "{message}");
    }

    /// A current cluster whose config stops resolving is dropped from the shared info so the
    /// watchers stop provisioning nodes from its last applied config
    #[tokio::test]
    async fn reconcile_forgets_unresolvable_cluster() {
        // a cluster at the latest revision whose config Secret is missing
        let cluster = created_cluster("thorium", "thorium", "2026-10-01T00:00:00Z");
        let state = thorium::models::upgrades::UpgradeState::new(
            thorium::models::upgrades::latest(),
            "1.8.1",
        );
        let fake = fake_listing(std::slice::from_ref(&cluster), &cluster).route(
            "GET",
            crate::upgrades::tests::STATE_PATH,
            200,
            crate::upgrades::tests::state_config_map(&state),
        );
        let state = applied_state(&fake, &cluster).await;
        // the reconcile fails resolving the config
        let error = Box::pin(reconcile(Arc::new(cluster), state.clone()))
            .await
            .expect_err("config can't resolve");
        assert!(error.to_string().contains("config"), "{error}");
        // the watchers stop acting on it while leaving other clusters alone
        assert_eq!(shared_keys(&state), other_key());
    }
}
