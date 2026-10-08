use futures::StreamExt;
use futures::future::try_join_all;
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::Api;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::runtime::{conditions, wait::await_condition};
use sha2::{Digest, Sha256};
use thorium::Error;
use thorium::conf::K8sCluster;

use super::clusters::ClusterMeta;
use super::crds;

/// Where the tracing config is mounted in node provision pods
const TRACING_MOUNT_PATH: &str = "/tmp/tracing.yml";

/// How long in seconds to wait for a deleted provision pod to disappear
const POD_DELETE_TIMEOUT_SECS: u64 = 120;

/// How many times to replace a node's provision pod when another pod keeps taking its name
const PROVISION_CREATE_ATTEMPTS: usize = 2;

/// How many nodes are provisioned at once
const PROVISION_CONCURRENCY: usize = 4;

/// The `app` label on every provision pod
const PROVISION_APP_LABEL: &str = "node-provisioner";

/// The label naming the `ThoriumCluster` a provision pod belongs to
const CLUSTER_LABEL: &str = "thorium.sandia.gov/cluster";

/// The annotation holding the hash of the template a provision pod was built from
const POD_HASH_ANNOTATION: &str = "thorium.sandia.gov/pod-hash";

/// The annotation holding the Thorium version the API reported when a provision pod was built
const THORIUM_VERSION_ANNOTATION: &str = "thorium.sandia.gov/thorium-version";

/// The longest name a provision pod may have so it fits in a DNS label (and its hostname)
const MAX_POD_NAME_LEN: usize = 63;

/// How many hex characters of a node name's hash suffix a shortened provision pod name
const POD_NAME_HASH_LEN: usize = 8;

/// The taints Kubernetes puts on a node whose kubelet is down, unreachable, or shut down
const DOWN_NODE_TAINTS: [&str; 3] = [
    "node.kubernetes.io/unreachable",
    "node.kubernetes.io/not-ready",
    "node.kubernetes.io/out-of-service",
];

/// A node Thorium uses along with whether it can run pods right now
#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalNode {
    /// The name of the node
    name: String,
    /// Why the node can't run pods right now, if it can't
    unavailable: Option<String>,
}

/// Check why a node can't run pods right now, if it can't
///
/// A node is unavailable when it carries one of the [`DOWN_NODE_TAINTS`] or doesn't report a
/// `True` Ready condition. A pod bound to such a node stays Pending, or stays Terminating once
/// deleted, until its kubelet is back. A cordoned node is still available since provision pods
/// are bound to their node directly rather than scheduled.
///
/// # Arguments
///
/// * `node` - The node to check
pub fn node_unavailable(node: &Node) -> Option<String> {
    // a taint from the node lifecycle controller means the kubelet is down or unreachable
    let taint = node
        .spec
        .as_ref()
        .and_then(|spec| spec.taints.as_ref())
        .into_iter()
        .flatten()
        .find(|taint| DOWN_NODE_TAINTS.contains(&taint.key.as_str()));
    if let Some(taint) = taint {
        return Some(format!("it has the {} taint", taint.key));
    }
    // the node must report that it is ready
    let ready = node
        .status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())
        .into_iter()
        .flatten()
        .find(|condition| condition.type_ == "Ready");
    match ready {
        Some(condition) if condition.status == "True" => None,
        Some(condition) => Some(format!("its Ready condition is {}", condition.status)),
        None => Some("it has not reported a Ready condition".to_owned()),
    }
}

/// Build the name of the provision pod for a node
///
/// Names that would exceed 63 characters are cut short and suffixed with a hash of the full
/// node name so two long node names sharing a prefix still get distinct pods. Provision pods
/// are found by their labels during cleanup, so a shortened name doesn't affect that.
///
/// # Arguments
///
/// * `node` - The name of the node the pod provisions
fn provision_pod_name(node: &str) -> String {
    // short enough names are used as is
    let name = format!("{PROVISION_APP_LABEL}-{node}");
    if name.len() <= MAX_POD_NAME_LEN {
        return name;
    }
    // hash the full node name so the shortened name stays unique
    let hash = format!("{:x}", Sha256::digest(node.as_bytes()));
    // keep as much of the name as fits ahead of the suffix (node names are ASCII DNS names),
    // ending on an alphanumeric so the name stays valid
    let keep = MAX_POD_NAME_LEN - POD_NAME_HASH_LEN - 1;
    let prefix = name.chars().take(keep).collect::<String>();
    let prefix = prefix.trim_end_matches(['-', '.']);
    format!("{prefix}-{}", &hash[..POD_NAME_HASH_LEN])
}

/// Describe a failed request for a node provision pod with just the API server's message
///
/// # Arguments
///
/// * `action` - What the request tried to do (e.g. "create")
/// * `name` - The name of the provision pod
/// * `error` - The error the request failed with
fn pod_request_error(action: &str, name: &str, error: kube::Error) -> Error {
    match error {
        // the API server's own message says why, without kube's error wrapping
        kube::Error::Api(response) => Error::new(format!(
            "Failed to {action} node provision pod {name}: {}",
            response.message
        )),
        // anything else (a timeout or connection error) has no such message
        other => Error::new(format!(
            "Failed to {action} node provision pod {name}: {other}"
        )),
    }
}

/// Resolve the nodes Thorium uses in a k8s cluster
///
/// An empty node list in the config means every node visible to the operator. The operator
/// can only list nodes in its own cluster, so a cluster with an `api_url` only ever resolves
/// to its configured nodes.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `k8s_cluster` - The k8s cluster config to resolve nodes for
pub async fn resolve_nodes(
    meta: &ClusterMeta,
    k8s_cluster: &K8sCluster,
) -> Result<Vec<String>, Error> {
    // use the configured node names when any are listed
    if !k8s_cluster.nodes.is_empty() {
        return Ok(k8s_cluster.nodes.clone());
    }
    // our node api only sees the local cluster so never use its nodes for a remote one
    if k8s_cluster.api_url.is_some() {
        return Ok(Vec::new());
    }
    // list every node in this cluster
    let nodes = meta.node_api.list(&ListParams::default()).await?;
    // keep just the node names
    Ok(nodes
        .items
        .into_iter()
        .filter_map(|node| node.metadata.name)
        .collect())
}

/// Resolve the nodes Thorium uses across every k8s cluster local to the operator
///
/// Clusters with an `api_url` are skipped since the operator can't label or provision
/// nodes in another k8s cluster.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
async fn resolve_local_nodes(meta: &ClusterMeta) -> Result<Vec<String>, Error> {
    // collect the nodes from each local cluster
    let mut nodes = Vec::new();
    for (name, k8s_cluster) in &meta.conf.thorium.scaler.k8s.clusters {
        // skip clusters the operator can't reach
        if k8s_cluster.api_url.is_some() {
            println!("Skipping nodes in {name} as it has a custom API url");
            continue;
        }
        // add this cluster's nodes
        nodes.extend(resolve_nodes(meta, k8s_cluster).await?);
    }
    // a node listed by several clusters only needs to be handled once
    nodes.sort_unstable();
    nodes.dedup();
    Ok(nodes)
}

/// Resolve the nodes Thorium uses across every local k8s cluster along with whether each can
/// run pods right now
///
/// A configured node that doesn't exist in the kube API (such as one that was deleted) is
/// unavailable too, since a pod bound to it is never run.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
async fn resolve_local_node_states(meta: &ClusterMeta) -> Result<Vec<LocalNode>, Error> {
    // resolve the nodes Thorium uses
    let names = resolve_local_nodes(meta).await?;
    // without any nodes there is nothing to check
    if names.is_empty() {
        return Ok(Vec::new());
    }
    // list every node once to check the state of each of ours
    let listed = meta.node_api.list(&ListParams::default()).await?;
    let mut nodes = listed
        .items
        .into_iter()
        .filter_map(|node| node.metadata.name.clone().map(|name| (name, node)))
        .collect::<std::collections::HashMap<String, Node>>();
    // check each of our nodes, treating one the kube API doesn't know as unavailable
    Ok(names
        .into_iter()
        .map(|name| {
            let unavailable = match nodes.remove(&name) {
                Some(node) => node_unavailable(&node),
                None => Some("it does not exist in the Kubernetes API".to_owned()),
            };
            LocalNode { name, unavailable }
        })
        .collect())
}

/// Label a node with the version of Thorium provisioned on it
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `node` - The name of the node to label
/// * `version` - The version of Thorium provisioned on this node
pub async fn label_node(meta: &ClusterMeta, node: &str, version: &str) -> Result<(), Error> {
    println!("labeling {node} with thorium_version={version}");
    // build a label json template; the labels don't name the ThoriumCluster since Thorium
    // supports one deployment per Kubernetes cluster (the operator manages only the oldest
    // ThoriumCluster), so a single cluster ever sets or removes them
    let label = serde_json::json!({
        "metadata": {
            "labels": {
                "thorium": "enabled",
                "thorium_version": &version,
            }
        }
    });
    // patch the node with the new label
    match meta
        .node_api
        .patch(node, &PatchParams::default(), &Patch::Merge(&label))
        .await
    {
        Ok(_) => {
            println!(
                "Node {node} labeled successfully with thorium=enabled and thorium_version={version}"
            );
            Ok(())
        }
        // a node deleted since it was listed has nothing left to label
        Err(kube::Error::Api(error)) if error.code == 404 => {
            println!("Node {node} no longer exists, skipping labelling it");
            Ok(())
        }
        Err(error) => Err(Error::new(format!("Failed to label node {node}: {error}"))),
    }
}

/// Label every available node in our local clusters with the version of Thorium provisioned
///
/// Before the Thorium scaler can schedule reactions to run on k8s nodes, those nodes must be
/// labeled with thorium=enabled. A node that is down or unreachable isn't labelled since it
/// wasn't provisioned. Once it is ready again, the node watcher provisions and labels it if it
/// has no thorium label or one for an older version; a node already labelled with the current
/// version is skipped by the node watcher and handled by the next reconcile of its
/// `ThoriumCluster` instead. A node that failed to provision isn't labelled either, so it keeps
/// whatever labels it had. Labels are never removed from a node here, so a node that goes down
/// keeps its thorium=enabled label.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `version` - The Thorium version the API reports
/// * `failed` - The nodes that failed to provision
pub async fn label_nodes(
    meta: &ClusterMeta,
    version: &str,
    failed: &[String],
) -> Result<(), Error> {
    // label each node in our local clusters
    for node in resolve_local_node_states(meta).await? {
        // a node that can't run its provision pod keeps the labels it has
        if let Some(reason) = &node.unavailable {
            println!("Not labelling node {} as {reason}", node.name);
            continue;
        }
        // a node that failed to provision isn't marked as provisioned
        if failed.contains(&node.name) {
            println!("Not labelling node {} as it failed to provision", node.name);
            continue;
        }
        // label this node
        label_node(meta, &node.name, version).await?;
    }
    Ok(())
}

/// Remove Thorium worker node labels
///
/// A node an admin labelled `thorium=disabled` is left untouched, `thorium_version` included,
/// so the opt-out survives deleting and redeploying Thorium; the node watcher never acts on a
/// disabled node, so its leftover version label has no effect.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
pub async fn delete_node_labels(meta: &ClusterMeta) -> Result<(), Error> {
    let params = PatchParams::default();
    // resolve the nodes Thorium uses
    let names = resolve_local_nodes(meta).await?;
    if names.is_empty() {
        return Ok(());
    }
    // list every node once to find the ones an admin disabled for Thorium
    let disabled = meta
        .node_api
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .filter(|node| {
            node.metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get("thorium"))
                .is_some_and(|label| label == "disabled")
        })
        .filter_map(|node| node.metadata.name)
        .collect::<std::collections::HashSet<String>>();
    // build a patch that removes our label
    let label = serde_json::json!({
        "metadata": {
            "labels": {
                "thorium": null,
                "thorium_version": null,
            }
        }
    });
    // remove the label from each node in our local clusters
    for node in names {
        // keep an admin's opt-out in place
        if disabled.contains(&node) {
            println!("Leaving labels on node {node} as it is disabled for Thorium");
            continue;
        }
        // patch the node to remove the label
        match meta
            .node_api
            .patch(&node, &params, &Patch::Merge(&label))
            .await
        {
            Ok(_) => {
                println!("Patched node to remove labels thorium and thorium_version from {node}");
            }
            // a node that no longer exists has no label to remove
            Err(kube::Error::Api(error)) if error.code == 404 => {
                println!("Node {node} not found to remove label, skipping node update");
            }
            Err(error) => {
                return Err(Error::new(format!(
                    "Failed to remove label {label} from node {node}: {error}"
                )));
            }
        }
    }
    Ok(())
}

/// Wait for a deleted pod to disappear from the k8s API
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata for a specific cluster
/// * `name` - The name of the pod being deleted
/// * `uid` - The uid of the pod being deleted so a replacement isn't mistaken for it
async fn wait_for_pod_deletion(meta: &ClusterMeta, name: &str, uid: &str) -> Result<(), Error> {
    // wait for this pod to be gone
    let deleted = await_condition(meta.pod_api.clone(), name, conditions::is_deleted(uid));
    // give up if the pod takes too long to terminate
    match tokio::time::timeout(
        std::time::Duration::from_secs(POD_DELETE_TIMEOUT_SECS),
        deleted,
    )
    .await
    {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(Error::new(format!(
            "Failed waiting for pod {name} to be deleted: {error}"
        ))),
        Err(_) => Err(Error::new(format!(
            "Timed out waiting for pod {name} to be deleted"
        ))),
    }
}

/// Delete a specific node's provision pod if it exists
///
/// Returns the name and uid of the deleted pod if it still has to terminate. A forced delete
/// removes the pod from the API right away without waiting on its kubelet, so it never has to
/// be waited on.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata for a specific cluster
/// * `node` - The name of the node to delete a provision pod for
/// * `force` - Whether to remove the pod without waiting for its kubelet to stop it
async fn delete_provision_pod(
    meta: &ClusterMeta,
    node: &str,
    force: bool,
) -> Result<Option<(String, String)>, Error> {
    // build provisioner pod name
    let name = provision_pod_name(node);
    // a pod on a down node is never stopped by its kubelet, so it is removed with no grace
    let params = if force {
        DeleteParams::default().grace_period(0)
    } else {
        DeleteParams::default()
    };
    // attempt deletion of the pod
    match meta.pod_api.delete(&name, &params).await {
        Ok(deleted) => {
            println!("Cleaning up {name} pod");
            // a pod that is still terminating has to be waited on unless it was forced out
            Ok(deleted
                .left()
                .filter(|_| !force)
                .and_then(|pod| pod.metadata.uid)
                .map(|uid| (name, uid)))
        }
        // don't fail if the pod doesn't exist, thats the desired state
        Err(kube::Error::Api(error)) if error.code == 404 => Ok(None),
        Err(error) => Err(pod_request_error("delete", &name, error)),
    }
}

/// Cleanup a specific nodes provision pod if it exists
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata for a specific cluster
/// * `node` - The name of the node to cleanup a provision pod for
async fn cleanup_provision_pod_specific(meta: &ClusterMeta, node: &str) -> Result<(), Error> {
    // delete this node's provision pod
    if let Some((name, uid)) = delete_provision_pod(meta, node, false).await? {
        // wait for the pod to be gone so a replacement with the same name can be created
        wait_for_pod_deletion(meta, &name, &uid).await?;
    }
    Ok(())
}

/// Cleanup node provision pods
///
/// This deletes node provision pods as part of a ``ThoriumCluster`` cleanup. Each node that that
/// is used by Thorium to schedule reactions will have have a pod run on it to configure host
/// paths such as /opt/thorium. This method cleans up any pods that have run to provision host
/// paths. The pod of a node that is down or gone is force deleted without waiting, since its
/// kubelet would never finish terminating it and the cluster's deletion would hang.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
pub async fn cleanup_provision_pods(meta: &ClusterMeta) -> Result<(), Error> {
    // delete the provision pod on every node in our local clusters
    let mut deleted = Vec::new();
    for node in resolve_local_node_states(meta).await? {
        // force out the pod of a node that can't stop it
        let force = node.unavailable.is_some();
        // delete this node's provision pod and track it if it is still terminating
        if let Some(pod) = delete_provision_pod(meta, &node.name, force).await? {
            deleted.push(pod);
        }
    }
    // wait for every deleted pod to be gone at once
    try_join_all(
        deleted
            .iter()
            .map(|(name, uid)| wait_for_pod_deletion(meta, name, uid)),
    )
    .await?;
    Ok(())
}

/// Build the labels every provision pod for a cluster carries
///
/// # Arguments
///
/// * `cluster` - The name of the `ThoriumCluster` the pods belong to
fn provision_pod_labels(cluster: &str) -> serde_json::Value {
    serde_json::json!({
        "app.kubernetes.io/managed-by": "thorium-operator",
        "app": PROVISION_APP_LABEL,
        CLUSTER_LABEL: cluster,
    })
}

/// Build the label selector matching every provision pod for a cluster
///
/// # Arguments
///
/// * `cluster` - The name of the `ThoriumCluster` the pods belong to
pub fn provision_pod_selector(cluster: &str) -> String {
    format!("app={PROVISION_APP_LABEL},{CLUSTER_LABEL}={cluster}")
}

/// Build the provision pod for a node
///
/// The pod is annotated with a hash of its own template so an existing pod is only
/// replaced when its image, its spec, or the Thorium version the API reports changes.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `node` - Name of node to provision
/// * `version` - The Thorium version the API reports
fn provision_pod(meta: &ClusterMeta, node: &str, version: &str) -> Result<Pod, Error> {
    // get the api component spec to set environment, we don't need node provisioners having
    // their own CRD config just for environment
    let api_spec = &meta.cluster.spec.components.api;
    let pod_name = provision_pod_name(node);
    // set default resources for node provision pods
    let resources = serde_json::json!({"cpu": "250m", "memory": "250Mi"});
    // reference the chart's pull secret and the one rendered from registry_auth
    let image_pull_secrets = meta.cluster.image_pull_secrets();
    let mut pod_template = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "namespace": meta.namespace,
            "name": pod_name,
            "labels": provision_pod_labels(&meta.name),
        },
        "spec": {
            "containers": [
                {
                    "name": "node-provisioner",
                    "image": meta.cluster.get_image(),
                    "imagePullPolicy": meta.cluster.spec.image_pull_policy.clone(),
                    "command": ["/app/thoradm"],
                    "args": ["provision", "node", "--keys", "/keys/keys.yml"],
                    // the image runs as the unprivileged thorium user, but installing the agent
                    // into the node's /opt needs root
                    "securityContext": {"runAsUser": 0, "runAsGroup": 0},
                    "resources": {
                        "limits": resources.clone(),
                        "requests": resources
                    },
                    "env": api_spec.env.clone(),
                    "volumeMounts": [
                        {
                            "name": "opt-mount",
                            "mountPath": "/opt"
                        },
                        {
                            "name": "keys",
                            "mountPath": "/keys/keys.yml",
                            "subPath": "keys.yml"
                        },
                        {
                            "name": "tracing",
                            "mountPath": TRACING_MOUNT_PATH.to_string(),
                            "subPath": "tracing.yml"
                        }
                    ]
                }
            ],
            "nodeName": node,
            "restartPolicy": "OnFailure",
            "volumes": [
                {
                    "name": "opt-mount",
                    "hostPath": {
                        "path": "/opt",
                        "type": "Directory"
                    }
                },
                {
                    "name": "keys",
                    "secret": {
                        "secretName": "keys"
                    }
                },
                {
                    "name": "tracing",
                    "configMap": {
                        "name": super::config_maps::TRACING_CONFIG_MAP
                    }
                }
            ],
            "imagePullSecrets": image_pull_secrets
        }
    });
    // record the version and the template hash so a pod is only replaced when it would change
    annotate_provision_pod(&mut pod_template, version)?;
    Ok(serde_json::from_value(pod_template)?)
}

/// Annotate a provision pod template with the Thorium version and the hash of the result
///
/// The version is part of the hashed template, so a same-tag image push that changes the
/// version the API reports also replaces the provision pods that install the agent.
///
/// # Arguments
///
/// * `pod_template` - The provision pod template to annotate
/// * `version` - The Thorium version the API reports
fn annotate_provision_pod(
    pod_template: &mut serde_json::Value,
    version: &str,
) -> Result<(), serde_json::Error> {
    // record the version this pod provisions so it is hashed with the rest of the template
    pod_template["metadata"]["annotations"] =
        serde_json::json!({ THORIUM_VERSION_ANNOTATION: version });
    // hash the template and record the hash next to the version
    let hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&*pod_template)?));
    pod_template["metadata"]["annotations"][POD_HASH_ANNOTATION] = serde_json::json!(hash);
    Ok(())
}

/// Check whether an existing provision pod can be kept in place of a freshly built one
///
/// # Arguments
///
/// * `existing` - The provision pod that already exists
/// * `desired` - The provision pod the node should have
fn provision_pod_current(existing: &Pod, desired: &Pod) -> bool {
    // a terminating pod is replaced, and any other pod is kept only if built from the same template
    existing.metadata.deletion_timestamp.is_none()
        && pod_hash(existing).is_some()
        && pod_hash(existing) == pod_hash(desired)
}

/// Get the template hash a pod was created with
///
/// # Arguments
///
/// * `pod` - The pod to get the hash of
fn pod_hash(pod: &Pod) -> Option<&str> {
    pod.metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(POD_HASH_ANNOTATION))
        .map(String::as_str)
}

/// Deploy a single node provision pod unless an identical one already exists
///
/// Node provision pods configure the /opt/thorium directory so that the Thorium agent
/// can run jobs on the system. The directory includes a tracing.yml config to enable
/// agent logging and the agent binary itself. The provision command itself is part of
/// the thorium admin binary thoradm. An existing pod built from the same template is left
/// alone; any other pod with the same name is replaced.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `node` - Name of node to deploy pod
/// * `version` - The Thorium version the API reports
pub async fn deploy_provision_pod(
    meta: &ClusterMeta,
    node: &str,
    version: &str,
) -> Result<(), Error> {
    // build the pod this node should have
    let pod = provision_pod(meta, node, version)?;
    let pod_name = provision_pod_name(node);
    // leave an existing pod alone if it was built from the same template
    let existing = meta
        .pod_api
        .get_opt(&pod_name)
        .await
        .map_err(|error| pod_request_error("read", &pod_name, error))?;
    if let Some(existing) = existing
        && provision_pod_current(&existing, &pod)
    {
        return Ok(());
    }
    // replace any outdated pod, retrying if another pod with this name appears in between
    for _ in 0..PROVISION_CREATE_ATTEMPTS {
        // replace any outdated pod with this name
        cleanup_provision_pod_specific(meta, node).await?;
        // create a provision pod for this node, stopping once it or an identical one exists
        if create_provision_pod(meta, &pod).await? == Created::Current {
            return Ok(());
        }
    }
    // give up for now so the reconcile requeues and tries again later
    Err(Error::new(format!(
        "Node provision pod {pod_name} kept being recreated with a different spec while it was \
         being replaced"
    )))
}

/// How creating a provision pod went
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Created {
    /// The pod was created, or an identical one already exists
    Current,
    /// A pod with the same name but another spec exists, so it must be replaced
    Outdated,
}

/// Create a node's provision pod, checking any pod that already exists with its name
///
/// A pod with the same name can appear between deleting the old one and creating the new one
/// (another operator or a reconcile racing this one), so an existing pod only counts as
/// created when it was built from the same template.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pod` - The provision pod the node should have
async fn create_provision_pod(meta: &ClusterMeta, pod: &Pod) -> Result<Created, Error> {
    // get the name of the pod to create
    let name = pod.metadata.name.as_deref().unwrap_or_default();
    // create the pod
    match meta.pod_api.create(&PostParams::default(), pod).await {
        Ok(_) => {
            println!("Node provision pod created: {name}");
            Ok(Created::Current)
        }
        // another pod took this name, so check whether it is the one we wanted
        Err(kube::Error::Api(error)) if error.reason == "AlreadyExists" => {
            let existing = meta
                .pod_api
                .get_opt(name)
                .await
                .map_err(|error| pod_request_error("read", name, error))?;
            match existing {
                Some(existing) if provision_pod_current(&existing, pod) => Ok(Created::Current),
                // a different pod, or one already gone again, still needs replacing
                _ => {
                    println!("Node provision pod {name} already exists with another spec");
                    Ok(Created::Outdated)
                }
            }
        }
        Err(error) => Err(pod_request_error("create", name, error)),
    }
}

/// Delete every provision pod belonging to a `ThoriumCluster`
///
/// This matches pods by label so it doesn't need the cluster's config.
///
/// # Arguments
///
/// * `pod_api` - The Pod API for the `ThoriumCluster`'s namespace
/// * `cluster` - The name of the `ThoriumCluster`
pub async fn delete_provision_pods_by_label(
    pod_api: &Api<Pod>,
    cluster: &str,
) -> Result<(), Error> {
    // match every provision pod for this cluster
    let params = ListParams::default().labels(&provision_pod_selector(cluster));
    // delete them all at once
    pod_api
        .delete_collection(&DeleteParams::default(), &params)
        .await
        .map_err(|error| {
            Error::new(format!(
                "Failed to delete provision pods for {cluster}: {error}"
            ))
        })?;
    Ok(())
}

/// The nodes that failed to provision and why
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProvisionFailures {
    /// Each node that failed to provision along with its error
    pub nodes: Vec<(String, String)>,
}

impl ProvisionFailures {
    /// Get the names of the nodes that failed to provision
    pub fn names(&self) -> Vec<String> {
        // keep just the node names
        self.nodes.iter().map(|(name, _)| name.clone()).collect()
    }

    /// Describe every node that failed to provision, or None when none did
    pub fn message(&self) -> Option<String> {
        // nothing to report when every node was provisioned
        if self.nodes.is_empty() {
            return None;
        }
        // list each failed node with its error
        let details = self
            .nodes
            .iter()
            .map(|(name, error)| format!("node {name}: {error}"))
            .collect::<Vec<String>>();
        Some(format!("Failed to provision nodes: {}", details.join("; ")))
    }
}

/// Provision every available node in our local clusters, replacing only outdated pods
///
/// Node provision pods configure the /opt/thorium directory so that the thorium-agent can run
/// jobs on that system. If no nodes are listed then all nodes visible to the operator are
/// provisioned. A node that is down, unreachable, or gone is skipped and its existing provision
/// pod is left alone: a pod bound to it would stay Pending, and replacing one would wait on a
/// deletion its kubelet never finishes. Once it is ready again, the node watcher provisions it if
/// it has no thorium label or one for an older version; a node already labelled with the
/// current version is provisioned by the next reconcile of its `ThoriumCluster` instead (on an
/// operator restart, a spec, Secret, or banner change, a component's availability changing, or
/// the daily requeue). Up to [`PROVISION_CONCURRENCY`] nodes are provisioned at once, so one
/// node waiting on its old pod's deletion doesn't hold up the rest. A node that fails to
/// provision doesn't stop the others from being provisioned; every failure is returned
/// together afterwards, sorted by node name. Only failing to list the nodes is an error.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `version` - The Thorium version the API reports
pub async fn provision_nodes(
    meta: &ClusterMeta,
    version: &str,
) -> Result<ProvisionFailures, Error> {
    // leave nodes that can't run pods until they are ready again
    let available = resolve_local_node_states(meta)
        .await?
        .into_iter()
        .filter(|node| match &node.unavailable {
            Some(reason) => {
                println!(
                    "Not provisioning node {} as {reason}; it is provisioned once it is ready",
                    node.name
                );
                false
            }
            None => true,
        })
        .collect::<Vec<LocalNode>>();
    // provision a few nodes at a time since replacing an outdated pod can wait minutes on its
    // deletion, collecting each failure without stopping the other nodes
    let mut nodes = futures::stream::iter(available)
        .map(|node| async move {
            // provision this node and keep its error if it fails
            match deploy_provision_pod(meta, &node.name, version).await {
                Ok(()) => None,
                Err(error) => {
                    println!("Failed to provision node {}: {error}", node.name);
                    // keep the plain message since the status already marks the cluster as
                    // errored
                    Some((node.name, crds::error_message(&error)))
                }
            }
        })
        .buffer_unordered(PROVISION_CONCURRENCY)
        .filter_map(futures::future::ready)
        .collect::<Vec<(String, String)>>()
        .await;
    // report failures in node order whatever order the nodes finished in
    nodes.sort_unstable();
    Ok(ProvisionFailures { nodes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::clusters::tests::{
        FakeKube, full_spec, kube_status, meta_for, namespaced_cluster,
    };

    /// Build a minimal provision pod template for a node
    ///
    /// # Arguments
    ///
    /// * `image` - The image the pod runs
    fn template(image: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": "node-provisioner-a", "namespace": "thorium"},
            "spec": {"nodeName": "a", "containers": [{"name": "node-provisioner", "image": image}]}
        })
    }

    /// Build an annotated provision pod
    ///
    /// # Arguments
    ///
    /// * `image` - The image the pod runs
    /// * `version` - The Thorium version the API reports
    fn annotated(image: &str, version: &str) -> Pod {
        let mut pod = template(image);
        annotate_provision_pod(&mut pod, version).expect("annotate");
        serde_json::from_value(pod).expect("pod should deserialize")
    }

    /// The provision pod hash covers the API-reported version as well as the template
    #[test]
    fn provision_pod_hash_includes_version() {
        // the same template and version hash the same
        let pod = annotated("thorium:main", "1.2.3");
        assert_eq!(
            pod_hash(&pod),
            pod_hash(&annotated("thorium:main", "1.2.3"))
        );
        // the version is recorded on the pod
        let annotations = pod.metadata.annotations.clone().expect("annotations");
        assert_eq!(annotations[THORIUM_VERSION_ANNOTATION], "1.2.3");
        // a new version under the same image tag changes the hash
        assert_ne!(
            pod_hash(&pod),
            pod_hash(&annotated("thorium:main", "1.2.4"))
        );
        // a new image changes the hash
        assert_ne!(
            pod_hash(&pod),
            pod_hash(&annotated("thorium:next", "1.2.3"))
        );
    }

    /// An existing provision pod is kept only when it matches and isn't terminating
    #[test]
    fn provision_pod_skip_decision() {
        // a pod built from the same template and version is kept
        let desired = annotated("thorium:main", "1.2.3");
        assert!(provision_pod_current(
            &annotated("thorium:main", "1.2.3"),
            &desired
        ));
        // a pod built for an older version is replaced
        assert!(!provision_pod_current(
            &annotated("thorium:main", "1.2.2"),
            &desired
        ));
        // a pod without a hash is replaced
        let unhashed: Pod = serde_json::from_value(template("thorium:main")).expect("pod");
        assert!(!provision_pod_current(&unhashed, &desired));
        // a matching pod that is terminating is replaced
        let mut terminating = annotated("thorium:main", "1.2.3");
        terminating.metadata.deletion_timestamp = Some(
            k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(chrono::Utc::now()),
        );
        assert!(!provision_pod_current(&terminating, &desired));
    }

    /// A node's provision pod runs thoradm on that node and is found by the cluster's selector
    #[tokio::test]
    async fn provision_pod_shape() {
        // build the provision pod for a node of a chart cluster
        let fake = crate::k8s::clusters::tests::FakeKube::default();
        let meta = crate::k8s::clusters::tests::meta_for(
            crate::k8s::clusters::tests::namespaced_cluster(
                crate::k8s::clusters::tests::full_spec(),
            ),
            &fake.client(),
        );
        let pod = provision_pod(&meta, "node-a", "1.8.1").expect("pod");
        assert_eq!(
            pod.metadata.name.as_deref(),
            Some("node-provisioner-node-a")
        );
        // its labels match the selector cleanup deletes provision pods by
        let labels = pod.metadata.labels.clone().expect("labels");
        for pair in provision_pod_selector("thorium").split(',') {
            let (key, value) = pair.split_once('=').expect("key=value");
            assert_eq!(labels.get(key).map(String::as_str), Some(value), "{key}");
        }
        // it runs on the node with the keys and tracing config mounted
        let spec = pod.spec.clone().expect("spec");
        assert_eq!(spec.node_name.as_deref(), Some("node-a"));
        assert_eq!(
            spec.containers[0].image.as_deref(),
            Some("registry/thorium:1.8.1")
        );
        let volumes = spec
            .volumes
            .unwrap_or_default()
            .into_iter()
            .map(|volume| volume.name)
            .collect::<Vec<_>>();
        assert_eq!(volumes, ["opt-mount", "keys", "tracing"]);
        // it runs as root to install the agent, unlike the image's default thorium user
        let context = spec.containers[0].security_context.clone().expect("context");
        assert_eq!(context.run_as_user, Some(0));
        // it records the version it provisions and the hash of its template
        assert!(pod_hash(&pod).is_some());
        assert_eq!(
            pod.metadata.annotations.expect("annotations")[THORIUM_VERSION_ANNOTATION],
            "1.8.1"
        );
    }

    /// The path of the test node's provision pod
    const PROVISION_POD_PATH: &str = "/api/v1/namespaces/thorium/pods/node-provisioner-node-a";

    /// The path provision pods are created at
    const PODS_PATH: &str = "/api/v1/namespaces/thorium/pods";

    /// Build a fake kube API, the metadata of a chart cluster using it, and the provision pod
    /// node `node-a` should have at version 1.8.1
    ///
    /// # Arguments
    ///
    /// * `routes` - Adds the canned answers the test needs
    fn provision_setup(
        routes: impl FnOnce(FakeKube, &Pod) -> FakeKube,
    ) -> (FakeKube, ClusterMeta, Pod) {
        // build the desired pod against a cluster that answers nothing yet
        let cluster = namespaced_cluster(full_spec());
        let desired = provision_pod(
            &meta_for(cluster.clone(), &FakeKube::default().client()),
            "node-a",
            "1.8.1",
        )
        .expect("pod");
        // add the routes and build the metadata the code under test uses
        let fake = routes(FakeKube::default(), &desired);
        let meta = meta_for(cluster, &fake.client());
        (fake, meta, desired)
    }

    /// Count the provision pods created through a fake kube API
    ///
    /// # Arguments
    ///
    /// * `fake` - The fake kube API
    fn creates(fake: &FakeKube) -> usize {
        fake.writes()
            .iter()
            .filter(|request| request.method == "POST" && request.path == PODS_PATH)
            .count()
    }

    /// A pod that already exists with the desired spec counts as created
    #[tokio::test]
    async fn already_existing_identical_pod_is_current() {
        // the create races a pod built from the same template
        let (_fake, meta, desired) = provision_setup(|fake, desired| {
            fake.route("POST", PODS_PATH, 409, kube_status(409, "AlreadyExists"))
                .route(
                    "GET",
                    PROVISION_POD_PATH,
                    200,
                    serde_json::to_value(desired).expect("pod"),
                )
        });
        assert_eq!(
            create_provision_pod(&meta, &desired).await.expect("create"),
            Created::Current
        );
    }

    /// A pod that already exists with another spec, or is gone again, is not assumed created
    #[tokio::test]
    async fn already_existing_different_pod_is_outdated() {
        // the create races a pod built for an older version
        let older = provision_pod(
            &meta_for(
                namespaced_cluster(full_spec()),
                &FakeKube::default().client(),
            ),
            "node-a",
            "1.8.0",
        )
        .expect("pod");
        let (_fake, meta, desired) = provision_setup(|fake, _| {
            fake.route("POST", PODS_PATH, 409, kube_status(409, "AlreadyExists"))
                .route(
                    "GET",
                    PROVISION_POD_PATH,
                    200,
                    serde_json::to_value(&older).expect("pod"),
                )
        });
        assert_eq!(
            create_provision_pod(&meta, &desired).await.expect("create"),
            Created::Outdated
        );
        // a pod deleted again before it could be read still needs creating
        let (_fake, meta, desired) = provision_setup(|fake, _| {
            fake.route("POST", PODS_PATH, 409, kube_status(409, "AlreadyExists"))
        });
        assert_eq!(
            create_provision_pod(&meta, &desired).await.expect("create"),
            Created::Outdated
        );
        // any other failure is an error carrying just the API server's message
        let (_fake, meta, desired) = provision_setup(|fake, _| {
            fake.route("POST", PODS_PATH, 403, kube_status(403, "Forbidden"))
        });
        let error = create_provision_pod(&meta, &desired)
            .await
            .expect_err("forbidden");
        assert_eq!(
            crds::error_message(&error),
            "Failed to create node provision pod node-provisioner-node-a: fake Forbidden"
        );
    }

    /// A pod name another pod keeps taking is retried a bounded number of times and then
    /// left to the next reconcile
    #[tokio::test]
    async fn racing_pod_is_retried_then_requeued() {
        // the name always holds a pod built from another template
        let mut other = serde_json::to_value(provision_setup(|fake, _| fake).2).expect("pod");
        other["metadata"]["annotations"][POD_HASH_ANNOTATION] = serde_json::json!("other");
        let (fake, meta, _) = provision_setup(|fake, _| {
            fake.route("POST", PODS_PATH, 409, kube_status(409, "AlreadyExists"))
                .route("GET", PROVISION_POD_PATH, 200, other)
        });
        // provisioning fails after replacing the pod on each attempt
        let error = deploy_provision_pod(&meta, "node-a", "1.8.1")
            .await
            .expect_err("racing pod");
        assert!(error.to_string().contains("node-provisioner-node-a"));
        assert_eq!(creates(&fake), PROVISION_CREATE_ATTEMPTS);
        // a fresh create succeeds on the first attempt
        let (fake, meta, _) = provision_setup(|fake, desired| {
            fake.route(
                "POST",
                PODS_PATH,
                201,
                serde_json::to_value(desired).expect("pod"),
            )
        });
        deploy_provision_pod(&meta, "node-a", "1.8.1")
            .await
            .expect("created");
        assert_eq!(creates(&fake), 1);
    }

    /// Provision pod names fit in a DNS label, staying readable and distinct for long nodes
    #[test]
    fn provision_pod_names_fit() {
        // a short node name is kept whole
        assert_eq!(provision_pod_name("node-a"), "node-provisioner-node-a");
        // a name exactly at the limit is kept whole too
        let exact = "n".repeat(MAX_POD_NAME_LEN - "node-provisioner-".len());
        assert_eq!(provision_pod_name(&exact).len(), MAX_POD_NAME_LEN);
        assert!(provision_pod_name(&exact).ends_with(&exact));
        // long node names sharing a prefix get distinct names within the limit
        let long_a = format!("{}.a.example.com", "worker-".repeat(10));
        let long_b = format!("{}.b.example.com", "worker-".repeat(10));
        let (name_a, name_b) = (provision_pod_name(&long_a), provision_pod_name(&long_b));
        assert!(name_a.len() <= MAX_POD_NAME_LEN, "{name_a}");
        assert!(name_b.len() <= MAX_POD_NAME_LEN, "{name_b}");
        assert_ne!(name_a, name_b);
        assert!(name_a.starts_with("node-provisioner-worker-"));
        // the name never has a separator right before the hash suffix
        assert!(!name_a.contains("--") && !name_a.contains(".-"), "{name_a}");
        // the same node always gets the same name
        assert_eq!(provision_pod_name(&long_a), name_a);
    }

    /// A provision pod for a long node name is shortened but still matched by the selector
    #[tokio::test]
    async fn long_node_provision_pod_matches_selector() {
        // build the provision pod for a node with a long name
        let fake = crate::k8s::clusters::tests::FakeKube::default();
        let meta = crate::k8s::clusters::tests::meta_for(
            crate::k8s::clusters::tests::namespaced_cluster(
                crate::k8s::clusters::tests::full_spec(),
            ),
            &fake.client(),
        );
        let node = format!("{}.example.com", "worker".repeat(12));
        let pod = provision_pod(&meta, &node, "1.8.1").expect("pod");
        // the pod name fits while it still runs on the full node name
        let name = pod.metadata.name.clone().expect("name");
        assert!(name.len() <= MAX_POD_NAME_LEN, "{name}");
        assert_eq!(
            pod.spec.as_ref().and_then(|spec| spec.node_name.as_deref()),
            Some(node.as_str())
        );
        // the labels cleanup selects by are unchanged
        let labels = pod.metadata.labels.clone().expect("labels");
        for pair in provision_pod_selector("thorium").split(',') {
            let (key, value) = pair.split_once('=').expect("key=value");
            assert_eq!(labels.get(key).map(String::as_str), Some(value), "{key}");
        }
    }

    /// The path nodes are listed at
    const NODES_PATH: &str = "/api/v1/nodes";

    /// Build a node with a Ready condition status and taints
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the node
    /// * `ready` - The status of its Ready condition, if it reports one
    /// * `taints` - The keys of its `NoExecute` taints
    fn node(name: &str, ready: Option<&str>, taints: &[&str]) -> serde_json::Value {
        // build the taints the node lifecycle controller would add
        let taints = taints
            .iter()
            .map(|key| serde_json::json!({"key": key, "effect": "NoExecute"}))
            .collect::<Vec<_>>();
        // report the Ready condition when the node has one
        let conditions = ready.map_or_else(Vec::new, |status| {
            vec![serde_json::json!({"type": "Ready", "status": status})]
        });
        serde_json::json!({
            "apiVersion": "v1",
            "kind": "Node",
            "metadata": {"name": name},
            "spec": {"taints": taints},
            "status": {"conditions": conditions}
        })
    }

    /// Build a node list answer
    ///
    /// # Arguments
    ///
    /// * `nodes` - The nodes in the list
    fn node_list(nodes: &[serde_json::Value]) -> serde_json::Value {
        serde_json::json!({"apiVersion": "v1", "kind": "NodeList", "metadata": {}, "items": nodes})
    }

    /// Build the metadata of a chart cluster whose local k8s cluster lists specific nodes
    ///
    /// # Arguments
    ///
    /// * `fake` - The fake kube API the metadata uses
    /// * `nodes` - The nodes the cluster lists (every node when empty)
    fn meta_listing(fake: &FakeKube, nodes: &[&str]) -> ClusterMeta {
        // build the metadata of a chart cluster
        let mut meta = meta_for(namespaced_cluster(full_spec()), &fake.client());
        // list the requested nodes for the in-cluster context
        meta.conf.thorium.scaler.k8s.clusters = serde_json::from_value(serde_json::json!({
            "kubernetes-admin@cluster.local": {"alias": "thorium", "nodes": nodes}
        }))
        .expect("clusters parse");
        meta
    }

    /// Count the requests to a path with a method
    ///
    /// # Arguments
    ///
    /// * `fake` - The fake kube API
    /// * `method` - The HTTP method to count
    /// * `path` - The path to count
    fn count(fake: &FakeKube, method: &str, path: &str) -> usize {
        fake.requests()
            .iter()
            .filter(|request| request.method == method && request.path == path)
            .count()
    }

    /// Build a provision pod for a node that is stuck terminating
    ///
    /// # Arguments
    ///
    /// * `node` - The node the pod was bound to
    fn terminating_pod(node: &str) -> serde_json::Value {
        // build the pod the node should have and mark it as being deleted
        let meta = meta_for(
            namespaced_cluster(full_spec()),
            &FakeKube::default().client(),
        );
        let mut pod = serde_json::to_value(provision_pod(&meta, node, "1.8.1").expect("pod"))
            .expect("pod json");
        pod["metadata"]["uid"] = serde_json::json!(format!("uid-{node}"));
        pod["metadata"]["deletionTimestamp"] = serde_json::json!("2026-01-01T00:00:00Z");
        pod
    }

    /// Ready nodes are available while `NotReady`, `Unknown`, tainted, or silent nodes aren't, and
    /// a cordoned node is still available
    #[test]
    fn node_availability() {
        // build a node from JSON
        let parse = |raw: serde_json::Value| -> Node { serde_json::from_value(raw).expect("node") };
        // a ready node is available
        assert_eq!(node_unavailable(&parse(node("a", Some("True"), &[]))), None);
        // a cordoned ready node is too since provision pods are bound to it directly
        let mut cordoned = node("a", Some("True"), &[]);
        cordoned["spec"]["unschedulable"] = serde_json::json!(true);
        assert_eq!(node_unavailable(&parse(cordoned)), None);
        // an unrelated taint doesn't make a node unavailable
        assert_eq!(
            node_unavailable(&parse(node("a", Some("True"), &["dedicated"]))),
            None
        );
        // a stopped kubelet, an unreachable node, and a node without a Ready condition aren't
        for status in [Some("False"), Some("Unknown"), None] {
            assert!(
                node_unavailable(&parse(node("a", status, &[]))).is_some(),
                "{status:?}"
            );
        }
        // the node lifecycle taints make a node unavailable even before its condition changes
        for taint in DOWN_NODE_TAINTS {
            let reason = node_unavailable(&parse(node("a", Some("True"), &[taint])))
                .expect("tainted node is unavailable");
            assert!(reason.contains(taint), "{reason}");
        }
    }

    /// Down, unreachable, and deleted nodes are skipped by provisioning while ready ones are
    /// provisioned, and a pod stuck terminating on a down node is left alone
    #[tokio::test]
    async fn down_nodes_are_not_provisioned() {
        // one ready node, one unreachable node with a pod stuck terminating, and a listed node
        // that was deleted from the kube API
        let fake = FakeKube::default()
            .route(
                "GET",
                NODES_PATH,
                200,
                node_list(&[
                    node("node-a", Some("True"), &[]),
                    node(
                        "node-b",
                        Some("Unknown"),
                        &["node.kubernetes.io/unreachable"],
                    ),
                ]),
            )
            .route(
                "GET",
                "/api/v1/namespaces/thorium/pods/node-provisioner-node-b",
                200,
                terminating_pod("node-b"),
            )
            .route("POST", PODS_PATH, 201, terminating_pod("node-a"));
        let meta = meta_listing(&fake, &["node-a", "node-b", "node-gone"]);
        // provisioning finishes right away instead of waiting on the stuck pod
        let failures = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            provision_nodes(&meta, "1.8.1"),
        )
        .await
        .expect("provisioning should not wait on a down node")
        .expect("provisioned");
        assert_eq!(failures.message(), None);
        // only the ready node got a provision pod
        let created = fake
            .writes()
            .into_iter()
            .filter(|request| request.method == "POST")
            .map(|request| request.body["spec"]["nodeName"].clone())
            .collect::<Vec<_>>();
        assert_eq!(created, [serde_json::json!("node-a")]);
        // only the ready node's old pod was cleared, so the stuck pod wasn't deleted again
        let deleted = fake
            .writes()
            .into_iter()
            .filter(|request| request.method == "DELETE")
            .map(|request| request.path)
            .collect::<Vec<_>>();
        assert_eq!(deleted, [PROVISION_POD_PATH]);
        // the down and deleted nodes weren't even looked up
        assert_eq!(
            count(
                &fake,
                "GET",
                "/api/v1/namespaces/thorium/pods/node-provisioner-node-b"
            ),
            0
        );
        assert_eq!(
            count(
                &fake,
                "GET",
                "/api/v1/namespaces/thorium/pods/node-provisioner-node-gone"
            ),
            0
        );
    }

    /// Several nodes are provisioned at once, at most [`PROVISION_CONCURRENCY`] at a time, and
    /// failures are reported in node order whatever order the nodes finish in
    #[tokio::test]
    async fn nodes_provision_concurrently() {
        // five ready nodes where reading the pods of the last and second nodes fails
        let names = ["node-a", "node-b", "node-c", "node-d", "node-e"];
        let nodes = names
            .iter()
            .map(|name| node(name, Some("True"), &[]))
            .collect::<Vec<_>>();
        let fake = FakeKube::default()
            .route("GET", NODES_PATH, 200, node_list(&nodes))
            .route(
                "GET",
                "/api/v1/namespaces/thorium/pods/node-provisioner-node-e",
                500,
                kube_status(500, "InternalError"),
            )
            .route(
                "GET",
                "/api/v1/namespaces/thorium/pods/node-provisioner-node-b",
                500,
                kube_status(500, "InternalError"),
            )
            .route("POST", PODS_PATH, 201, terminating_pod("node-a"));
        let meta = meta_listing(&fake, &[]);
        // every failure is collected and sorted by node
        let failures = provision_nodes(&meta, "1.8.1").await.expect("listed");
        assert_eq!(failures.names(), ["node-b", "node-e"]);
        // the other nodes were all provisioned
        assert_eq!(creates(&fake), 3);
        // the first nodes' pods were all read before any of them got past that read, so they
        // were provisioned at once rather than one after another
        let first = fake
            .requests()
            .into_iter()
            .filter(|request| request.path != NODES_PATH)
            .take(PROVISION_CONCURRENCY)
            .map(|request| (request.method, request.path))
            .collect::<Vec<_>>();
        let expected = names[..PROVISION_CONCURRENCY]
            .iter()
            .map(|name| {
                (
                    "GET".to_owned(),
                    format!("/api/v1/namespaces/thorium/pods/node-provisioner-{name}"),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(first, expected);
    }

    /// A node that fails to provision doesn't stop the other nodes from being provisioned
    #[tokio::test]
    async fn failing_node_does_not_block_others() {
        // two ready nodes where reading the first one's pod fails
        let fake = FakeKube::default()
            .route(
                "GET",
                NODES_PATH,
                200,
                node_list(&[
                    node("node-a", Some("True"), &[]),
                    node("node-c", Some("True"), &[]),
                ]),
            )
            .route(
                "GET",
                PROVISION_POD_PATH,
                500,
                kube_status(500, "InternalError"),
            )
            .route("POST", PODS_PATH, 201, terminating_pod("node-c"));
        let meta = meta_listing(&fake, &[]);
        // the failure is reported with the node it happened on
        let failures = provision_nodes(&meta, "1.8.1").await.expect("listed");
        assert_eq!(failures.names(), ["node-a"]);
        let message = failures.message().expect("a failure");
        assert_eq!(
            message,
            "Failed to provision nodes: node node-a: Failed to read node provision pod \
             node-provisioner-node-a: fake InternalError"
        );
        // the other node was still provisioned
        assert_eq!(creates(&fake), 1);
        // only the failed node is left unlabelled
        let fake = fake.route(
            "PATCH",
            "/api/v1/nodes/node-c",
            200,
            node("node-c", Some("True"), &[]),
        );
        let meta = meta_listing(&fake, &[]);
        label_nodes(&meta, "1.8.1", &failures.names())
            .await
            .expect("labelled");
        assert_eq!(count(&fake, "PATCH", "/api/v1/nodes/node-a"), 0);
        assert_eq!(count(&fake, "PATCH", "/api/v1/nodes/node-c"), 1);
    }

    /// Only available nodes are labelled, and a node deleted after it was listed is skipped
    #[tokio::test]
    async fn down_nodes_are_not_labelled() {
        // a ready node, a node whose kubelet stopped, and a ready node deleted before its patch
        let fake = FakeKube::default()
            .route(
                "GET",
                NODES_PATH,
                200,
                node_list(&[
                    node("node-a", Some("True"), &[]),
                    node("node-b", Some("False"), &["node.kubernetes.io/not-ready"]),
                    node("node-c", Some("True"), &[]),
                ]),
            )
            .route(
                "PATCH",
                "/api/v1/nodes/node-a",
                200,
                node("node-a", Some("True"), &[]),
            );
        let meta = meta_listing(&fake, &[]);
        // labelling succeeds even though node-c is gone by the time it is patched
        label_nodes(&meta, "1.8.1", &[]).await.expect("labelled");
        // only the ready nodes were patched
        assert_eq!(count(&fake, "PATCH", "/api/v1/nodes/node-a"), 1);
        assert_eq!(count(&fake, "PATCH", "/api/v1/nodes/node-b"), 0);
        assert_eq!(count(&fake, "PATCH", "/api/v1/nodes/node-c"), 1);
        // any other failure to label is still an error
        let fake = FakeKube::default()
            .route(
                "GET",
                NODES_PATH,
                200,
                node_list(&[node("node-a", Some("True"), &[])]),
            )
            .route(
                "PATCH",
                "/api/v1/nodes/node-a",
                403,
                kube_status(403, "Forbidden"),
            );
        assert!(
            label_nodes(&meta_listing(&fake, &[]), "1.8.1", &[])
                .await
                .is_err()
        );
    }

    /// Cleanup force deletes the provision pods of down or deleted nodes without waiting on
    /// them, so deleting a cluster with a node down doesn't hang
    #[tokio::test]
    async fn cleanup_forces_pods_on_down_nodes() {
        // a ready node without a pod, an unreachable node whose pod would never terminate, and
        // a listed node that was deleted
        let fake = FakeKube::default()
            .route(
                "GET",
                NODES_PATH,
                200,
                node_list(&[
                    node("node-a", Some("True"), &[]),
                    node(
                        "node-b",
                        Some("Unknown"),
                        &["node.kubernetes.io/unreachable"],
                    ),
                ]),
            )
            .route(
                "DELETE",
                "/api/v1/namespaces/thorium/pods/node-provisioner-node-b",
                200,
                terminating_pod("node-b"),
            )
            .route(
                "DELETE",
                "/api/v1/namespaces/thorium/pods/node-provisioner-node-gone",
                200,
                terminating_pod("node-gone"),
            );
        let meta = meta_listing(&fake, &["node-a", "node-b", "node-gone"]);
        // cleanup finishes right away
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            cleanup_provision_pods(&meta),
        )
        .await
        .expect("cleanup should not wait on a down node")
        .expect("cleaned up");
        // the down and deleted nodes' pods were deleted with no grace period, the ready
        // node's pod with the default one
        let grace = |node: &str| {
            fake.writes()
                .into_iter()
                .find(|request| {
                    request.method == "DELETE"
                        && request.path == format!("{PODS_PATH}/node-provisioner-{node}")
                })
                .map(|request| request.body["gracePeriodSeconds"].clone())
                .expect("deleted")
        };
        assert_eq!(grace("node-b"), serde_json::json!(0));
        assert_eq!(grace("node-gone"), serde_json::json!(0));
        assert_eq!(grace("node-a"), serde_json::Value::Null);
        // nothing was watched waiting for a deletion
        assert_eq!(count(&fake, "GET", PODS_PATH), 0);
    }

    /// Cleanup removes the Thorium labels from every node but one an admin disabled, which
    /// keeps all of its labels
    #[tokio::test]
    async fn cleanup_keeps_disabled_node_labels() {
        // an enabled node, a disabled node, and a listed node that was deleted
        let mut enabled = node("node-a", Some("True"), &[]);
        enabled["metadata"]["labels"] =
            serde_json::json!({"thorium": "enabled", "thorium_version": "1.8.1"});
        let mut disabled = node("node-b", Some("True"), &[]);
        disabled["metadata"]["labels"] =
            serde_json::json!({"thorium": "disabled", "thorium_version": "1.8.1"});
        let fake = FakeKube::default()
            .route(
                "GET",
                NODES_PATH,
                200,
                node_list(&[enabled.clone(), disabled]),
            )
            .route("PATCH", "/api/v1/nodes/node-a", 200, enabled);
        let meta = meta_listing(&fake, &["node-a", "node-b", "node-gone"]);
        // the labels are removed
        delete_node_labels(&meta).await.expect("labels removed");
        // only the enabled node and the deleted one (whose 404 is tolerated) were patched
        let patched = fake
            .writes()
            .into_iter()
            .map(|request| (request.path, request.body))
            .collect::<Vec<_>>();
        let unlabel = serde_json::json!({
            "metadata": {"labels": {"thorium": null, "thorium_version": null}}
        });
        assert_eq!(
            patched,
            [
                ("/api/v1/nodes/node-a".to_owned(), unlabel.clone()),
                ("/api/v1/nodes/node-gone".to_owned(), unlabel)
            ]
        );
    }
}
