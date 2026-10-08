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

/// The kube context name the operator uses for its own k8s cluster when running in-cluster
/// without a kubeconfig
pub const IN_CLUSTER_CONTEXT: &str = "kubernetes-admin@cluster.local";

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

/// The state shared by the Thorium operator's controllers and watchers
#[derive(Default)]
pub struct SharedInfo {
    /// The info for different clusters in thorium keyed by `context/namespace/name`
    pub info: papaya::HashMap<String, ThoriumInfo>,
    /// Config warnings already logged, so a problem hit on every node reconcile is only
    /// logged once until its message changes
    warned: std::sync::Mutex<std::collections::BTreeSet<String>>,
}

/// Which Thorium cluster, if any, schedules on a node
#[derive(Debug, PartialEq, Eq)]
enum NodeMatch<T> {
    /// This cluster schedules on the node
    Found(T),
    /// The node is deliberately left out, or no cluster runs a k8s scaler
    Ignored,
    /// No cluster with a k8s scaler has config for the node's kube context
    Unconfigured {
        /// The kube contexts the clusters with a k8s scaler do have config for
        configured: Vec<String>,
    },
}

/// Find the Thorium cluster that schedules on a node
///
/// Clusters without a k8s scaler are skipped since they never schedule on nodes. A cluster
/// listing the node wins over one that lists no nodes (and so uses every node), and two
/// clusters that both list no nodes are ambiguous unless one lists the node, whatever order
/// the clusters are checked in.
///
/// # Arguments
///
/// * `clusters` - Each cluster's metadata along with the value to return when it matches
/// * `k8s_cluster` - The name of the kube context this node is in
/// * `node` - The name of the node
fn match_node<'a, T>(
    clusters: impl IntoIterator<Item = (&'a ClusterMeta, T)>,
    k8s_cluster: &str,
    node: &str,
) -> Result<NodeMatch<T>, Error> {
    // whether any cluster has config for this kube context
    let mut has_config = false;
    // the cluster with this context but an empty node list
    let mut possible_cluster = None;
    // whether two clusters both use every node of this context
    let mut ambiguous = false;
    // whether any cluster runs a k8s scaler at all
    let mut any_scaler = false;
    // the kube contexts that do have config, for a mismatch warning
    let mut configured = std::collections::BTreeSet::new();
    // iterate over our clusters
    for (meta, value) in clusters {
        // clusters without a k8s scaler don't use nodes
        if meta.cluster.spec.components.scaler.is_none() {
            continue;
        }
        // at least one cluster schedules on nodes
        any_scaler = true;
        // get a ref to our k8s clusters
        let k8s_config = &meta.conf.thorium.scaler.k8s;
        configured.extend(k8s_config.clusters.keys().cloned());
        // get the k8s cluster this node comes from
        if let Some(cluster) = k8s_config.clusters.get(k8s_cluster) {
            // this cluster does have a config
            has_config = true;
            // a cluster listing this node explicitly always wins
            if cluster.nodes.iter().any(|listed| listed == node) {
                return Ok(NodeMatch::Found(value));
            }
            // a cluster using every node is a possible match unless another one already is
            if cluster.nodes.is_empty() && possible_cluster.replace(value).is_some() {
                ambiguous = true;
            }
        }
    }
    // two clusters using every node can't both own it
    if ambiguous {
        return Err(Error::new(format!(
            "{k8s_cluster}:{node} has an ambiguous Thorium cluster assignment"
        )));
    }
    match possible_cluster {
        // a single cluster using every node owns it
        Some(value) => Ok(NodeMatch::Found(value)),
        // a node left out of a configured context is deliberately ignored, and without any
        // k8s scaler no cluster wants it either
        None if has_config || !any_scaler => Ok(NodeMatch::Ignored),
        // clusters with a k8s scaler exist but none has config for this context
        None => Ok(NodeMatch::Unconfigured {
            configured: configured.into_iter().collect(),
        }),
    }
}
impl SharedInfo {
    /// Build the key a `ThoriumCluster` is stored under
    ///
    /// The kube context is part of the key so clusters with the same namespace and name in
    /// two k8s clusters don't replace each other.
    ///
    /// # Arguments
    ///
    /// * `context` - The name of the kube context the `ThoriumCluster` is in
    /// * `namespace` - The namespace of the `ThoriumCluster`
    /// * `name` - The name of the `ThoriumCluster`
    pub fn key(context: &str, namespace: &str, name: &str) -> String {
        format!("{context}/{namespace}/{name}")
    }

    /// Check whether a warning still needs logging, remembering it so it is logged only once
    ///
    /// # Arguments
    ///
    /// * `message` - The warning to log
    fn first_warning(&self, message: &str) -> bool {
        // a panic while holding the lock can't leave the set half written
        let mut warned = self
            .warned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        warned.insert(message.to_owned())
    }

    /// Get the Thorium info for the cluster that schedules on a specific node
    ///
    /// See [`match_node`] for how the cluster is picked. Returns None when the node is
    /// deliberately left out of every cluster, or when no cluster has config for the node's
    /// kube context, which is logged once since it is a config mismatch rather than a problem
    /// with the node.
    ///
    /// # Arguments
    ///
    /// * `k8s_cluster` - The name of the kube context this node is in
    /// * `node` - The name of the node
    pub fn get_for_node(
        &self,
        k8s_cluster: &str,
        node: &str,
    ) -> Result<Option<ThoriumInfo>, Error> {
        // find the cluster scheduling on this node among the deployed clusters
        let pinned = self.info.pin();
        let found = match_node(
            pinned.values().map(|info| (info.meta.as_ref(), info)),
            k8s_cluster,
            node,
        )?;
        match found {
            NodeMatch::Found(info) => Ok(Some(info.to_owned())),
            NodeMatch::Ignored => Ok(None),
            NodeMatch::Unconfigured { configured } => {
                // log the mismatch once instead of on every reconcile of every node
                let message = context_mismatch_message(k8s_cluster, &configured);
                if self.first_warning(&message) {
                    eprintln!("Warning: {message}");
                }
                Ok(None)
            }
        }
    }
}

/// Describe a kube context that no Thorium cluster with a k8s scaler has config for
///
/// # Arguments
///
/// * `context` - The kube context the operator reaches the nodes through
/// * `configured` - The kube contexts the clusters do have config for
fn context_mismatch_message(context: &str, configured: &[String]) -> String {
    format!(
        "The operator reaches this Kubernetes cluster through kube context '{context}', but \
         thorium.scaler.k8s.clusters only has config for [{}], so its nodes are not provisioned \
         or labelled for Thorium. Key the cluster by '{context}' in thorium.scaler.k8s.clusters \
         (Helm value operator.cluster.scaler.k8s.context); an operator running in-cluster always \
         uses the context name '{IN_CLUSTER_CONTEXT}'",
        configured.join(", ")
    )
}

/// Build a kube client for every k8s cluster the operator manages
///
/// Every context in the kubeconfig named by `KUBECONFIG` gets a client named after it. Without
/// a kubeconfig the operator runs in-cluster with its service account.
async fn get_k8s_clients() -> Result<Vec<(String, Client)>, Error> {
    // try to load the kubeconfig from the environment
    let Some(kube_conf) = Kubeconfig::from_env()? else {
        // log that we are running with our service account
        println!("Couldn't find kubeconfig falling back to service account");
        // try to get a kubconfig from the environment
        let client = Client::try_default().await?;
        // assume we are using the default k8s context name
        let name = IN_CLUSTER_CONTEXT.to_owned();
        // service accounts will only have a single client ever
        return Ok(vec![(name, client)]);
    };
    // build a list of clients and their context names
    let mut clients = Vec::with_capacity(1);
    // iterate over all contexts in this kube config and build a client for each of them
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
    // the watchers run forever, so any of them finishing is fatal
    if let Some(result) = watchers.join_next().await {
        panic!("A watcher died/finished!: {result:#?}");
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::k8s::clusters::tests::{FakeKube, full_spec, meta_for, namespaced_cluster};

    /// Build shared info with a cluster deployed like an apply leaves it
    ///
    /// The cluster's Thorium client points at a closed port, so anything using it fails fast.
    ///
    /// # Arguments
    ///
    /// * `meta` - The metadata of the deployed cluster
    pub(crate) async fn shared_with(meta: ClusterMeta) -> SharedInfo {
        // build a Thorium client that never reaches an API
        let thorium = crate::app::helpers::thorium_client("http://127.0.0.1:9", "token")
            .await
            .expect("thorium client");
        // add the cluster under the key an apply uses
        let shared = SharedInfo::default();
        shared.info.pin().insert(
            SharedInfo::key(&meta.context, &meta.namespace, &meta.name),
            ThoriumInfo {
                thorium: Arc::new(thorium),
                meta: Arc::new(meta),
            },
        );
        shared
    }

    /// Build cluster metadata whose k8s scaler config lists clusters by context
    ///
    /// # Arguments
    ///
    /// * `client` - The kube client the metadata's APIs use
    /// * `scaler` - Whether the cluster runs a k8s scaler
    /// * `clusters` - Each configured context with the nodes it lists
    fn meta_with(client: &Client, scaler: bool, clusters: &[(&str, &[&str])]) -> ClusterMeta {
        // build a chart cluster with or without its k8s scaler
        let mut spec = full_spec();
        if !scaler {
            spec["components"]
                .as_object_mut()
                .expect("components")
                .remove("scaler");
        }
        let mut meta = meta_for(namespaced_cluster(spec), client);
        // replace the sample k8s config with the requested contexts
        let raw = clusters
            .iter()
            .map(|(context, nodes)| {
                (
                    (*context).to_owned(),
                    serde_json::json!({"alias": context, "nodes": nodes}),
                )
            })
            .collect::<serde_json::Map<String, serde_json::Value>>();
        meta.conf.thorium.scaler.k8s.clusters =
            serde_json::from_value(serde_json::Value::Object(raw)).expect("clusters parse");
        meta
    }

    /// A cluster listing a node wins over ones using every node, whatever order they come in
    #[tokio::test]
    async fn explicit_node_wins_in_any_order() {
        // two clusters using every node and one listing the node
        let client = FakeKube::default().client();
        let every_a = meta_with(&client, true, &[("ctx", &[])]);
        let every_b = meta_with(&client, true, &[("ctx", &[])]);
        let explicit = meta_with(&client, true, &[("ctx", &["node-a"])]);
        // the explicit cluster wins whether it comes first or last
        let first = [(&explicit, "explicit"), (&every_a, "a"), (&every_b, "b")];
        assert_eq!(
            match_node(first, "ctx", "node-a").expect("match"),
            NodeMatch::Found("explicit")
        );
        let last = [(&every_a, "a"), (&every_b, "b"), (&explicit, "explicit")];
        assert_eq!(
            match_node(last, "ctx", "node-a").expect("match"),
            NodeMatch::Found("explicit")
        );
        // without it the two clusters using every node are ambiguous
        assert!(match_node([(&every_a, "a"), (&every_b, "b")], "ctx", "node-a").is_err());
        // a node left out of a listing cluster is ignored
        assert_eq!(
            match_node([(&explicit, "explicit")], "ctx", "node-b").expect("match"),
            NodeMatch::Ignored
        );
    }

    /// A kube context no scaler is configured for is reported as a mismatch, not an error
    #[tokio::test]
    async fn unconfigured_context_reported() {
        // a cluster whose scaler is keyed by another context name
        let client = FakeKube::default().client();
        let meta = meta_with(&client, true, &[("prod-admin@prod", &[])]);
        assert_eq!(
            match_node([(&meta, ())], IN_CLUSTER_CONTEXT, "node-a").expect("match"),
            NodeMatch::Unconfigured {
                configured: vec!["prod-admin@prod".to_owned()]
            }
        );
        // without a k8s scaler nothing wants the node
        let no_scaler = meta_with(&client, false, &[("prod-admin@prod", &[])]);
        assert_eq!(
            match_node([(&no_scaler, ())], IN_CLUSTER_CONTEXT, "node-a").expect("match"),
            NodeMatch::Ignored
        );
        // the warning names both contexts and how to fix the mismatch
        let message = context_mismatch_message(IN_CLUSTER_CONTEXT, &["prod-admin@prod".to_owned()]);
        assert!(message.contains("'kubernetes-admin@cluster.local'"));
        assert!(message.contains("[prod-admin@prod]"));
        assert!(message.contains("operator.cluster.scaler.k8s.context"));
    }

    /// A warning is only logged the first time its message is seen
    #[test]
    fn warnings_logged_once() {
        // the first warning is logged and a repeat isn't
        let shared = SharedInfo::default();
        assert!(shared.first_warning("mismatch a"));
        assert!(!shared.first_warning("mismatch a"));
        // a changed message is logged again
        assert!(shared.first_warning("mismatch b"));
    }

    /// Clusters are keyed by context, namespace, and name so equal names in two namespaces or
    /// two kube contexts differ
    #[test]
    fn shared_info_key() {
        // the key joins the context, namespace, and name
        assert_eq!(SharedInfo::key("ctx", "thorium", "dev"), "ctx/thorium/dev");
        assert_ne!(
            SharedInfo::key("ctx", "a", "dev"),
            SharedInfo::key("ctx", "b", "dev")
        );
        // the same namespace and name in two contexts don't collide
        assert_ne!(
            SharedInfo::key("east", "thorium", "thorium"),
            SharedInfo::key("west", "thorium", "thorium")
        );
    }
}
