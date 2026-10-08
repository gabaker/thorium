//! Watches the api pods and applies any required labels

use futures::StreamExt;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{ListParams, Patch, PatchParams};
use kube::runtime::Controller;
use kube::runtime::controller::Action;
use kube::runtime::reflector::Lookup;
use kube::runtime::watcher::Config;
use kube::{Api, Client};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thorium::Error;

use crate::k8s::controller::{SharedInfo, ThoriumInfo};

/// A namespace's designated mcp api pod, if one is designated
///
/// The async lock is held across checking, scanning, labelling, and recording the designation
/// so only one reconcile per namespace acts on it at a time.
type DesignationSlot = Arc<tokio::sync::Mutex<Option<String>>>;

/// The context for our api pod watcher
struct ApiWatchContext {
    /// kube API client
    client: Client,
    /// The name of the kube context `client` reaches its k8s cluster through
    context: String,
    /// The designated mcp api pod slot of each namespace, shared by every reconcile
    ///
    /// Keyed by namespace so a cluster moving to another namespace never leaves a stale
    /// designation from its old namespace in place
    mcp_pods: Mutex<HashMap<String, DesignationSlot>>,
    /// The shared thorium operator info
    shared: Arc<SharedInfo>,
}

/// Handle errors in the reconcile process
///
/// # Arguments
///
/// * `_pod` - The api pod whose reconcile failed
/// * `error` - The error the reconcile failed with
/// * `_state` - The api pod watcher's context
fn api_pod_error_policy(_pod: Arc<Pod>, error: &Error, _state: Arc<ApiWatchContext>) -> Action {
    // log the failure and when the pod is retried
    println!("Controller error:\n\t{error}");
    println!(
        "Requeuing api pod reconciliation in {} seconds",
        super::RECONCILE_ERROR_REQUEUE_SECS
    );
    Action::requeue(Duration::from_secs(super::RECONCILE_ERROR_REQUEUE_SECS))
}

/// Check whether a pod is being deleted
///
/// # Arguments
///
/// * `pod` - The pod to check
fn pod_is_terminating(pod: &Pod) -> bool {
    pod.metadata.deletion_timestamp.is_some()
}

/// Check whether a pod looks like it is on a down or unreachable node
///
/// The kubelet of a down node stops reporting, so its pods keep their last Running phase and
/// ready containers while the node lifecycle controller marks the pod itself as not ready.
/// Such a pod gets no traffic until its node is back or it is evicted minutes later.
///
/// # Arguments
///
/// * `pod` - The pod to check
fn pod_is_unreachable(pod: &Pod) -> bool {
    // a pod without a status hasn't been reported on at all yet
    let Some(status) = &pod.status else {
        return false;
    };
    // the containers still report ready from before the node went down
    let containers_ready = status
        .container_statuses
        .as_ref()
        .is_some_and(|containers| {
            !containers.is_empty() && containers.iter().all(|container| container.ready)
        });
    // while the pod itself was marked not ready
    let pod_unready = status
        .conditions
        .iter()
        .flatten()
        .any(|condition| condition.type_ == "Ready" && condition.status != "True");
    status.phase.as_deref() == Some("Running") && containers_ready && pod_unready
}

/// Check whether a pod is still starting or running and is not being deleted
///
/// # Arguments
///
/// * `pod` - The pod to check
fn pod_is_alive(pod: &Pod) -> bool {
    // a terminating pod still reports Running until it exits, but it is going away (such as
    // during an api rollout) so another pod must take over MCP queries
    if pod_is_terminating(pod) {
        return false;
    }
    // a pod on a down node can't serve MCP queries until its node is back
    if pod_is_unreachable(pod) {
        return false;
    }
    // only a pending or running pod can serve MCP queries
    match pod
        .status
        .as_ref()
        .and_then(|status| status.phase.as_deref())
    {
        // no action is needed as this pod is still running or starting up
        Some("Pending" | "Running") => true,
        // a finished, failed, or unknown pod can't serve MCP queries
        _ => false,
    }
}

/// Check whether a pod carries the mcp=enabled label
///
/// # Arguments
///
/// * `pod` - The pod to check
fn has_mcp_label(pod: &Pod) -> bool {
    pod.metadata
        .labels
        .as_ref()
        .is_some_and(|labels| labels.get("mcp").map(String::as_str) == Some("enabled"))
}

/// Check whether a designated pod still serves MCP queries
///
/// The label can be removed or moved by anyone, so a live pod only serves MCP queries while it
/// still carries it.
///
/// # Arguments
///
/// * `pod` - The designated pod to check
fn pod_serves_mcp(pod: &Pod) -> bool {
    pod_is_alive(pod) && has_mcp_label(pod)
}

/// Make sure exactly one live api pod is labelled to serve MCP queries
///
/// # Arguments
///
/// * `pod` - The api pod that changed
/// * `ctx` - The api pod watcher's context
async fn reconcile_api_pods(pod: Arc<Pod>, ctx: Arc<ApiWatchContext>) -> Result<Action, Error> {
    // if we don't have any configs then just requeue this pod in 30 seconds
    if ctx.shared.info.is_empty() {
        // don't scan this pod for another 30 seconds
        return Ok(Action::requeue(Duration::from_secs(30)));
    }
    // skip any pods with out names
    let Some(name) = &pod.metadata.name else {
        // ignore this pod for one minute
        return Ok(Action::requeue(Duration::from_secs(60)));
    };
    // get this pods current namespace
    let namespace = match pod.namespace() {
        Some(ns) => ns.to_string(),
        // all pods we scan must be in a namespace
        None => {
            println!("Pod {name} is not in a namespace?");
            // ignore this pod for one minute
            return Ok(Action::requeue(Duration::from_secs(60)));
        }
    };
    // find the clusters deployed in this pod's namespace of this pod's k8s cluster
    let infos = ctx
        .shared
        .info
        .pin()
        .values()
        .filter(|info| info.meta.context == ctx.context && info.meta.namespace == namespace)
        .cloned()
        .collect::<Vec<ThoriumInfo>>();
    // pods outside of a deployed cluster's namespace are not ours to label
    if infos.is_empty() {
        return Ok(Action::requeue(Duration::from_secs(30)));
    }
    // leave this namespace alone while its cluster is being deleted
    for info in &infos {
        if !info.is_live().await? {
            return Ok(Action::requeue(Duration::from_secs(60)));
        }
    }
    // hold this namespace's designation for the whole check, scan, label, and record so
    // concurrent reconciles of different api pods can't each label a different pod
    let slot = designation_slot(&ctx.mcp_pods, &namespace)?;
    let mut designation = slot.lock().await;
    // build a Thorium api pod client
    let pod_api: Api<Pod> = Api::<Pod>::namespaced(ctx.client.clone(), &namespace);
    // check whether the designated pod, if any, still serves MCP queries
    if let Some(current) = designation.as_deref() {
        // get the designated pod's current state rather than the possibly stale event copy,
        // which is gone once it has been deleted
        let designated_pod = pod_api.get_opt(current).await.map_err(|error| {
            Error::new(format!(
                "Error getting info on designated MCP pod {current}: {error}",
            ))
        })?;
        // a live designated pod that still carries the label needs no scan
        if designated_pod.as_ref().is_some_and(pod_serves_mcp) {
            // a different pod that carries the label is a duplicate, so unlabel it
            if current != name.as_str() && has_mcp_label(&pod) {
                remove_label(&pod_api, name).await;
            }
            // our designated mcp pod is still serving so don't scan it for 15 minutes
            return Ok(Action::requeue(Duration::from_mins(15)));
        }
    }
    // the designated pod is gone, dead, or lost its label, so make sure exactly one live pod
    // is labelled and remember it for every later reconcile (or forget this namespace's
    // designation when no pod could be designated so its next reconcile scans again)
    *designation = scan(&pod_api).await?;
    // no more action is needed for 60 seconds
    Ok(Action::requeue(Duration::from_secs(60)))
}

/// Get the designation slot of a namespace, adding an empty one if it has none yet
///
/// Each namespace has its own async lock so reconciles of one namespace never wait on another.
///
/// # Arguments
///
/// * `mcp_pods` - The designation slot of each namespace
/// * `namespace` - The namespace to get the designation slot for
fn designation_slot(
    mcp_pods: &Mutex<HashMap<String, DesignationSlot>>,
    namespace: &str,
) -> Result<DesignationSlot, Error> {
    // lock the slots only long enough to find or add this namespace's slot
    let mut mcp_pods = mcp_pods
        .lock()
        .map_err(|_| Error::new("The designated MCP pod lock is poisoned"))?;
    Ok(mcp_pods.entry(namespace.to_owned()).or_default().clone())
}

/// Label this api pod to be able to serve mcp queries
///
/// # Arguments
///
/// * `pod_api` - K8s pod api client
/// * `pod` - The name of the pod to add the mcp label to
async fn add_label(pod_api: &Api<Pod>, pod: &str) {
    println!("labeling {pod} with mcp=enabled");
    // build a label json template
    let label = serde_json::json!({
        "metadata": {
            "labels": {
                "mcp": "enabled",
            }
        }
    });
    // patch the pod with the new label
    match pod_api
        .patch(pod, &PatchParams::default(), &Patch::Merge(&label))
        .await
    {
        Ok(_) => println!("pod {pod} labeled successfully with mcp=enabled"),
        Err(error) => println!("Failed to label pod {pod}: {error}"),
    }
}

/// Remove the mcp=enabled label from an api pod
///
/// # Arguments
///
/// * `pod_api` - K8s pod api client
/// * `pod` - The name of the pod to remove the mcp label from
async fn remove_label(pod_api: &Api<Pod>, pod: &str) {
    let params = PatchParams::default();
    // build a label json template
    let label = serde_json::json!({
        "metadata": {
            "labels": {
                "mcp": null,
            }
        }
    });
    // patch the pod with the new label
    match pod_api.patch(pod, &params, &Patch::Merge(&label)).await {
        Ok(_) => {
            println!("Patched pod to remove label mcp=enabled from {pod}");
        }
        Err(kube::Error::Api(error)) => {
            // pod does not exist to remove label, continue on
            if error.code == 404 {
                println!("pod {pod} not found to remove label, skipping pod update");
            } else {
                println!("Failed to remove label {label} from pod {pod}: {error}");
            }
        }
        Err(error) => {
            println!("Failed to remove label {label} from pod {pod}: {error}");
        }
    }
}

/// Make sure exactly one live api pod is labelled for MCP and return its name
///
/// An existing labelled pod is kept, every other labelled live pod is unlabelled so duplicates
/// heal, and an unlabelled pod is labelled when none is. Returns None when there is no live api
/// pod to label.
///
/// # Arguments
///
/// * `pod_api` - The pod api for the namespace of the api pods
async fn scan(pod_api: &Api<Pod>) -> Result<Option<String>, Error> {
    // list all api pods
    let pods = pod_api
        .list(&ListParams::default().labels("app=api"))
        .await
        .map_err(|error| Error::new(format!("Failed to list api pods: {error}")))?;
    // track the currently labelled mcp pods
    let mut is_mcp = Vec::with_capacity(1);
    // track the pods that were not labeled as mcp pods
    let mut not_mcp = Vec::with_capacity(10);
    // look for any existing mcp pods
    for pod in pods {
        // check whether this pod is on a down node or labelled before its name is taken out of it
        let unreachable = pod_is_unreachable(&pod);
        let labelled = has_mcp_label(&pod);
        // get this pods name and status
        let Some(name) = pod.metadata.name else {
            // log that we are skipping an api pod without a name
            println!("Skipping API pod without a name!");
            // skip to the next pod
            continue;
        };
        // skip pods being deleted; they still report Running during a rollout, and a labelled
        // one is left as is since it is about to go away
        if pod.metadata.deletion_timestamp.is_some() {
            continue;
        }
        // a pod on a down node can't serve MCP queries, and one still labelled is unlabelled
        // so it doesn't take MCP traffic alongside the new designation once its node is back
        if unreachable {
            if labelled {
                remove_label(pod_api, &name).await;
            }
            continue;
        }
        // skip any pods that have a bad phase
        let running = match pod
            .status
            .as_ref()
            .and_then(|status| status.phase.as_deref())
        {
            Some("Running") => true,
            Some("Pending") => false,
            // this pod is probably one of our old pods that we are replacing
            // if we log that we are skipping it then it constantly looks like
            // the operator is failing when this is expected and normal behavior
            Some("Failed") => continue,
            Some(phase) => {
                // log that we are skipping an api pod with the wrong phase
                println!("Skipping API pod with phase: {phase}");
                // skip to the next pod
                continue;
            }
            None => {
                // log that we are skipping an api pod without a phase
                println!("Skipping API pod without a phase!");
                // skip to the next pod
                continue;
            }
        };
        // sort this pod by whether it is already labelled as an mcp pod, keeping running pods
        // at the end of the unlabelled ones so a pod that can already serve queries is picked
        if labelled {
            is_mcp.push(name);
        } else if running {
            not_mcp.push(name);
        } else {
            not_mcp.insert(0, name);
        }
    }
    // keep exactly one mcp pod
    let designated = match is_mcp.len() {
        // we don't have any mcp pods so just pick one
        0 => {
            // get the last found not mcp pod, which is running if any is
            match not_mcp.pop() {
                Some(name) => {
                    // label this pod as being mcp enabled
                    add_label(pod_api, &name).await;
                    // return the newly designated mcp pod
                    Some(name)
                }
                None => {
                    // log that no pod could be marked as our mcp pod
                    println!("No viable API found to label for MCP support");
                    // we couldn't label a pod as being mcp enabled
                    None
                }
            }
        }
        // we found a single mcp pod
        1 => is_mcp.pop(),
        // we found multiple mcp pods
        _ => {
            // pop the one pod to keep
            let keep = is_mcp.pop();
            // remove the mcp enabled label for the rest of the pods
            for pod in is_mcp {
                remove_label(pod_api, &pod).await;
            }
            // return the name of the still designated mcp pod
            keep
        }
    };
    Ok(designated)
}

/// Watch our api pods for any changes
///
/// # Arguments
///
/// * `client` - The kube client to watch with
/// * `context` - The name of the kube context `client` reaches its k8s cluster through
/// * `namespace` - The namespace to watch api pods in (all namespaces if unset)
/// * `shared` - Data shared across watchers
pub async fn start(
    client: Client,
    context: String,
    namespace: Option<String>,
    shared: Arc<SharedInfo>,
) {
    // build a Thorium api pod client for the namespaces we watch
    let pod_api: Api<Pod> = super::scoped_api(&client, namespace.as_deref());
    // setup some state for our watcher; only the api pods of a cluster this operator has
    // applied are labelled, so the pods of a cluster held since the operator started (such as
    // one waiting for an upgrade) are never labelled
    let ctx = ApiWatchContext {
        client: client.clone(),
        context,
        mcp_pods: Mutex::new(HashMap::new()),
        shared: shared.clone(),
    };
    // set our config to only list api pods
    let config = Config::default().labels("app=api").any_semantic();
    // create a controller to watch for changes in our api pods, draining its results since
    // reconcile and the error policy already log every outcome
    Controller::new(pod_api, config)
        .shutdown_on_signal()
        .run(reconcile_api_pods, api_pod_error_policy, Arc::new(ctx))
        .for_each(|_| futures::future::ready(()))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a pod in a phase
    ///
    /// # Arguments
    ///
    /// * `phase` - The pod's phase, if it has one
    fn pod_in(phase: Option<&str>) -> Pod {
        serde_json::from_value(serde_json::json!({
            "metadata": {"name": "api-1"},
            "status": {"phase": phase}
        }))
        .expect("pod should deserialize")
    }

    /// Only pending and running pods can serve MCP queries
    #[test]
    fn pod_liveness_by_phase() {
        // starting and running pods are alive
        assert!(pod_is_alive(&pod_in(Some("Pending"))));
        assert!(pod_is_alive(&pod_in(Some("Running"))));
        // finished, failed, unknown, and unreported pods are not
        for phase in [Some("Succeeded"), Some("Failed"), Some("Unknown"), None] {
            assert!(!pod_is_alive(&pod_in(phase)), "{phase:?}");
        }
    }

    /// A terminating pod still reporting Running is not alive so a new MCP pod is designated
    #[test]
    fn terminating_pod_is_not_alive() {
        // a running pod being deleted during a rollout
        let mut pod = pod_in(Some("Running"));
        pod.metadata.deletion_timestamp = Some(
            k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(chrono::Utc::now()),
        );
        // it is terminating and so cannot keep serving MCP queries
        assert!(pod_is_terminating(&pod));
        assert!(!pod_is_alive(&pod));
        // a running pod not being deleted is alive
        assert!(!pod_is_terminating(&pod_in(Some("Running"))));
    }

    /// A running pod marked unready while its containers still report ready (its node is
    /// down or unreachable) is not alive, while starting and healthy pods are
    #[test]
    fn unreachable_pod_is_not_alive() {
        // build a running pod with a Ready condition and one container's readiness
        let pod = |pod_ready: &str, container_ready: bool| -> Pod {
            serde_json::from_value(serde_json::json!({
                "metadata": {"name": "api-1"},
                "status": {
                    "phase": "Running",
                    "conditions": [{"type": "Ready", "status": pod_ready}],
                    "containerStatuses": [{
                        "name": "api",
                        "ready": container_ready,
                        "restartCount": 0,
                        "image": "thorium",
                        "imageID": ""
                    }]
                }
            }))
            .expect("pod should deserialize")
        };
        // a pod on a node that stopped reporting is unreachable and not alive
        assert!(pod_is_unreachable(&pod("False", true)));
        assert!(!pod_is_alive(&pod("False", true)));
        assert!(pod_is_unreachable(&pod("Unknown", true)));
        // a healthy pod is alive
        assert!(!pod_is_unreachable(&pod("True", true)));
        assert!(pod_is_alive(&pod("True", true)));
        // a pod failing its own readiness probe is still alive and keeps its designation
        assert!(!pod_is_unreachable(&pod("False", false)));
        assert!(pod_is_alive(&pod("False", false)));
        // a pod without any status yet is not unreachable
        assert!(!pod_is_unreachable(&pod_in(Some("Pending"))));
        assert!(!pod_is_unreachable(&pod_in(None)));
    }

    /// A labelled api pod on a down node is unlabelled and a reachable pod designated instead
    #[tokio::test]
    async fn scan_moves_mcp_off_down_node() {
        // build a running api pod with its readiness and mcp label
        let pod = |name: &str, pod_ready: &str, mcp: bool| {
            let labels = if mcp {
                serde_json::json!({"app": "api", "mcp": "enabled"})
            } else {
                serde_json::json!({"app": "api"})
            };
            serde_json::json!({
                "metadata": {"name": name, "namespace": "thorium", "labels": labels},
                "status": {
                    "phase": "Running",
                    "conditions": [{"type": "Ready", "status": pod_ready}],
                    "containerStatuses": [{
                        "name": "api",
                        "ready": true,
                        "restartCount": 0,
                        "image": "thorium",
                        "imageID": ""
                    }]
                }
            })
        };
        // the designated pod is on a down node and another pod is healthy
        let list = serde_json::json!({
            "apiVersion": "v1",
            "kind": "PodList",
            "metadata": {},
            "items": [pod("api-down", "False", true), pod("api-up", "True", false)]
        });
        let fake = crate::k8s::clusters::tests::FakeKube::default().route(
            "GET",
            "/api/v1/namespaces/thorium/pods",
            200,
            list,
        );
        let pod_api: Api<Pod> = Api::namespaced(fake.client(), "thorium");
        // the healthy pod is designated
        assert_eq!(
            scan(&pod_api).await.expect("scan").as_deref(),
            Some("api-up")
        );
        // the down pod's label was removed and the healthy pod was labelled
        let patches = fake
            .writes()
            .into_iter()
            .map(|request| {
                (
                    request.path,
                    request.body["metadata"]["labels"]["mcp"].clone(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            patches,
            [
                (
                    "/api/v1/namespaces/thorium/pods/api-down".to_owned(),
                    serde_json::Value::Null
                ),
                (
                    "/api/v1/namespaces/thorium/pods/api-up".to_owned(),
                    serde_json::json!("enabled")
                ),
            ]
        );
    }

    /// A running api pod is designated over a pending one wherever it is listed
    #[tokio::test]
    async fn scan_prefers_running_pod() {
        // a pending pod listed after a running one
        let mut pending = api_pod("api-new", false);
        pending["status"]["phase"] = serde_json::json!("Pending");
        let fake = fake_api_pods(&[api_pod("api-old", false), pending], &[]);
        let pod_api: Api<Pod> = Api::namespaced(fake.client(), "thorium");
        // the running pod is labelled
        assert_eq!(
            scan(&pod_api).await.expect("scan").as_deref(),
            Some("api-old")
        );
    }

    /// Each namespace has its own designation slot and a namespace always gets the same one
    #[test]
    fn designation_slots_are_per_namespace() {
        let slots = Mutex::new(HashMap::new());
        // a namespace gets the same slot every time and starts with no designation
        let a = designation_slot(&slots, "a").unwrap();
        assert!(Arc::ptr_eq(&a, &designation_slot(&slots, "a").unwrap()));
        assert!(a.try_lock().unwrap().is_none());
        // designating a pod in one namespace leaves the other alone
        *a.try_lock().unwrap() = Some("api-a".to_owned());
        let b = designation_slot(&slots, "b").unwrap();
        assert!(!Arc::ptr_eq(&a, &b));
        assert!(b.try_lock().unwrap().is_none());
        // a held slot doesn't block another namespace's slot
        let _held = a.try_lock().unwrap();
        assert!(b.try_lock().is_ok());
        assert_eq!(slots.lock().unwrap().len(), 2);
    }

    /// Build a running, reachable api pod in the `thorium` namespace as JSON
    ///
    /// # Arguments
    ///
    /// * `name` - The pod's name
    /// * `mcp` - Whether the pod carries the mcp=enabled label
    fn api_pod(name: &str, mcp: bool) -> serde_json::Value {
        // label every api pod and the mcp ones as such
        let labels = if mcp {
            serde_json::json!({"app": "api", "mcp": "enabled"})
        } else {
            serde_json::json!({"app": "api"})
        };
        serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": name, "namespace": "thorium", "labels": labels},
            "status": {"phase": "Running"}
        })
    }

    /// Build a pod list answer from pod JSON
    ///
    /// # Arguments
    ///
    /// * `pods` - The pods to list
    fn pod_list(pods: &[serde_json::Value]) -> serde_json::Value {
        serde_json::json!({"apiVersion": "v1", "kind": "PodList", "metadata": {}, "items": pods})
    }

    /// Build a fake kube API where the `thorium` cluster is live and the api pods are listed
    ///
    /// Every pod patch succeeds, and getting a single pod answers it as given.
    ///
    /// # Arguments
    ///
    /// * `listed` - The api pods a list answers with
    /// * `gets` - The api pods a get by name answers with
    fn fake_api_pods(
        listed: &[serde_json::Value],
        gets: &[serde_json::Value],
    ) -> crate::k8s::clusters::tests::FakeKube {
        use crate::k8s::clusters::tests::{FakeKube, full_spec, namespaced_cluster};
        // the cluster exists and isn't being deleted
        let cluster = serde_json::to_value(namespaced_cluster(full_spec())).unwrap();
        let mut fake = FakeKube::default()
            .route(
                "GET",
                "/apis/sandia.gov/v1/namespaces/thorium/thoriumclusters/thorium",
                200,
                cluster,
            )
            .route(
                "GET",
                "/api/v1/namespaces/thorium/pods",
                200,
                pod_list(listed),
            );
        // answer gets and patches of every pod by name
        for pod in listed.iter().chain(gets) {
            let path = format!(
                "/api/v1/namespaces/thorium/pods/{}",
                pod["metadata"]["name"].as_str().unwrap()
            );
            fake = fake.route("PATCH", &path, 200, pod.clone());
        }
        for pod in gets {
            let path = format!(
                "/api/v1/namespaces/thorium/pods/{}",
                pod["metadata"]["name"].as_str().unwrap()
            );
            fake = fake.route("GET", &path, 200, pod.clone());
        }
        fake
    }

    /// Build an api pod watcher context with the `thorium` cluster deployed
    ///
    /// # Arguments
    ///
    /// * `fake` - The fake kube API the context's clients use
    async fn watch_context(fake: &crate::k8s::clusters::tests::FakeKube) -> Arc<ApiWatchContext> {
        use crate::k8s::clusters::tests::{TEST_CONTEXT, full_spec, meta_for, namespaced_cluster};
        // build the deployed cluster's info like an apply does
        let client = fake.client();
        let meta = meta_for(namespaced_cluster(full_spec()), &client);
        let shared = crate::k8s::controller::tests::shared_with(meta).await;
        Arc::new(ApiWatchContext {
            client,
            context: TEST_CONTEXT.to_owned(),
            mcp_pods: Mutex::new(HashMap::new()),
            shared: Arc::new(shared),
        })
    }

    /// Get the mcp label patches the fake kube API received as (pod path, label value)
    ///
    /// # Arguments
    ///
    /// * `fake` - The fake kube API to get the patches from
    fn label_patches(
        fake: &crate::k8s::clusters::tests::FakeKube,
    ) -> Vec<(String, serde_json::Value)> {
        fake.writes()
            .into_iter()
            .map(|request| {
                (
                    request.path,
                    request.body["metadata"]["labels"]["mcp"].clone(),
                )
            })
            .collect()
    }

    /// Concurrent reconciles of two new api pods label exactly one of them
    #[tokio::test]
    async fn concurrent_reconciles_label_one_pod() {
        // both new pods are unlabelled when listed, and whichever is labelled reads back so
        let fake = fake_api_pods(
            &[api_pod("api-a", false), api_pod("api-b", false)],
            &[api_pod("api-a", true), api_pod("api-b", true)],
        );
        let ctx = watch_context(&fake).await;
        // reconcile both new pods at once like the controller does after both were replaced
        let pod = |name: &str| -> Arc<Pod> {
            Arc::new(serde_json::from_value(api_pod(name, false)).unwrap())
        };
        let (a, b) = tokio::join!(
            reconcile_api_pods(pod("api-a"), ctx.clone()),
            reconcile_api_pods(pod("api-b"), ctx.clone())
        );
        a.expect("reconcile api-a");
        b.expect("reconcile api-b");
        // only one pod was labelled and nothing else was patched
        let patches = label_patches(&fake);
        assert_eq!(patches.len(), 1, "{patches:?}");
        assert_eq!(patches[0].1, serde_json::json!("enabled"));
        // the labelled pod is the designated one
        let designated = designation_slot(&ctx.mcp_pods, "thorium").unwrap();
        let designated = designated.lock().await.clone().expect("a designated pod");
        assert_eq!(
            patches[0].0,
            format!("/api/v1/namespaces/thorium/pods/{designated}")
        );
    }

    /// Scan keeps one labelled pod and unlabels every other one
    #[tokio::test]
    async fn scan_removes_duplicate_labels() {
        // three live pods all carry the label
        let fake = fake_api_pods(
            &[
                api_pod("api-a", true),
                api_pod("api-b", true),
                api_pod("api-c", true),
            ],
            &[],
        );
        let pod_api: Api<Pod> = Api::namespaced(fake.client(), "thorium");
        // the last one is kept and the rest are unlabelled
        assert_eq!(
            scan(&pod_api).await.expect("scan").as_deref(),
            Some("api-c")
        );
        assert_eq!(
            label_patches(&fake),
            [
                (
                    "/api/v1/namespaces/thorium/pods/api-a".to_owned(),
                    serde_json::Value::Null
                ),
                (
                    "/api/v1/namespaces/thorium/pods/api-b".to_owned(),
                    serde_json::Value::Null
                ),
            ]
        );
    }

    /// A reconcile of a labelled pod that isn't the designated one unlabels it
    #[tokio::test]
    async fn reconcile_unlabels_duplicate() {
        // the designated pod still carries the label and serves MCP queries
        let fake = fake_api_pods(&[], &[api_pod("api-b", true)]);
        let ctx = watch_context(&fake).await;
        *designation_slot(&ctx.mcp_pods, "thorium")
            .unwrap()
            .lock()
            .await = Some("api-b".to_owned());
        // another pod that also carries the label changes
        let pod: Pod = serde_json::from_value(api_pod("api-a", true)).unwrap();
        reconcile_api_pods(Arc::new(pod), ctx.clone())
            .await
            .expect("reconcile");
        // its label was removed and the designation kept
        assert_eq!(
            label_patches(&fake),
            [(
                "/api/v1/namespaces/thorium/pods/api-a".to_owned(),
                serde_json::Value::Null
            )]
        );
        let slot = designation_slot(&ctx.mcp_pods, "thorium").unwrap();
        assert_eq!(slot.lock().await.as_deref(), Some("api-b"));
    }

    /// A live designated pod that lost its label is not trusted, so a pod is labelled again
    #[tokio::test]
    async fn designated_pod_without_label_rescans() {
        // the designated pod is still running but its label was removed by hand
        let fake = fake_api_pods(
            &[api_pod("api-a", false), api_pod("api-b", false)],
            &[api_pod("api-a", false)],
        );
        let ctx = watch_context(&fake).await;
        *designation_slot(&ctx.mcp_pods, "thorium")
            .unwrap()
            .lock()
            .await = Some("api-a".to_owned());
        // the label removal triggers a reconcile of the designated pod
        let pod: Pod = serde_json::from_value(api_pod("api-a", false)).unwrap();
        reconcile_api_pods(Arc::new(pod), ctx.clone())
            .await
            .expect("reconcile");
        // a scan labelled a pod and it became the designated one
        assert_eq!(
            label_patches(&fake),
            [(
                "/api/v1/namespaces/thorium/pods/api-b".to_owned(),
                serde_json::json!("enabled")
            )]
        );
        let slot = designation_slot(&ctx.mcp_pods, "thorium").unwrap();
        assert_eq!(slot.lock().await.as_deref(), Some("api-b"));
    }

    /// A designated pod that still carries its label and is alive is left alone
    #[tokio::test]
    async fn labelled_designated_pod_is_kept() {
        // the designated pod is running and labelled
        let fake = fake_api_pods(&[], &[api_pod("api-a", true)]);
        let ctx = watch_context(&fake).await;
        *designation_slot(&ctx.mcp_pods, "thorium")
            .unwrap()
            .lock()
            .await = Some("api-a".to_owned());
        // an unlabelled pod changes
        let pod: Pod = serde_json::from_value(api_pod("api-b", false)).unwrap();
        let action = reconcile_api_pods(Arc::new(pod), ctx)
            .await
            .expect("reconcile");
        // nothing was listed or patched and the pod is checked again later
        assert!(fake.writes().is_empty());
        assert!(
            !fake
                .requests()
                .iter()
                .any(|request| request.path == "/api/v1/namespaces/thorium/pods")
        );
        assert_eq!(action, Action::requeue(Duration::from_mins(15)));
    }

    /// The api pods of a cluster dropped from the shared info (such as one held by the upgrade
    /// gate) are left alone even while another namespace's cluster is still deployed
    #[tokio::test]
    async fn forgotten_cluster_pods_left_alone() {
        use crate::k8s::clusters::tests::TEST_CONTEXT;
        // the thorium cluster is deployed alongside one in another namespace
        let fake = fake_api_pods(&[api_pod("api-a", false)], &[]);
        let ctx = watch_context(&fake).await;
        let mut other = (*ctx.shared.info.pin().values().next().expect("info").meta).clone();
        other.namespace = "other".to_owned();
        ctx.shared.info.pin().insert(
            SharedInfo::key(TEST_CONTEXT, "other", "thorium"),
            ThoriumInfo {
                thorium: ctx
                    .shared
                    .info
                    .pin()
                    .values()
                    .next()
                    .expect("info")
                    .thorium
                    .clone(),
                meta: Arc::new(other),
            },
        );
        // the thorium cluster is dropped
        ctx.shared
            .info
            .pin()
            .remove(&SharedInfo::key(TEST_CONTEXT, "thorium", "thorium"));
        // its api pods are neither read nor labelled
        let pod: Pod = serde_json::from_value(api_pod("api-a", false)).unwrap();
        let action = reconcile_api_pods(Arc::new(pod), ctx)
            .await
            .expect("reconcile");
        assert_eq!(action, Action::requeue(Duration::from_secs(30)));
        assert_eq!(fake.requests().len(), 0, "{:?}", fake.requests());
    }
}
