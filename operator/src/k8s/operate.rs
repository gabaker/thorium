use kube::runtime::controller::Action;
use kube::{Api, Client};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use thorium::Error;
use thorium::conf::K8sHostAliases;
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
    println!("{details}");
    // record what we are still waiting on
    k8s::crds::set_status(
        &meta.client,
        &meta.cluster,
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

/// Create a ``ThoriumCluster``
///
/// This creates a Thorium cluster from scratch using a ``ThoriumCluster`` CRD as defined
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
/// # Arguments
///
/// * `meta` - Thorium cluster metadata being operated upon
/// * `url` - Override url for Kubernetes api service.
/// * `shared` - Data shared across watchers
#[allow(clippy::too_many_lines)]
pub async fn apply(
    meta: &ClusterMeta,
    url: Option<String>,
    shared: &Arc<SharedInfo>,
) -> Result<Action, Error> {
    println!(
        "Applying {} ThoriumCluster in {} namespace",
        &meta.name, &meta.namespace
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
    let status = meta.cluster.status.as_ref();
    // mark this cluster as provisioning when it is new or its spec changed, so a retry with
    // the same spec doesn't hide an earlier error
    if status.and_then(|status| status.phase).is_none()
        || status.and_then(|status| status.observed_generation) != meta.cluster.metadata.generation
    {
        k8s::crds::set_status(
            &meta.client,
            &meta.cluster,
            ClusterPhase::Provisioning,
            None,
        )
        .await;
    }
    // create ThoriumCluster namespace if none
    k8s::namespaces::try_create(&meta.namespace).await?;
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
    // create thorium-kaboom user using operator token
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
    // only a cluster with a k8s scaler schedules on, and so provisions, its nodes
    let has_scaler = meta.cluster.spec.components.scaler.is_some();
    if has_scaler {
        // deploy node provision pods
        k8s::nodes::deploy_provision_pods(meta, &thorium).await?;
    }
    // create scaler deployments from CR
    k8s::deployments::deploy_scalers(meta, &host_aliases, &scaler_hashes, &component_hashes)
        .await?;
    // create event handler deployment from CR
    k8s::deployments::deploy_event_handler(meta, &host_aliases, &component_hashes).await?;
    // create search streamer deployment from CR
    k8s::deployments::deploy_search_streamer(meta, &host_aliases, &component_hashes).await?;
    if has_scaler {
        // label nodes
        k8s::nodes::label_all_nodes(meta, &thorium).await?;
        // add nodes to Thorium for each k8s cluster
        app::nodes::add_nodes_to_thorium(meta, &thorium).await?;
    }
    // deploy version specific upgrades here if needed
    app::upgrades::handler(meta).await?;
    // build an info object for our shared map
    let thorium_info = ThoriumInfo {
        thorium: Arc::new(thorium),
        meta: Arc::new(meta.to_owned()),
    };
    // add this Thorium meta object to our shared map
    shared
        .info
        .pin()
        .insert(SharedInfo::key(&meta.namespace, &meta.name), thorium_info);
    // only report ready once every other component has rolled out too, checking back later
    // if they don't finish soon
    let components = meta
        .cluster
        .list_component_names()
        .into_iter()
        .filter(|name| name != "api")
        .collect::<Vec<String>>();
    if let Some(details) =
        k8s::deployments::wait_for_rollouts(meta, &components, ROLLOUT_WAIT).await?
    {
        return Ok(still_provisioning(meta, details).await);
    }
    // fail without recording the bootstrap so it is retried once the missing Secrets exist
    if !missing.is_empty() {
        return Err(Error::new(format!(
            "Components rolled out but the bootstrap is incomplete: {}",
            missing.join("; ")
        )));
    }
    // log completed ThoriumCluster instance
    println!("Completed creation of {} ThoriumCluster", &meta.name);
    // report any non-fatal problems alongside the ready phase
    let message = (!notes.is_empty()).then(|| notes.join("; "));
    // mark this cluster as ready and record the bootstrap inputs we applied
    k8s::crds::set_status_with_bootstrap(
        &meta.client,
        &meta.cluster,
        ClusterPhase::Ready,
        message,
        bootstrap_hash,
    )
    .await;
    // reconcile again in a day if nothing changes before then
    Ok(Action::requeue(Duration::from_secs(APPLY_REQUEUE_SECS)))
}

/// Delete a ``ThoriumCluster``
///
/// This deletes an existing Thorium cluster, leaving only certain artifacts behind for future
/// ``ThoriumCluster`` deployments.
///
/// Notes:
///   Not all cluster remnants are removed with this operation. Databases and database content persist
///   after k8s Thorium resources are cleaned up. User passwords, cluster and node settings will all
///   persist after you delete a ``ThoriumCluster`` resource. This also does not remove any on host files
///   such as those dropped into the /opt/thorium directory of each worker node. Since we don't delete
///   the thorium-operator user, we also choose not to delete the corresponding thorium-operator-pass
///   k8s secret. This will allow future reprovisioning of a new ``ThoriumCluster`` using the same DBs without
///   manual intervention. If you wipe out the DBs after this operation runs, you will need to manually
///   delete that secret, otherwise provisioning with that user will fail. Finally, since some resources
///   may remain inside this namespace, we do not delete the namespace from k8s.
///
/// # Arguments
///
/// * `meta` - Thorium cluster metadata being operated upon
/// * `shared` - Data shared across watchers
pub async fn cleanup(meta: &ClusterMeta, shared: &Arc<SharedInfo>) -> Result<Action, Error> {
    println!(
        "Deleting {} ThoriumCluster in {} namespace",
        meta.name, meta.namespace
    );
    // stop the watchers from provisioning or labelling for this cluster before cleaning up
    shared
        .info
        .pin()
        .remove(&SharedInfo::key(&meta.namespace, &meta.name));
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

/// Delete the resources a ``ThoriumCluster`` created in its own namespace
///
/// This doesn't need the cluster's Thorium config, so it also runs for a cluster whose
/// config can no longer be resolved.
///
/// # Arguments
///
/// * `cluster` - The ``ThoriumCluster`` being deleted
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
