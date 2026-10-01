use crate::k8s::crds::ThoriumCluster;
use kube::api::{Api, ListParams};
use kube::client::Client;
use kube::config::{KubeConfigOptions, Kubeconfig};
use std::sync::Arc;
use thorium::{Error, Thorium};

use super::watchers;
use crate::args::OperateCluster;
use crate::k8s::clusters::ClusterMeta;
use crate::k8s::crds;

/// Info and client about our deployed Thorium cluster
#[derive(Clone)]
pub struct ThoriumInfo {
    /// A client for this Thorium clusters api
    pub thorium: Arc<Thorium>,
    /// The config for this Thorium cluster
    pub meta: Arc<ClusterMeta>,
}

impl ThoriumInfo {
    /// Check whether this cluster's `ThoriumCluster` still exists and isn't being deleted
    ///
    /// The shared info is a snapshot from the last apply, so watchers check the live
    /// resource before acting on a cluster that may be mid-deletion.
    pub async fn is_live(&self) -> Result<bool, Error> {
        // get the current state of this cluster
        let api: Api<ThoriumCluster> =
            Api::namespaced(self.meta.client.clone(), &self.meta.namespace);
        let cluster = api.get_opt(&self.meta.name).await?;
        // a missing or deleting cluster should be left alone
        Ok(cluster.is_some_and(|cluster| cluster.metadata.deletion_timestamp.is_none()))
    }
}

/// The controller the Thorium k8s operator
#[derive(Default)]
pub struct SharedInfo {
    /// The info for different clusters in thorium keyed by `namespace/name`
    pub info: papaya::HashMap<String, ThoriumInfo>,
}

impl SharedInfo {
    /// Build the key a `ThoriumCluster` is stored under
    ///
    /// # Arguments
    ///
    /// * `namespace` - The namespace of the `ThoriumCluster`
    /// * `name` - The name of the `ThoriumCluster`
    pub fn key(namespace: &str, name: &str) -> String {
        format!("{namespace}/{name}")
    }

    /// Get the Thorium info for a specific node
    ///
    /// Clusters without a k8s scaler are skipped since they never schedule on nodes.
    ///
    /// # Arguments
    ///
    /// * `k8s_cluster` - The name of the k8s cluster this node is in
    /// * `node` - The name of the node
    pub fn get_for_node(
        &self,
        k8s_cluster: &str,
        node: &String,
    ) -> Result<Option<ThoriumInfo>, Error> {
        // whether this cluster has any config or not
        let mut has_config = false;
        // track clusters with this name but an empty node list
        let mut possible_cluster = None;
        // whether any cluster runs a k8s scaler at all
        let mut any_scaler = false;
        // iterate over our clusters
        for (_, info) in &self.info.pin() {
            // clusters without a k8s scaler don't use nodes
            if info.meta.cluster.spec.components.scaler.is_none() {
                continue;
            }
            // at least one cluster schedules on nodes
            any_scaler = true;
            // get a ref to our k8s clusters
            let k8s_config = &info.meta.conf.thorium.scaler.k8s;
            // get the k8s cluster this node comes from
            if let Some(cluster) = k8s_config.clusters.get(k8s_cluster) {
                // if this cluster contains this node explicitly then return its config
                if cluster.nodes.contains(node) {
                    return Ok(Some(info.to_owned()));
                }
                // if this cluster contains no nodes then add it as a possible cluster to return
                if cluster.nodes.is_empty() {
                    // set this cluster info as a possible match
                    // but make sure we don't have an ambiguous cluster assignment
                    if possible_cluster.replace(info.to_owned()).is_some() {
                        // we have an ambiguous cluster assignment
                        return Err(Error::new(format!(
                            "{k8s_cluster}:{node} has an ambiguous Thorium cluster assigment"
                        )));
                    }
                }
                // this cluster does have a config
                has_config = true;
            }
        }
        // if we have a possible cluster then return that
        match possible_cluster {
            Some(info) => Ok(Some(info)),
            None => {
                // if this cluster has specified nodes and this node is one of them
                // then its been deliberately ignored and we should not return any cluster info;
                // without any k8s scaler no cluster wants this node either
                if has_config || !any_scaler {
                    Ok(None)
                } else {
                    // this cluster has no config set so return an error
                    Err(Error::new(format!("No config for {k8s_cluster}:{node}")))
                }
            }
        }
    }
}

async fn get_k8s_clients() -> Result<Vec<(String, Client)>, Error> {
    // try to load the kubeconfig from the environment
    let Some(kube_conf) = Kubeconfig::from_env()? else {
        println!("Couldn't find kubeconfig falling back to service account");
        // try to get a kubconfig from the environment
        let client = Client::try_default().await?;
        // assume we are using the default k8s context name
        let name = "kubernetes-admin@cluster.local".to_owned();
        // service accounts will only have a single client ever
        return Ok(vec![(name, client)]);
    };
    // build a list of clients and their context names
    let mut clients = Vec::with_capacity(1);
    // iterate over all contexts in this kube config and build a client for each of them
    // ignoring any of the ignored clusters
    for context in &kube_conf.contexts {
        // build the options for getting a specific clusters config
        let opts = KubeConfigOptions {
            context: Some(context.name.clone()),
            ..Default::default()
        };
        // get this clusters config
        let cluster_conf = kube::Config::from_custom_kubeconfig(kube_conf.clone(), &opts).await?;
        // create a client based on this config
        let client = kube::Client::try_from(cluster_conf)?;
        // add this client and context name to our list of clients
        clients.push((context.name.clone(), client));
    }
    Ok(clients)
}

/// Initialize the controller and shared state (given the crd is installed)
///
/// # Arguments
///
/// * `args` - Arguments passed to the thorium-operator operate sub command
pub async fn run(args: OperateCluster) {
    // TODO: explicitly set ring as default crypto provider, otherwise we get panics
    // when creating the kube client; possibly fixed in newer versions of the kube crate
    // so we might be able to remove this after upgrading kube
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("Failed to set 'ring' as default crypto provider");
    }
    // get clients for all Thorium clusters
    let clients = get_k8s_clients()
        .await
        .expect("Failed to get kubernetes clients");
    // initialize our shared info across controllers/watchers
    let shared = Arc::new(SharedInfo::default());
    // instance a set to spawn our watchers into
    let mut watchers = tokio::task::JoinSet::new();
    // spawn watchers for all clients
    for (name, client) in clients {
        // the crd always has to exist before we can read the resource from k8s
        // create the ThoriumCluster CRD in k8s
        crds::create_or_update(&client)
            .await
            .expect("failed to create ThoriumCluster CRD");
        // list ThoriumCluster resources
        let clusters_api: Api<ThoriumCluster> =
            watchers::scoped_api(&client, args.namespace.as_deref());
        if let Err(error) = clusters_api.list(&ListParams::default().limit(1)).await {
            println!("Failed to list ThoriumCluster API: {error}");
            std::process::exit(1);
        }
        // start the watchers for this cluster
        watchers::start(name, &client, &args, &shared, &mut watchers);
    }
    // wait for either of our watchers to finish
    if let Some(result) = watchers.join_next().await {
        panic!("A watcher died/finished!: {result:#?}");
    }
}
