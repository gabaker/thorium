//! Watches nodes and provisions and labels the ones a Thorium cluster schedules on

use futures::StreamExt;
use k8s_openapi::api::core::v1::Node;
use kube::runtime::Controller;
use kube::runtime::controller::Action;
use kube::runtime::watcher::Config;
use kube::{Api, Client};
use std::sync::Arc;
use std::time::Duration;
use thorium::Error;

use crate::k8s::controller::SharedInfo;

/// The context for our node watcher
#[derive(Clone)]
struct NodeWatchContext {
    /// The name of the k8s cluster this node is from
    k8s_name: String,
    /// The shared thorium operator info
    shared: Arc<SharedInfo>,
}

/// Handle errors in the reconcile process
fn node_error_policy(_cluster: Arc<Node>, error: &Error, _state: Arc<NodeWatchContext>) -> Action {
    println!("Controller error:\n\t{}", error);
    println!(
        "Requeuing node provision reconciliation in {} seconds",
        super::RECONCILE_ERROR_REQUEUE_SECS
    );
    Action::requeue(Duration::from_secs(super::RECONCILE_ERROR_REQUEUE_SECS))
}

/// Provision and label a node for the Thorium cluster that schedules on it
///
/// # Arguments
///
/// * `node` - The node that changed
/// * `ctx` - The node watcher's context
async fn reconcile_nodes(node: Arc<Node>, ctx: Arc<NodeWatchContext>) -> Result<Action, Error> {
    // if we don't have any configs then just requeue this node in 30 seconds
    if ctx.shared.info.is_empty() {
        // don't scan this node for another 30 seconds
        return Ok(Action::requeue(Duration::from_secs(30)));
    }
    // get this nodes name or throw an error if it doesn't have one
    let name = node.metadata.name.as_ref().unwrap();
    // get this nodes labels so we can check if its already been enabled/disabled for Thorium
    if let Some(labels) = &node.metadata.labels {
        // get the info for this nodes k8s cluster
        let info = match ctx.shared.get_for_node(&ctx.k8s_name, name)? {
            // this node is a valid node for Thorium to potentially schedule on
            Some(info) => info,
            // this node has been deliberately excluded from usage in Thorium so ignore it
            None => return Ok(Action::requeue(Duration::from_mins(15))),
        };
        // leave this node alone while its cluster is being deleted
        if !info.is_live().await? {
            return Ok(Action::requeue(Duration::from_secs(60)));
        }
        // get the current version of the api
        let api_version = info.thorium.updates.get_version().await?;
        // check if this node already has a Thorium enabled label
        if let Some(is_enabled) = labels.get("thorium") {
            // if this node is disabld then ignore it
            match is_enabled.as_str() {
                // this node is enabled check its version to see if we need to reprovision it
                "enabled" => {
                    // get the version deployed to this node
                    let node_version = labels.get("thorium_version");
                    // check the version deployed to this node
                    if let Some(node_version) = node_version {
                        // check if this node is already up to date
                        if api_version.compare_thorium(node_version)? {
                            // this node is already up to date so ignore it for 15 minutes
                            return Ok(Action::requeue(Duration::from_mins(15)));
                        }
                        println!(
                            "node {name} is running {node_version} but needs to be updated to {}",
                            api_version.thorium
                        );
                    }
                }
                // this node is disabled so ignore it for 15 minutes
                "disabled" => return Ok(Action::requeue(Duration::from_mins(15))),
                // this node has an unknown thorium enabled label
                unknown => {
                    return Err(Error::new(format!(
                        "Node {:?} has unknown 'thorium' label '{unknown}'. Expected values are 'enabled' or 'disabled'",
                        node.metadata.name
                    )));
                }
            }
        }
        // provision this node with the API's version, replacing any outdated provision pod
        let version = api_version.thorium.to_string();
        crate::k8s::nodes::deploy_provision_pod(&info.meta, name, &version).await?;
        // label this node with its new version
        crate::k8s::nodes::label_node(&info.meta, name, &version).await?;
        // don't scan this node for 15 minutes
        return Ok(Action::requeue(Duration::from_mins(15)));
    }
    println!("{:?} doesn't have any labels?", node.metadata.name);
    Ok(Action::requeue(Duration::from_secs(60)))
}

/// Watch our nodes for any changes
///
/// # Arguments
///
/// * `k8s_name` - The name of the k8s cluster these nodes are in
/// * `client` - The kube client to watch with
/// * `shared` - Data shared across watchers
pub async fn start(k8s_name: String, client: Client, shared: Arc<SharedInfo>) {
    // build a node api
    let node_api: Api<Node> = Api::<Node>::all(client);
    // setup some state for our watcher
    let ctx = NodeWatchContext { k8s_name, shared };
    // create a controller to watch for changes in our nodes
    Controller::new(node_api, Config::default().any_semantic())
        .shutdown_on_signal()
        .run(reconcile_nodes, node_error_policy, Arc::new(ctx))
        .filter_map(|x| async move { std::result::Result::ok(x) })
        .for_each(|_| futures::future::ready(()))
        .await;
}
