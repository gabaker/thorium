use thorium::{
    Error, Thorium,
    models::{NodeRegistration, Resources},
};

use crate::k8s::clusters::ClusterMeta;

/// Add worker nodes to Thorium
///
/// Each Thorium worker must be added to Thorium via the API with an empty resource spec before
/// the scaler can schedule pods on them.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `thorium` - The Thorium client being used for API interactions
pub async fn add_nodes_to_thorium(meta: &ClusterMeta, thorium: &Thorium) -> Result<(), Error> {
    // register nodes with empty resources; their agents report the real ones later
    let resources = Resources::default();
    // add each cluster's nodes to Thorium
    let clusters = meta.conf.thorium.scaler.k8s.clusters.clone();
    for (name, k8s_cluster) in &clusters {
        // get the node names in our conf, or every node when none are listed
        let nodes = crate::k8s::nodes::resolve_nodes(meta, k8s_cluster).await?;
        // Thorium knows a cluster by its alias when it has one
        let cluster = k8s_cluster.alias.clone().unwrap_or_else(|| name.clone());
        for node in &nodes {
            // build node registration object
            let node_reg = NodeRegistration {
                cluster: cluster.clone(),
                name: node.clone(),
                resources,
            };
            // register this node, which the API skips if it is already registered
            let reg_result = thorium.system.register_node(&node_reg).await?;
            // return any non-success result codes
            if reg_result.status() != 201 {
                return Err(Error::new(format!(
                    "Failed to register node {node} in cluster {cluster}: {}",
                    reg_result.status()
                )));
            }
        }
    }
    // TODO: add nodes for baremetal clusters?
    Ok(())
}
