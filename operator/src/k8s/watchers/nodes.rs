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

/// How long in minutes to wait before checking on a node that is down or unreachable again;
/// a node becoming ready changes its status, which reconciles it sooner
const UNAVAILABLE_REQUEUE_MINS: u64 = 5;

/// The context for our node watcher
#[derive(Clone)]
struct NodeWatchContext {
    /// The name of the k8s cluster this node is from
    k8s_name: String,
    /// The shared thorium operator info
    shared: Arc<SharedInfo>,
}

/// Handle errors in the reconcile process
///
/// # Arguments
///
/// * `node` - The node whose reconcile failed
/// * `error` - The error the reconcile failed with
/// * `_state` - The node watcher's context
// the controller hands every error policy its object by value
#[allow(clippy::needless_pass_by_value)]
fn node_error_policy(node: Arc<Node>, error: &Error, _state: Arc<NodeWatchContext>) -> Action {
    // log the failure and when the node is retried
    let name = node.metadata.name.as_deref().unwrap_or_default();
    println!("Controller error:\n\t{error}");
    println!(
        "Requeuing node provision reconciliation of {name} in {} seconds",
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
    let Some(name) = node.metadata.name.as_ref() else {
        return Err(Error::new("Found a node without a name"));
    };
    // get this nodes labels so we can check if its already been enabled/disabled for Thorium
    if let Some(labels) = &node.metadata.labels {
        // get the info for the cluster that schedules on this node, ignoring a node no cluster
        // uses
        let Some(info) = ctx.shared.get_for_node(&ctx.k8s_name, name)? else {
            return Ok(Action::requeue(Duration::from_mins(15)));
        };
        // leave this node alone while its cluster is being deleted
        if !info.is_live().await? {
            return Ok(Action::requeue(Duration::from_secs(60)));
        }
        // get the current version of the api
        let api_version = info.thorium.updates.get_version().await?;
        // check if this node already has a Thorium enabled label
        if let Some(is_enabled) = labels.get("thorium") {
            // act on whether this node is enabled or disabled
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
                        "Node {name} has unknown 'thorium' label '{unknown}'. Expected values are 'enabled' or 'disabled'"
                    )));
                }
            }
        }
        // a down or unreachable node can't run its provision pod, so check it again once it
        // reports a change (such as becoming ready) or after a while
        if let Some(reason) = crate::k8s::nodes::node_unavailable(&node) {
            println!("Not provisioning node {name} until it is ready as {reason}");
            return Ok(Action::requeue(Duration::from_mins(
                UNAVAILABLE_REQUEUE_MINS,
            )));
        }
        // provision this node with the API's version, replacing any outdated provision pod
        let version = api_version.thorium.to_string();
        crate::k8s::nodes::deploy_provision_pod(&info.meta, name, &version).await?;
        // label this node with its new version
        crate::k8s::nodes::label_node(&info.meta, name, &version).await?;
        // don't scan this node for 15 minutes
        return Ok(Action::requeue(Duration::from_mins(15)));
    }
    // a node without any labels is checked again later
    println!("{name} doesn't have any labels?");
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
    // create a controller to watch for changes in our nodes, draining its results since
    // reconcile and the error policy already log every outcome
    Controller::new(node_api, Config::default().any_semantic())
        .shutdown_on_signal()
        .run(reconcile_nodes, node_error_policy, Arc::new(ctx))
        .for_each(|_| futures::future::ready(()))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::clusters::tests::{
        FakeKube, TEST_CONTEXT, full_spec, meta_for, namespaced_cluster,
    };

    /// Build a ready node with the given labels
    ///
    /// # Arguments
    ///
    /// * `labels` - The labels the node carries
    fn node(labels: &serde_json::Value) -> Arc<Node> {
        Arc::new(
            serde_json::from_value(serde_json::json!({
                "metadata": {"name": "node-a", "labels": labels},
                "status": {"conditions": [{"type": "Ready", "status": "True"}]}
            }))
            .expect("node"),
        )
    }

    /// A node whose cluster was dropped from the shared info (such as one held by the upgrade
    /// gate) is left alone, while the same node is acted on while the cluster is present
    #[tokio::test]
    async fn forgotten_cluster_leaves_node_alone() {
        // a cluster that schedules on every node of the test context
        let fake = FakeKube::default();
        let meta = meta_for(namespaced_cluster(full_spec()), &fake.client());
        let shared = Arc::new(crate::k8s::controller::tests::shared_with(meta).await);
        let ctx = Arc::new(NodeWatchContext {
            k8s_name: TEST_CONTEXT.to_owned(),
            shared: shared.clone(),
        });
        let labels = serde_json::json!({"thorium": "enabled", "thorium_version": "1.8.0"});
        // while the cluster is present its live state is checked before acting on the node
        let action = reconcile_nodes(node(&labels), ctx.clone())
            .await
            .expect("reconcile");
        assert_eq!(action, Action::requeue(Duration::from_secs(60)));
        assert_eq!(fake.requests().len(), 1, "{:?}", fake.requests());
        // once the cluster is dropped the node is left alone without any kube request
        shared
            .info
            .pin()
            .remove(&SharedInfo::key(TEST_CONTEXT, "thorium", "thorium"));
        let action = reconcile_nodes(node(&labels), ctx)
            .await
            .expect("reconcile");
        assert_eq!(action, Action::requeue(Duration::from_secs(30)));
        assert_eq!(fake.requests().len(), 1, "{:?}", fake.requests());
        assert_eq!(fake.writes().len(), 0, "{:?}", fake.writes());
    }
}
