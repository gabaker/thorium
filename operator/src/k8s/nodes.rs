use futures::future::try_join_all;
use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::runtime::{conditions, wait::await_condition};
use sha2::{Digest, Sha256};
use thorium::conf::K8sCluster;
use thorium::{Error, Thorium};

use super::clusters::ClusterMeta;

/// Where the tracing config is mounted in node provision pods
const TRACING_MOUNT_PATH: &str = "/tmp/tracing.yml";

/// How long in seconds to wait for a deleted provision pod to disappear
const POD_DELETE_TIMEOUT_SECS: u64 = 120;

/// The `app` label on every provision pod
const PROVISION_APP_LABEL: &str = "node-provisioner";

/// The label naming the `ThoriumCluster` a provision pod belongs to
const CLUSTER_LABEL: &str = "thorium.sandia.gov/cluster";

/// The annotation holding the hash of the template a provision pod was built from
const POD_HASH_ANNOTATION: &str = "thorium.sandia.gov/pod-hash";

/// The annotation holding the Thorium version the API reported when a provision pod was built
const THORIUM_VERSION_ANNOTATION: &str = "thorium.sandia.gov/thorium-version";

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

/// Label a node with the version of Thorium provisioned on it
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `node` - The name of the node to label
/// * `version` - The version of Thorium provisioned on this node
pub async fn label_node(meta: &ClusterMeta, node: &str, version: &str) -> Result<(), Error> {
    println!("labeling {node} with thorium_version={version}");
    // build a label json template
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
        Err(error) => Err(Error::new(format!("Failed to label node {node}: {error}"))),
    }
}

/// Label Thorium worker nodes in a kubernetes cluster
///
/// Before the Thorium scaler can schedule reactions to run on k8s nodes, those nodes
/// must be labeled with thorium=enabled via the nodes k8s API. This method will label
/// each node listed under the scaler/k8s section of the CRD config.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
pub async fn label_all_nodes(meta: &ClusterMeta, thorium: &Thorium) -> Result<(), Error> {
    // get the version of Thorium that is being deployed for this node
    let version = thorium.updates.get_version().await?.thorium.to_string();
    // label each node in our local clusters
    for node in resolve_local_nodes(meta).await? {
        // label this node
        label_node(meta, &node, &version).await?;
    }
    Ok(())
}

/// Remove Thorium worker node labels
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
pub async fn delete_node_labels(meta: &ClusterMeta) -> Result<(), Error> {
    let params = PatchParams::default();
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
    for node in resolve_local_nodes(meta).await? {
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
/// Returns the name and uid of the deleted pod if it still has to terminate.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata for a specific cluster
/// * `node` - The name of the node to delete a provision pod for
async fn delete_provision_pod(
    meta: &ClusterMeta,
    node: &str,
) -> Result<Option<(String, String)>, Error> {
    // build provisioner pod name
    let name = format!("node-provisioner-{node}");
    let params = DeleteParams::default();
    // attempt deletion of the pod
    match meta.pod_api.delete(&name, &params).await {
        Ok(deleted) => {
            println!("Cleaning up {name} pod");
            // a pod that is still terminating has to be waited on
            Ok(deleted
                .left()
                .and_then(|pod| pod.metadata.uid)
                .map(|uid| (name, uid)))
        }
        // don't fail if the pod doesn't exist, thats the desired state
        Err(kube::Error::Api(error)) if error.code == 404 => Ok(None),
        Err(error) => Err(Error::new(format!("Failed to delete {name} pod: {error}"))),
    }
}

/// Cleanup a specific nodes provision pod if it exists
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata for a specific cluster
/// * `node` - The name of the node to cleanup a provision pod for
pub async fn cleanup_provision_pod_specific(meta: &ClusterMeta, node: &str) -> Result<(), Error> {
    // delete this node's provision pod
    if let Some((name, uid)) = delete_provision_pod(meta, node).await? {
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
/// paths.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
pub async fn cleanup_provision_pods(meta: &ClusterMeta) -> Result<(), Error> {
    // delete the provision pod on every node in our local clusters
    let mut deleted = Vec::new();
    for node in resolve_local_nodes(meta).await? {
        // delete this node's provision pod and track it if it is still terminating
        if let Some(pod) = delete_provision_pod(meta, &node).await? {
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
    let pod_name = format!("node-provisioner-{node}");
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
                    "args": ["provision", "node", "--keys", "/keys/keys.yml", "--k8s"],
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
                        "name": "tracing-conf"
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
    let pod_name = format!("node-provisioner-{node}");
    // leave an existing pod alone if it was built from the same template
    if let Some(existing) = meta.pod_api.get_opt(&pod_name).await?
        && provision_pod_current(&existing, &pod)
    {
        return Ok(());
    }
    // replace any outdated pod with this name
    cleanup_provision_pod_specific(meta, node).await?;
    // create a provision pod for this node
    match meta.pod_api.create(&PostParams::default(), &pod).await {
        Ok(_) => println!("Node provision pod created: {pod_name}"),
        Err(error) => {
            return Err(Error::new(format!(
                "Failed to create node provision pod: {error}",
            )));
        }
    }
    Ok(())
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

/// Deploy node provision pods to each thorium enabled k8s server
///
/// This function will spawn a node provision pod on each server designated to be used as
/// a compute node within the Thorium. This provision pod will configure the /opt/thorium
/// directory so that the thorium-agent can run jobs on that system. If no nodes are listed
/// then all nodes visible to the operator will be provisioned.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `thorium` - A Thorium client to get the version the API reports with
pub async fn deploy_provision_pods(meta: &ClusterMeta, thorium: &Thorium) -> Result<(), Error> {
    // get the version of Thorium the API reports so a new version replaces the pods
    let version = thorium.updates.get_version().await?.thorium.to_string();
    // provision every node in our local clusters, replacing only outdated pods
    for node in resolve_local_nodes(meta).await? {
        // provision this node
        deploy_provision_pod(meta, &node, &version).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
