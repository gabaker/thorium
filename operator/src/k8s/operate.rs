use kube::runtime::controller::Action;
use kube::{Api, Client};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use thorium::conf::K8sHostAliases;
use thorium::{Error, Thorium};
use tokio::time::Duration;

use crate::app;
use crate::app::bootstrap::StepOutcome;
use crate::app::helpers::CheckError;
use crate::k8s;
use crate::k8s::clusters::{ClusterMeta, cluster_name_and_namespace};
use crate::k8s::controller::{SharedInfo, ThoriumInfo};
use crate::k8s::crds::{ClusterPhase, KUBE_CONFIG_KEY, KUBE_CONFIG_SECRET, ThoriumCluster};
use crate::k8s::deployments::RolloutHashes;

/// How long a reconcile waits for deployments to finish rolling out before requeueing, so a
/// pending rollout never blocks a newer spec from being reconciled for long
const ROLLOUT_WAIT: Duration = Duration::from_secs(20);

/// How long to wait for a rolled out API to answer health checks
const HEALTH_TIMEOUT: Duration = Duration::from_secs(60);

/// How long in seconds to wait before checking on a rollout or backend that isn't ready yet
const WAITING_REQUEUE_SECS: u64 = 15u64;

/// How long in seconds to wait before reconciling a ready cluster that has no new events
const APPLY_REQUEUE_SECS: u64 = 86400u64;

/// How long in seconds to wait before retrying nodes that failed to provision
const NODE_FAILURE_REQUEUE_SECS: u64 = 60u64;

/// Hash the documents a component mounts besides thorium.yml
///
/// Each document is prefixed with its length so moving bytes between documents changes the
/// hash.
///
/// # Arguments
///
/// * `docs` - Every mounted document in a fixed order
fn mounts_hash<T: AsRef<[u8]>>(docs: &[T]) -> String {
    // hash each document after its length
    let mut hasher = Sha256::new();
    for doc in docs {
        // get the raw bytes of this document
        let doc = doc.as_ref();
        hasher.update((doc.len() as u64).to_le_bytes());
        hasher.update(doc);
    }
    format!("{:x}", hasher.finalize())
}

/// Read the content of the Elastic CA every component mounts, if the cluster sets one
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
async fn elastic_ca_content(meta: &ClusterMeta) -> Result<Vec<u8>, Error> {
    // clusters without an Elastic CA mount nothing
    let Some(secret_ref) = &meta.cluster.spec.elastic_ca_secret else {
        return Ok(Vec::new());
    };
    // read the key the components mount
    k8s::secrets::mounted_content(meta, &secret_ref.name, &secret_ref.key).await
}

/// Read the kube config the k8s scaler mounts when it doesn't use a service account
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
async fn kube_config_content(meta: &ClusterMeta) -> Result<Vec<u8>, Error> {
    // only a scaler without a service account mounts the kube config
    if meta
        .cluster
        .spec
        .components
        .scaler
        .as_ref()
        .is_none_or(|scaler| scaler.service_account)
    {
        return Ok(Vec::new());
    }
    // read the key the scaler mounts
    k8s::secrets::mounted_content(meta, KUBE_CONFIG_SECRET, KUBE_CONFIG_KEY).await
}

/// Record that a cluster is still waiting on a rollout or backend and check back later
///
/// # Arguments
///
/// * `meta` - Thorium cluster metadata being operated upon
/// * `details` - What the cluster is still waiting on
async fn still_provisioning(meta: &ClusterMeta, details: String) -> Action {
    // log what we are waiting on
    println!("{details}");
    // record what we are still waiting on
    k8s::crds::set_status(
        &meta.client,
        &meta.status,
        ClusterPhase::Provisioning,
        Some(details),
    )
    .await;
    // check back again soon
    Action::requeue(Duration::from_secs(WAITING_REQUEUE_SECS))
}

/// What the backend checks found besides failures
struct BackendChecks {
    /// Non-fatal problems to report alongside the ready phase
    notes: Vec<String>,
    /// Privileged bootstrap steps that couldn't run because a Secret they need is missing
    missing: Vec<String>,
}

/// Prepare Thorium's backends and check each of them with Thorium's own credentials
///
/// This creates Thorium's S3 buckets, runs the privileged Scylla and Elastic bootstrap steps
/// when they are due, and then checks Elastic, Scylla, and Redis. A backend that isn't up yet
/// is returned as [`CheckError::Waiting`].
///
/// # Arguments
///
/// * `meta` - Thorium cluster metadata being operated upon
/// * `bootstrap_due` - Whether the privileged bootstrap steps should run
async fn check_backends(
    meta: &ClusterMeta,
    bootstrap_due: bool,
) -> Result<BackendChecks, CheckError> {
    // create required s3 buckets if not exists
    app::helpers::create_all_buckets(meta).await?;
    // track bootstrap steps that couldn't run because a Secret they need is missing
    let mut missing = Vec::new();
    if bootstrap_due {
        // create Thorium's Scylla role if requested, which the components can't run without
        app::bootstrap::scylla(meta).await?;
        // create Thorium's role and user in an external Elastic if requested
        if let StepOutcome::MissingSecret(details) = app::bootstrap::elastic_identity(meta).await? {
            println!("{details}");
            missing.push(details);
        }
    }
    // make sure Elastic accepts Thorium's credentials with the privileges the search streamer
    // (which creates the indexes) and API need, and note any bad index mappings
    let notes = match app::bootstrap::elastic_access(meta).await {
        Ok(notes) => notes,
        // a skipped Elastic bootstrap is likely why Thorium's user failed and needs attention
        Err(error) if !missing.is_empty() => {
            return Err(Error::new(format!("{}; {error}", missing.join("; "))).into());
        }
        Err(error) => return Err(error),
    };
    // make sure Thorium's own Scylla role can log in
    app::checks::scylla_login(meta).await?;
    // make sure Redis accepts Thorium's credentials
    app::checks::redis_ping(meta).await?;
    Ok(BackendChecks { notes, missing })
}

/// Create or update a `ThoriumCluster`
///
/// This creates or updates a Thorium cluster using a `ThoriumCluster` CRD as defined
/// in the k8s API. Thorium's S3 buckets are created and every backend is checked with
/// Thorium's own credentials before thorium.yml is written, so a bad config is reported in
/// the status instead of being rolled out to the components.
///
/// A backend that isn't reachable or accepting connections yet, like a rollout that hasn't
/// finished, leaves the cluster in the provisioning phase with what it is waiting on and
/// checks back soon instead of failing the reconcile.
///
/// A privileged bootstrap step that has work to do but whose admin Secret is missing doesn't
/// stop the rollout: the components are still deployed, then the reconcile fails with the
/// missing Secrets so the cluster is marked as errored and the bootstrap is retried.
///
/// Nodes are provisioned, labelled, and registered only after every component is deployed, so
/// a node that fails to provision doesn't hold back the components; the cluster is marked as
/// errored listing the failed nodes and they are retried shortly.
///
/// Every component is patched to the same spec on each apply, which changes nothing for a
/// component that is already deployed, so an apply triggered by a component becoming
/// unavailable only waits on it and reports it rather than rolling anything out.
///
/// # Arguments
///
/// * `meta` - Thorium cluster metadata being operated upon
/// * `url` - An override for the url the operator reaches the Thorium API at
/// * `shared` - Data shared across watchers
#[allow(clippy::too_many_lines)]
pub async fn apply(
    meta: &ClusterMeta,
    url: Option<String>,
    shared: &Arc<SharedInfo>,
) -> Result<Action, Error> {
    // log which cluster we are applying
    println!(
        "Applying {} ThoriumCluster in {} namespace",
        meta.name, meta.namespace
    );
    // get our k8s config
    let k8s_config = &meta.conf.thorium.scaler.k8s;
    // get the name of our primary cluster
    let primary = &k8s_config.primary_cluster;
    // get our host aliases
    let unconverted_aliases = k8s_config.host_aliases(primary);
    // convert our host aliases into a K8s host aliases object
    let host_aliases: Vec<K8sHostAliases> = unconverted_aliases.map_or_else(Vec::new, |map| {
        map.iter().map(K8sHostAliases::from).collect()
    });
    // get the status this cluster was last left in
    let status = meta.status.current();
    // mark this cluster as provisioning when it is new or its spec changed, so a retry with
    // the same spec doesn't hide an earlier error
    if status.phase.is_none() || status.observed_generation != meta.cluster.metadata.generation {
        k8s::crds::set_status(&meta.client, &meta.status, ClusterPhase::Provisioning, None).await;
    }
    // create or update ConfigMaps
    k8s::config_maps::create_or_update_all(meta).await?;
    // create or update registry secrets
    k8s::secrets::create_or_update_registry_auth(meta).await?;
    // render thorium.yml without writing it so the backends can be checked first
    let rendered = k8s::secrets::render_thorium_config(&meta.conf)?;
    // hash the inputs of our privileged bootstrap steps
    let bootstrap_hash = app::bootstrap::input_hash(meta, &rendered.hash).await?;
    // only run the privileged bootstrap steps when their inputs changed
    let bootstrap_due = app::bootstrap::is_due(&meta.cluster, &bootstrap_hash);
    // prepare and check every backend, waiting on any that isn't up yet
    let BackendChecks { notes, mut missing } = match check_backends(meta, bootstrap_due).await {
        Ok(checks) => checks,
        Err(CheckError::Waiting(details)) => return Ok(still_provisioning(meta, details).await),
        Err(CheckError::Failed(error)) => return Err(error),
    };
    // write the checked config right before the components that mount it are deployed
    k8s::secrets::write_thorium_config(meta, &rendered).await?;
    // create or update API service
    k8s::services::create_or_update_all(meta).await?;
    // read the Elastic CA every component mounts so rotating it rolls them out
    let elastic_ca = elastic_ca_content(meta).await?;
    // read the banner the API only reads at startup so changing it rolls the API out
    let banner = k8s::config_maps::banner(meta).await?;
    // roll the API out whenever its config, banner, or Elastic CA change
    let api_hashes = RolloutHashes {
        config: rendered.hash.clone(),
        mounts: mounts_hash(&[banner.as_bytes(), elastic_ca.as_slice()]),
    };
    // create or update api deployment from CR
    k8s::deployments::deploy_api(meta, &host_aliases, &api_hashes).await?;
    // build API url host string
    let host: String = app::helpers::get_thorium_host(meta, url.as_ref());
    // check back later if the api deployment doesn't finish rolling out soon
    let api = vec!["api".to_owned()];
    if let Some(details) = k8s::deployments::wait_for_rollouts(meta, &api, ROLLOUT_WAIT).await? {
        return Ok(still_provisioning(meta, details).await);
    }
    // make sure the API answers at the url we reach it with
    if let Some(details) = k8s::deployments::wait_for_health(&host, HEALTH_TIMEOUT).await? {
        return Ok(still_provisioning(meta, details).await);
    }
    // create operator user and retrieve token
    let operator_token = app::users::create_operator(meta, &host).await?;
    // build out operator thorium client
    let operator = app::helpers::thorium_client(&host, &operator_token).await?;
    // create the initial admin user if requested and our bootstrap inputs changed
    if bootstrap_due
        && let StepOutcome::MissingSecret(details) =
            app::bootstrap::admin(meta, &host, &operator).await?
    {
        println!("{details}");
        missing.push(details);
    }
    // create thorium user using operator token
    let (thorium_password, thorium_token) =
        app::users::create(meta, &operator, &host, "thorium").await?;
    // build out thorium user's thorium client
    let thorium = app::helpers::thorium_client(&host, &thorium_token).await?;
    // create thorium-kaboom user using the thorium user's token
    let (kaboom_password, _) = app::users::create(meta, &thorium, &host, "thorium-kaboom").await?;
    // create keys.yml secret for thorium user
    let keys = k8s::secrets::create_keys(meta, "thorium", &thorium_password, None).await?;
    // create keys.yml secret for thorium-kaboom user, which no component mounts
    k8s::secrets::create_keys(
        meta,
        "thorium-kaboom",
        &kaboom_password,
        Some("keys-kaboom"),
    )
    .await?;
    // roll the components out whenever the config, keys, or Elastic CA they mount change
    let component_hashes = RolloutHashes {
        config: rendered.hash.clone(),
        mounts: mounts_hash(&[keys.as_bytes(), elastic_ca.as_slice()]),
    };
    // the k8s scaler also mounts the kube config when it doesn't use a service account
    let kube_config = kube_config_content(meta).await?;
    let scaler_hashes = RolloutHashes {
        config: rendered.hash.clone(),
        mounts: mounts_hash(&[
            keys.as_bytes(),
            elastic_ca.as_slice(),
            kube_config.as_slice(),
        ]),
    };
    // init cluster system settings
    app::configure::init_settings(&thorium).await?;
    // create scaler deployments from CR
    k8s::deployments::deploy_scalers(meta, &host_aliases, &scaler_hashes, &component_hashes)
        .await?;
    // create event handler deployment from CR
    k8s::deployments::deploy_event_handler(meta, &host_aliases, &component_hashes).await?;
    // create search streamer deployment from CR
    k8s::deployments::deploy_search_streamer(meta, &host_aliases, &component_hashes).await?;
    // share this cluster's Thorium client with the node watcher, which provisions ready nodes
    // that have no thorium label or one for an older version (such as nodes added later)
    let thorium = Arc::new(thorium);
    let thorium_info = ThoriumInfo {
        thorium: thorium.clone(),
        meta: Arc::new(meta.to_owned()),
    };
    // add this cluster to our shared map
    shared.info.pin().insert(
        SharedInfo::key(&meta.context, &meta.namespace, &meta.name),
        thorium_info,
    );
    // only a cluster with a k8s scaler schedules on, and so provisions, its nodes; this runs
    // after every component is deployed so a node that fails to provision never holds back
    // the rest of the cluster
    let provisioning = if meta.cluster.spec.components.scaler.is_some() {
        provision_workers(meta, &thorium).await?
    } else {
        None
    };
    // wait on the remaining components and report the cluster's phase
    let outcome = Outcome {
        notes,
        missing,
        provisioning,
        bootstrap_hash,
    };
    finish(meta, outcome, ROLLOUT_WAIT).await
}

/// Provision, label, and register the nodes a cluster with a k8s scaler schedules on
///
/// Returns a description of the nodes that failed to provision, if any did. A failed node
/// doesn't stop the others from being provisioned, labelled, and registered.
///
/// # Arguments
///
/// * `meta` - Thorium cluster metadata being operated upon
/// * `thorium` - The thorium user's Thorium client
async fn provision_workers(meta: &ClusterMeta, thorium: &Thorium) -> Result<Option<String>, Error> {
    // get the version the API reports so a new version replaces provision pods and labels
    let version = thorium.updates.get_version().await?.thorium.to_string();
    // provision and label every available node
    let failures = provision_and_label(meta, &version).await?;
    // add nodes to Thorium for each k8s cluster
    app::nodes::add_nodes_to_thorium(meta, thorium).await?;
    Ok(failures)
}

/// Provision every available node and label the ones that were provisioned
///
/// Returns a description of the nodes that failed to provision (or why the nodes couldn't be
/// listed) instead of failing, so the caller can still finish reconciling the cluster.
///
/// # Arguments
///
/// * `meta` - Thorium cluster metadata being operated upon
/// * `version` - The Thorium version the API reports
async fn provision_and_label(meta: &ClusterMeta, version: &str) -> Result<Option<String>, Error> {
    // deploy node provision pods, collecting the nodes that fail
    let failures = match k8s::nodes::provision_nodes(meta, version).await {
        Ok(failures) => failures,
        // without the node list nothing can be provisioned or labelled
        Err(error) => {
            return Ok(Some(format!(
                "Failed to provision nodes: {}",
                k8s::crds::error_message(&error)
            )));
        }
    };
    // label the nodes that were provisioned
    k8s::nodes::label_nodes(meta, version, &failures.names()).await?;
    Ok(failures.message())
}

/// What an apply found before waiting on the components that report a cluster as ready
struct Outcome {
    /// Non-fatal problems to report alongside the ready phase
    notes: Vec<String>,
    /// Privileged bootstrap steps that couldn't run because a Secret they need is missing
    missing: Vec<String>,
    /// A description of the nodes that failed to provision, if any did
    provisioning: Option<String>,
    /// The hash of the bootstrap inputs this apply used
    bootstrap_hash: String,
}

/// Wait on every component besides the API and report the cluster's phase
///
/// A cluster whose components are all available is `Ready`; one whose components are still
/// rolling out or have become unavailable (such as when their node goes down) is
/// `Provisioning` naming them and is checked again soon. Nodes that failed to provision make
/// the cluster `Error` listing them (along with any pending components, which aren't waited
/// on then) while its components stay deployed, and they are retried shortly.
///
/// # Arguments
///
/// * `meta` - Thorium cluster metadata being operated upon
/// * `outcome` - What the apply found
/// * `rollout_wait` - How long to wait for the components to roll out
async fn finish(
    meta: &ClusterMeta,
    outcome: Outcome,
    rollout_wait: Duration,
) -> Result<Action, Error> {
    // get every component but the API, which has already rolled out
    let components = meta
        .cluster
        .list_component_names()
        .into_iter()
        .filter(|name| name != "api")
        .collect::<Vec<String>>();
    // nodes that failed to provision are reported right away, so don't wait on the components
    let rollout_wait = if outcome.provisioning.is_some() {
        Duration::ZERO
    } else {
        rollout_wait
    };
    // only report ready once every other component has rolled out too
    let pending = k8s::deployments::wait_for_rollouts(meta, &components, rollout_wait).await?;
    // without failed nodes the components alone decide the cluster's phase
    let Some(failures) = outcome.provisioning else {
        // check back later on components that don't finish soon
        if let Some(details) = pending {
            return Ok(still_provisioning(meta, details).await);
        }
        // fail without recording the bootstrap so it is retried once the missing Secrets exist
        if !outcome.missing.is_empty() {
            return Err(Error::new(format!(
                "Components rolled out but the bootstrap is incomplete: {}",
                outcome.missing.join("; ")
            )));
        }
        // log completed ThoriumCluster instance
        println!("Completed creation of {} ThoriumCluster", meta.name);
        // report any non-fatal problems alongside the ready phase
        let message = (!outcome.notes.is_empty()).then(|| outcome.notes.join("; "));
        // mark this cluster as ready and record the bootstrap inputs we applied
        k8s::crds::set_status_with_bootstrap(
            &meta.client,
            &meta.status,
            ClusterPhase::Ready,
            message,
            outcome.bootstrap_hash,
        )
        .await;
        // reconcile again in a day if nothing changes before then
        return Ok(Action::requeue(Duration::from_secs(APPLY_REQUEUE_SECS)));
    };
    // list the failed nodes first, then anything else that needs attention
    let mut problems = vec![failures];
    problems.extend(pending);
    // fail without recording the bootstrap so it is retried once the missing Secrets exist
    if !outcome.missing.is_empty() {
        problems.push(format!(
            "the bootstrap is incomplete: {}",
            outcome.missing.join("; ")
        ));
        return Err(Error::new(problems.join("; ")));
    }
    // report the non-fatal problems too
    problems.extend(outcome.notes);
    let message = problems.join("; ");
    println!("Error: {message}");
    // mark this cluster as errored while recording the bootstrap inputs we applied
    k8s::crds::set_status_with_bootstrap(
        &meta.client,
        &meta.status,
        ClusterPhase::Error,
        Some(message),
        outcome.bootstrap_hash,
    )
    .await;
    // retry the failed nodes shortly
    Ok(Action::requeue(Duration::from_secs(
        NODE_FAILURE_REQUEUE_SECS,
    )))
}

/// Delete a `ThoriumCluster`
///
/// This deletes an existing Thorium cluster, leaving only certain artifacts behind for future
/// `ThoriumCluster` deployments.
///
/// Notes:
///   Not all cluster remnants are removed with this operation. Databases and database content persist
///   after k8s Thorium resources are cleaned up. User passwords, cluster and node settings will all
///   persist after you delete a `ThoriumCluster` resource. This also does not remove any on host files
///   such as those dropped into the /opt/thorium directory of each worker node. Since we don't delete
///   the thorium-operator user, we also choose not to delete the corresponding thorium-operator-pass
///   k8s secret. This will allow future reprovisioning of a new `ThoriumCluster` using the same DBs without
///   manual intervention. If you wipe out the DBs after this operation runs, you will need to manually
///   delete that secret, otherwise provisioning with that user will fail. Finally, since some resources
///   may remain inside this namespace, we do not delete the namespace from k8s.
///
/// # Arguments
///
/// * `meta` - Thorium cluster metadata being operated upon
/// * `shared` - Data shared across watchers
pub async fn cleanup(meta: &ClusterMeta, shared: &Arc<SharedInfo>) -> Result<Action, Error> {
    // log which cluster we are deleting
    println!(
        "Deleting {} ThoriumCluster in {} namespace",
        meta.name, meta.namespace
    );
    // stop the watchers from provisioning or labelling for this cluster before cleaning up
    shared
        .info
        .pin()
        .remove(&SharedInfo::key(&meta.context, &meta.namespace, &meta.name));
    // only a cluster with a k8s scaler labels nodes, so leave other clusters' labels alone
    if meta.cluster.spec.components.scaler.is_some() {
        // remove kubernetes node labels
        k8s::nodes::delete_node_labels(meta).await?;
    }
    // delete node provision pods by name, including any created before they were labelled
    k8s::nodes::cleanup_provision_pods(meta).await?;
    // remove the resources that don't depend on this cluster's config
    cleanup_namespace(&meta.cluster, &meta.client).await?;
    // log completion of cluster deletion
    println!("Completed cleanup of {} ThoriumCluster", meta.name);
    Ok(Action::await_change())
}

/// Delete the resources a `ThoriumCluster` created in its own namespace
///
/// This doesn't need the cluster's Thorium config, so it also runs for a cluster whose
/// config can no longer be resolved.
///
/// # Arguments
///
/// * `cluster` - The `ThoriumCluster` being deleted
/// * `client` - The kube client to delete resources with
pub async fn cleanup_namespace(cluster: &ThoriumCluster, client: &Client) -> Result<(), Error> {
    // get the name and namespace of this cluster
    let (name, namespace) = cluster_name_and_namespace(cluster)?;
    // delete this cluster's node provision pods by label
    k8s::nodes::delete_provision_pods_by_label(&Api::namespaced(client.clone(), &namespace), &name)
        .await?;
    // remove thorium component deployments
    k8s::deployments::delete(&Api::namespaced(client.clone(), &namespace), cluster).await?;
    // delete the api and mcp services
    k8s::services::delete(&Api::namespaced(client.clone(), &namespace)).await?;
    // remove secrets including thorium.yml and keys.yml
    k8s::secrets::delete(&Api::namespaced(client.clone(), &namespace)).await?;
    // remove configmaps such as tracing.yml
    k8s::config_maps::delete(&Api::namespaced(client.clone(), &namespace)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::clusters::tests::{FakeKube, meta_for, namespaced_cluster};

    /// The path of the test cluster's status subresource
    const STATUS_PATH: &str =
        "/apis/sandia.gov/v1/namespaces/thorium/thoriumclusters/thorium/status";

    /// Build a cluster with an API and a k8s scaler whose status is in a phase
    ///
    /// # Arguments
    ///
    /// * `phase` - The phase the cluster's status reports
    fn scaling_cluster(phase: ClusterPhase) -> ThoriumCluster {
        // build a cluster with just an API and a scaler
        let mut cluster = namespaced_cluster(serde_json::json!({
            "components": {"api": {}, "scaler": {}},
            "registry": "registry/thorium",
            "version": "1.8.1",
            "config": {},
            "config_secrets": [{"name": "thorium-config-secrets"}]
        }));
        cluster.metadata.generation = Some(1);
        cluster.status = Some(crate::k8s::crds::ThoriumClusterStatus {
            phase: Some(phase),
            observed_generation: Some(1),
            bootstrap_hash: Some("bootstrap".to_owned()),
            ..Default::default()
        });
        cluster
    }

    /// Build a fake kube API serving a scaler Deployment with some available replicas, a pod
    /// stranded on a down node, and the cluster's status subresource
    ///
    /// # Arguments
    ///
    /// * `cluster` - The cluster whose status is patched
    /// * `available` - How many of the scaler's single replica are available
    fn fake_components(cluster: &ThoriumCluster, available: i32) -> FakeKube {
        // a scaler that rolled out with one replica that is or isn't available
        let scaler = serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": {"name": "scaler", "namespace": "thorium", "generation": 1},
            "spec": {"replicas": 1, "selector": {"matchLabels": {"app": "scaler"}}, "template": {}},
            "status": {
                "observedGeneration": 1,
                "replicas": 1,
                "updatedReplicas": 1,
                "availableReplicas": available
            }
        });
        // its pod, which the node lifecycle controller marked unready
        let pods = serde_json::json!({
            "apiVersion": "v1",
            "kind": "PodList",
            "metadata": {},
            "items": [{
                "metadata": {"name": "scaler-1"},
                "status": {
                    "phase": "Running",
                    "conditions": [{"type": "Ready", "status": "False"}],
                    "containerStatuses": [{
                        "name": "scaler",
                        "ready": true,
                        "restartCount": 0,
                        "image": "thorium",
                        "imageID": "",
                        "state": {"running": {}}
                    }]
                }
            }]
        });
        FakeKube::default()
            .route(
                "GET",
                "/apis/apps/v1/namespaces/thorium/deployments/scaler",
                200,
                scaler,
            )
            .route("GET", "/api/v1/namespaces/thorium/pods", 200, pods)
            .route(
                "PATCH",
                STATUS_PATH,
                200,
                serde_json::to_value(cluster).expect("cluster"),
            )
    }

    /// Build what an apply found
    ///
    /// # Arguments
    ///
    /// * `missing` - The bootstrap steps whose Secrets are missing
    /// * `provisioning` - A description of the nodes that failed to provision
    fn outcome(missing: &[&str], provisioning: Option<&str>) -> Outcome {
        Outcome {
            notes: Vec::new(),
            missing: missing.iter().map(ToString::to_string).collect(),
            provisioning: provisioning.map(ToOwned::to_owned),
            bootstrap_hash: "bootstrap-2".to_owned(),
        }
    }

    /// Get the status patches sent through a fake kube API
    ///
    /// # Arguments
    ///
    /// * `fake` - The fake kube API
    fn status_writes(fake: &FakeKube) -> Vec<serde_json::Value> {
        fake.writes()
            .into_iter()
            .filter(|request| request.path == STATUS_PATH)
            .map(|request| request.body["status"].clone())
            .collect()
    }

    /// A ready cluster whose component becomes unavailable (here with its node down) moves to
    /// provisioning naming it and why, and moves back to ready once it recovers
    #[tokio::test]
    async fn unavailable_component_leaves_and_regains_ready() {
        // a ready cluster whose scaler lost its only available replica
        let cluster = scaling_cluster(ClusterPhase::Ready);
        let fake = fake_components(&cluster, 0);
        let meta = meta_for(cluster, &fake.client());
        // the cluster is checked again soon
        let action = finish(&meta, outcome(&[], None), Duration::ZERO)
            .await
            .expect("finish");
        assert_eq!(
            action,
            Action::requeue(Duration::from_secs(WAITING_REQUEUE_SECS))
        );
        // the only write is the provisioning status naming the scaler and its down node
        assert_eq!(fake.writes().len(), 1, "{:?}", fake.writes());
        let status = &status_writes(&fake)[0];
        assert_eq!(status["phase"], "Provisioning");
        let message = status["message"].as_str().expect("message");
        assert!(
            message.starts_with("Waiting for scaler to roll out: scaler: "),
            "{message}"
        );
        assert!(
            message.contains("node may be down or unreachable"),
            "{message}"
        );
        // once the scaler is available again the cluster is ready
        let cluster = scaling_cluster(ClusterPhase::Provisioning);
        let fake = fake_components(&cluster, 1);
        let meta = meta_for(cluster, &fake.client());
        let action = finish(&meta, outcome(&[], None), Duration::ZERO)
            .await
            .expect("finish");
        assert_eq!(
            action,
            Action::requeue(Duration::from_secs(APPLY_REQUEUE_SECS))
        );
        let status = &status_writes(&fake)[0];
        assert_eq!(status["phase"], "Ready");
        assert_eq!(status["bootstrap_hash"], "bootstrap-2");
    }

    /// Nodes that failed to provision mark the cluster as errored listing them and any pending
    /// component without waiting on it, record the completed bootstrap, and are retried soon
    #[tokio::test]
    async fn failed_nodes_report_error() {
        // a cluster whose components are deployed but whose scaler isn't available yet
        let cluster = scaling_cluster(ClusterPhase::Ready);
        let fake = fake_components(&cluster, 0);
        let meta = meta_for(cluster, &fake.client());
        let failed = "Failed to provision nodes: node node-a: fake error";
        // the reconcile succeeds and retries the nodes shortly
        let action = finish(&meta, outcome(&[], Some(failed)), ROLLOUT_WAIT)
            .await
            .expect("finish");
        assert_eq!(
            action,
            Action::requeue(Duration::from_secs(NODE_FAILURE_REQUEUE_SECS))
        );
        // a single error status lists the failed node and then the pending scaler
        let statuses = status_writes(&fake);
        assert_eq!(statuses.len(), 1, "{statuses:?}");
        assert_eq!(statuses[0]["phase"], "Error");
        let message = statuses[0]["message"].as_str().expect("message");
        assert!(message.starts_with(failed), "{message}");
        assert!(
            message.contains("Waiting for scaler to roll out"),
            "{message}"
        );
        assert_eq!(statuses[0]["bootstrap_hash"], "bootstrap-2");
        // with every component available only the failed nodes are listed
        let cluster = scaling_cluster(ClusterPhase::Ready);
        let fake = fake_components(&cluster, 1);
        let meta = meta_for(cluster, &fake.client());
        finish(&meta, outcome(&[], Some(failed)), Duration::ZERO)
            .await
            .expect("finish");
        assert_eq!(status_writes(&fake)[0]["message"], failed);
    }

    /// Nodes that failed to provision alongside missing bootstrap Secrets fail the reconcile
    /// with both, without recording the bootstrap
    #[tokio::test]
    async fn failed_nodes_and_missing_secrets_fail() {
        // a cluster whose components are all available
        let cluster = scaling_cluster(ClusterPhase::Ready);
        let fake = fake_components(&cluster, 1);
        let meta = meta_for(cluster, &fake.client());
        // the reconcile fails naming the failed node and the missing Secret
        let error = finish(
            &meta,
            outcome(
                &["missing admin Secret"],
                Some("Failed to provision nodes: node node-a"),
            ),
            Duration::ZERO,
        )
        .await
        .expect_err("incomplete bootstrap");
        let message = error.to_string();
        assert!(message.contains("node node-a"), "{message}");
        assert!(
            message.contains("the bootstrap is incomplete: missing admin Secret"),
            "{message}"
        );
        // nothing was written, so the bootstrap isn't recorded
        assert!(fake.writes().is_empty(), "{:?}", fake.writes());
        // missing Secrets alone keep their message
        let error = finish(
            &meta,
            outcome(&["missing admin Secret"], None),
            Duration::ZERO,
        )
        .await
        .expect_err("incomplete bootstrap");
        assert!(
            error
                .to_string()
                .contains("Components rolled out but the bootstrap is incomplete")
        );
    }

    /// The mounts hash is stable and changes when any mounted document changes
    #[test]
    fn mounts_hash_tracks_documents() {
        // hash the same keys, banner, and CA twice
        let base = mounts_hash(&["keys", "banner", "ca"]);
        assert_eq!(base, mounts_hash(&["keys", "banner", "ca"]));
        // rotating the keys, changing the banner, or rotating the CA changes the hash
        assert_ne!(base, mounts_hash(&["rotated", "banner", "ca"]));
        assert_ne!(base, mounts_hash(&["keys", "changed", "ca"]));
        assert_ne!(base, mounts_hash(&["keys", "banner", "rotated"]));
        // adding a kube config changes the hash
        assert_ne!(base, mounts_hash(&["keys", "banner", "ca", "kube"]));
        // moving bytes between documents changes the hash
        assert_ne!(mounts_hash(&["ab", "c"]), mounts_hash(&["a", "bc"]));
        // an empty document is still counted
        assert_ne!(mounts_hash(&["a", ""]), mounts_hash(&["a"]));
    }
}
