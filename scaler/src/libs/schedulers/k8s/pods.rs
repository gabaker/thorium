use futures::stream::{self, StreamExt};
use hashbrown::HashMap;
use k8s_openapi::api::core::v1::{Pod, PodSecurityContext, PodSpec};
use kube::api::{Api, DeleteParams, ListParams, ObjectList, PostParams};
use reqwest::StatusCode;
use serde_json::json;
use std::collections::{BTreeMap, HashSet};
use thorium::conf::K8sHostAliases;
use thorium::models::{Image, TOOL_INSTANCE_LABEL, k8s_label};
use thorium::{Conf, Error};
use tracing::{Level, event, instrument};

use super::{Containers, Secrets, Volumes};
use crate::libs::Cache;
use crate::libs::scaler::ErrorOutKinds;
use crate::libs::schedulers::{Spawned, WorkerDeletion};
use crate::{raw_entry_map_extend, raw_entry_map_insert};

/// Pods api wrapper for kubernetes
pub struct Pods {
    /// Client to use for creating namespaced clients
    client: kube::Client,
    /// Secrets wrapper
    secrets: Secrets,
    /// volume wrappers
    volumes: Volumes,
    /// The wrapper for container operations with k8s
    containers: Containers,
    /// Pod API client for all namespaces
    api: Api<Pod>,
    /// The host aliases to apply to pods in this cluster
    host_aliases: Vec<K8sHostAliases>,
    /// The deletes that are still pending
    pub pending_deletes: HashMap<String, HashMap<String, Spawned>>,
    /// An intermediary vec to store pending deletes when checking them
    temp_deletes: Vec<(String, Spawned)>,
}

impl Pods {
    /// Creates new pods wrapper
    ///
    /// # Arguments
    ///
    /// * `client` - Kubernetes client
    /// * `conf` - Thorium Config
    /// * `cluster_name` - The name of this cluster
    /// * `context_name` - The name of this context
    pub fn new<T: Into<String>>(
        client: &kube::Client,
        conf: &Conf,
        cluster_name: T,
        context_name: &str,
    ) -> Self {
        // build pods api client
        let api: Api<Pod> = Api::all(client.clone());
        // build secrets wrapper
        let secrets = Secrets::new(client, conf, context_name);
        // build volumes wrapper
        let volumes = Volumes::new(client, conf, context_name);
        // build the containers wrapper
        let containers = Containers::new(cluster_name, context_name);
        // get our host aliases
        let unconverted_aliases = conf.thorium.scaler.k8s.host_aliases(context_name);
        // convert our host aliases into a K8s host aliases object
        let host_aliases = unconverted_aliases
            .map(|map| map.iter().map(K8sHostAliases::from).collect())
            .unwrap_or_default();
        // get client for creating namespaced clients with
        let client = client.clone();
        Pods {
            client,
            secrets,
            volumes,
            containers,
            api,
            host_aliases,
            pending_deletes: HashMap::default(),
            temp_deletes: Vec::default(),
        }
    }

    /// List all pods in a namespace
    ///
    /// # Arguments
    ///
    /// * `ns` - Namespace to list pods from
    /// * `node` - Optional node to list pods from
    #[instrument(name = "k8s::Pods::list", skip(self))]
    pub async fn list(&self, ns: &str, node: Option<&str>) -> Result<ObjectList<Pod>, Error> {
        // build list params
        let params = match node {
            Some(node) => ListParams::default()
                .fields(&format!("metadata.namespace=={ns},spec.nodeName=={node}")),
            None => ListParams::default().fields(&format!("metadata.namespace=={ns}")),
        };
        // list all pods in namespace
        let pods = self.api.list(&params).await?;
        Ok(pods)
    }

    /// List all pods across all namespaces
    ///
    /// # Arguments
    ///
    /// * `node` - Optional node to list pods from
    #[instrument(name = "k8s::Pods::list_all", skip(self))]
    pub async fn list_all(&self, node: Option<String>) -> Result<ObjectList<Pod>, Error> {
        // build list params
        let params = match node {
            Some(node) => ListParams::default().fields(&format!("spec.nodeName=={node}")),
            None => ListParams::default(),
        };
        // list all pods
        let pods = self.api.list(&params).await?;
        Ok(pods)
    }

    /// Checks if a pod is owned by Thorium through labels
    ///
    /// # Arguments
    ///
    /// * `pod` - The pod to check for ownership by Thorium
    pub fn thorium_owned(pod: &Pod) -> bool {
        // if labels don't exist then this is not owned by Thorium
        let labels = match &pod.metadata.labels {
            Some(labels) => labels,
            None => return false,
        };

        // all of these labels must exist for this to be a valid Thorium pod
        let required = ["user", "group", TOOL_INSTANCE_LABEL, "thorium"];
        // check each label exists and return false if one doesn't
        for require in required.iter() {
            if !labels.contains_key(require.to_owned()) {
                return false;
            }
        }
        true
    }

    /// Sort any terminal pods into either Failed or Succeeded buckets
    ///
    /// # Arguments
    ///
    /// * `pod` - The pod to check if its running
    /// * `failed` - The pods that have failed due to an error
    /// * `succeeded` - The pods that are in a succeeded state
    /// * `error_out` - The pods whose workers we should fail out instead of just resetting
    ///
    /// This is an associated function (it never needs `self`) so its classification logic
    /// can be unit tested without constructing a `Pods` and its live kube client.
    fn filter_terminal(
        pod: &Pod,
        failed: &mut Vec<String>,
        succeeded: &mut Vec<String>,
        error_out: &mut Vec<ErrorOutKinds>,
    ) -> Result<(), Error> {
        // get this pods name
        let pod_name = match &pod.metadata.name {
            Some(name) => name,
            None => {
                return Err(Error::new("Pod has no name".to_owned()));
            }
        };
        // error on pods without a status block
        if pod.status.is_none() {
            let err = Error::new(format!("Pod {pod_name} has no status"));
            return Err(err);
        }
        // get our pods status
        if let Some(status) = &pod.status {
            // check this pods status
            match &status.phase.as_deref() {
                Some("Succeeded") => succeeded.push(pod_name.to_owned()),
                // pods whose image cannot be pulled stay pending forever
                Some("Pending") => {
                    // check if any of our containers are waiting on an image that can't be pulled
                    let pull_failed = status
                        .container_statuses
                        .iter()
                        .flatten()
                        .filter_map(|container_status| container_status.state.as_ref())
                        .filter_map(|state| state.waiting.as_ref())
                        .filter_map(|waiting| waiting.reason.as_deref())
                        // ErrImagePull is skipped since it fires on the first and possibly
                        // transient pull failure before the kubelet starts backing off
                        .any(|reason| matches!(reason, "ImagePullBackOff" | "InvalidImageName"));
                    // fail out this pods job if its image cannot be pulled
                    if pull_failed {
                        // build our image pull backoff reason
                        let reason = ErrorOutKinds::image_pull_backoff(pod_name);
                        // log that we found a worker whose image can't be pulled
                        event!(
                            Level::INFO,
                            worker = pod_name,
                            reason = reason.reason_as_str(),
                        );
                        // error out pods are deleted and failed out by the scaler
                        error_out.push(reason);
                    }
                }
                Some("Failed") => {
                    // check if this pod failed due to OOM in container state
                    if let Some(container_statuses) = pod
                        .status
                        .as_ref()
                        .and_then(|status| status.container_statuses.as_ref())
                    {
                        // check if any of our container statuses are an oom error
                        for container_status in container_statuses {
                            // check if we have a container state
                            if let Some(state) = &container_status.state {
                                // check if we have a terminated state
                                if let Some(terminated) = &state.terminated {
                                    // check why this image failed
                                    let reason = match terminated.reason.as_deref() {
                                        Some("OOMKilled") => ErrorOutKinds::oom(pod_name),
                                        Some("ImagePullBackOff") => {
                                            ErrorOutKinds::image_pull_backoff(pod_name)
                                        }
                                        _ => continue,
                                    };
                                    // log that we found a dead worker
                                    event!(
                                        Level::INFO,
                                        worker = pod_name,
                                        reason = reason.reason_as_str(),
                                    );
                                    // set this pods job to be failed out instead of reset
                                    error_out.push(reason);
                                }
                            }
                        }
                    }
                    failed.push(pod_name.to_owned());
                }
                _ => (),
            }
        }
        Ok(())
    }

    /// Deletes a pod by name within a namespace
    ///
    /// # Arguments
    ///
    /// * `ns` - The namespace of the pod to delete
    /// * `name` - The name of the pod to delete
    #[instrument(name = "k8s::Pods::delete", skip(self))]
    pub async fn delete(&self, ns: &str, name: &str) -> Result<(), Error> {
        // pod api instance
        let pod_api: Api<Pod> = Api::namespaced(self.client.clone(), ns);
        // build delete params
        let params = DeleteParams::default().grace_period(0);
        // delete target pod
        if let Err(error) = pod_api.delete(name, &params).await {
            // if we got an erorr while deleting this pod then check if its a 404
            // if it was ignore it since the pod is already gone
            // match on the error enum
            match &error {
                // this is an API error check the return code
                kube::Error::Api(api_err) => {
                    if api_err.code != 404 {
                        return Err(Error::from(error));
                    }
                }
                // not an API error
                _ => return Err(Error::from(error)),
            }
        }
        Ok(())
    }

    /// Deletes pods 10 at a time in an unordered buffer
    ///
    /// # Arguments
    ///
    /// * `ns` - The namespace to delete pods from
    /// * `spawns` - The spawned objects to delete
    /// * `results` - The results of these and any other pending deletes
    #[instrument(name = "k8s::Pods::delete_many", skip(self, spawns, results))]
    pub async fn delete_many<'a>(
        &mut self,
        ns: &str,
        spawns: &Vec<Spawned>,
        results: &mut Vec<WorkerDeletion>,
    ) {
        // pod api instance
        let api: Api<Pod> = Api::namespaced(self.client.clone(), ns);
        // build delete params
        let params = DeleteParams::default().grace_period(0);
        // build a list to store our cloned names
        let mut cloned_names = Vec::with_capacity(spawns.len());
        // clone our names to work around lifetime issues
        for spawn in spawns {
            // log the pod we are deleting
            event!(Level::INFO, pod = spawn.name);
            // add this pods name
            cloned_names.push(spawn.name.clone());
        }
        // delete pods 10 at a time
        let deletes = stream::iter(cloned_names)
            .map(|name| {
                // get references to minimize cloning
                let api_ref = &api;
                let params_ref = &params;
                async move { api_ref.delete(&name, params_ref).await }
            })
            .buffered(5)
            .collect::<Vec<Result<_, _>>>()
            .await;
        // crawl our deletes and determine if any failures occured
        for (spawn, delete) in spawns.iter().zip(deletes.into_iter()) {
            // check if our delete failed
            if let Err(error) = delete {
                // cast this k8s error to a Thorium error
                let error = Error::from(error);
                // if this is an error becauset the pod no longer exists then ignore it
                if error.status() != Some(StatusCode::NOT_FOUND) {
                    // this error will get logged by the scaler later
                    // build our error object
                    let err = WorkerDeletion::Error {
                        delete: spawn.clone(),
                        error,
                    };
                    // this attempt failed so track this error
                    results.push(err);
                }
            }
        }
    }

    /// Validates that pods were deleted or not
    ///
    /// # Arguments
    ///
    /// * `ns` - The namespace to delete pods from
    /// * `names` - The names of the pods to delete
    /// * `results` - The results of these and any other pending deletes
    #[instrument(name = "k8s::Pods::validate_deletes", skip(self, spawns, results))]
    pub async fn validate_deletes(
        &mut self,
        ns: &str,
        spawns: Vec<Spawned>,
        results: &mut Vec<WorkerDeletion>,
    ) {
        // get a list of all pods in our target namespace
        match self.list(ns, None).await {
            Ok(pods) => {
                // condense our list to just the pod names
                let condensed = pods
                    .iter()
                    .filter_map(|pod| pod.metadata.name.as_ref())
                    .collect::<HashSet<&String>>();
                // check if any of our prior pending deletions have completed yet
                if let Some(ns_map) = self.pending_deletes.remove(ns) {
                    // classify our prior pending deletes into completed deletes and still pending
                    let ns_map = Self::classify_pending_deletes(
                        &condensed,
                        ns_map,
                        &mut self.temp_deletes,
                        results,
                    );
                    // if we still have pending pods then readd this namespaces list
                    if !ns_map.is_empty() {
                        self.pending_deletes.insert(ns.to_owned(), ns_map);
                    }
                }
                // add any pods still in our pods list to our results list as pending
                for spawn in spawns {
                    // if this pod is still in our list then mark it as pending
                    if condensed.contains(&spawn.name) {
                        // track that our pending deleted worker hasn't been deleted yet
                        raw_entry_map_insert!(self.pending_deletes, ns, spawn.name.clone(), spawn);
                    } else {
                        // this pod no longer exists so mark it as deleted
                        results.push(WorkerDeletion::Deleted(spawn));
                    }
                }
            }
            Err(err) => {
                // TODO: log this error with TRACING
                println!("ERROR: {err:#?}");
                // we failed to list our pods so mark all of their deletes as pending
                raw_entry_map_extend!(
                    self.pending_deletes,
                    ns,
                    spawns.into_iter().map(|spawn| (spawn.name.clone(), spawn))
                );
            }
        }
    }

    /// Classify a namespaces prior pending deletes into completed deletes and still pending ones
    ///
    /// This is the first (prior-pending) classification loop in [`Pods::validate_deletes`] (it
    /// never needs `self`) extracted so its logic can be unit tested without a live kube client.
    /// A pod that is no longer present in `condensed` is reported Deleted and a pod that is still
    /// present stays pending.
    ///
    /// # Arguments
    ///
    /// * `condensed` - The set of pod names STILL existing in the namespace
    /// * `ns_map` - The prior pending deletes for this namespace, consumed by this call
    /// * `temp` - Scratch space for the still pending deletes before they are re-collected
    /// * `results` - The list of worker deletions, appended to in place
    fn classify_pending_deletes(
        condensed: &HashSet<&String>,
        mut ns_map: HashMap<String, Spawned>,
        temp: &mut Vec<(String, Spawned)>,
        results: &mut Vec<WorkerDeletion>,
    ) -> HashMap<String, Spawned> {
        // its probably best to assume it takes less than a scale loop to delete pods
        // so drain and store our still pending deletes in a temp vec
        // prune any of our no longer pending deletes
        for (name, delete) in ns_map.drain() {
            // check if this pod has been deleted
            if !condensed.contains(&name) {
                // this pod no longer exists so it has been deleted
                results.push(WorkerDeletion::Deleted(delete));
            } else {
                temp.push((name, delete));
            }
        }
        // readd any still pending deletes to our pending deletes map
        for (name, delete) in temp.drain(..) {
            ns_map.insert(name, delete);
        }
        ns_map
    }

    /// Deletes pods 10 at a time in an unordered buffer
    ///
    /// # Arguments
    ///
    /// * `ns` - The namespace to delete pods from
    /// * `names` - The names of the pods to delete
    #[instrument(name = "k8s::Pods::delete_many_owned", skip(self), err(Debug))]
    pub async fn delete_many_owned(&self, ns: &str, names: Vec<String>) -> Result<(), Error> {
        // delete our pods 10 at a time
        stream::iter(names)
            .map(|name| async move { self.delete(ns, &name).await })
            .buffer_unordered(10)
            .collect::<Vec<Result<(), _>>>()
            .await
            // check for any errors and propagate the first one upwards
            .into_iter()
            .collect::<Result<Vec<()>, _>>()?;
        Ok(())
    }

    /// Deletes pods 10 at a time in an unordered buffer in a best effort manner
    ///
    /// # Arguments
    ///
    /// * `ns` - The namespace to delete pods from
    /// * `names` - The names of the pods to delete
    #[instrument(name = "k8s::Pods::delete_many_owned", skip(self))]
    pub async fn try_delete_many(&self, ns: &str, names: Vec<&String>) {
        // assert this stream is a send stream to prevent FnOnce errors
        //let asserted = thorium::utils::helpers::assert_send_stream(names);
        // delete our pods 10 at a time
        let stream_iter = stream::iter(&names)
            .map(|name| async move { self.delete(ns, &name).await })
            .buffered(10);
        // assert this stream is send to avoid FnOnce errors
        let attempts = thorium::utils::helpers::assert_send_stream(stream_iter)
            .collect::<Vec<Result<(), Error>>>()
            .await;
        // log any errors
        for (pod_name, attempt) in names.iter().zip(attempts) {
            // check if this delete failed
            if let Err(error) = attempt {
                // log our failed delete
                event!(
                    Level::ERROR,
                    msg = "Failed to delete worker",
                    worker = pod_name,
                    error = error.to_string()
                );
            }
        }
    }

    /// Builds a security context for this pod
    ///
    /// * `cache` - A cache of info from Thorium
    /// * `user` - The user this pod will be executing jobs as
    /// * `image` - The image this pod is using
    fn build_security_ctx(cache: &Cache, user: &String, image: &Image) -> PodSecurityContext {
        // if this user has any unix info then infject that
        if let Some(unix) = &cache.users[user].unix {
            // if we have a user set in the security context override then use that
            let user = image.security_context.user.unwrap_or(unix.user as i64);
            // if we have a group set in the security context override then use that
            let group = image.security_context.group.unwrap_or(unix.group as i64);
            // if we are allowed
            // build our pod security context
            PodSecurityContext {
                run_as_user: Some(user),
                run_as_group: Some(group),
                ..Default::default()
            }
        } else {
            // get a reference to our image specs security_context settings
            let ctx = &image.security_context;
            // build this pods security context
            PodSecurityContext {
                run_as_user: ctx.user,
                run_as_group: ctx.group,
                ..Default::default()
            }
        }
    }

    /// Generate the pod spec to deploy into k8s
    ///
    /// All pods generated by this will by default have a termination grace period
    /// of 0 and a resetart policy of never.
    ///
    /// # Arguments
    ///
    /// * `cache` - A cache of info from Thorium
    /// * `spawn` - The spawn to generate a pod for
    pub async fn generate(&self, cache: &Cache, spawn: &Spawned) -> Result<Pod, Error> {
        // get our user info
        let user = match cache.users.get(&spawn.req.user) {
            Some(user) => user,
            None => {
                // build our error message
                let msg = format!("User {} not in cache", spawn.req.user);
                return Err(Error::new(msg));
            }
        };
        // grab our workload info
        let workload = cache
            .get_workload(&spawn.req.tool_instance)
            .ok_or(Error::new(format!(
                "Unable to retrieve tool instance '{}' from cache",
                spawn.req.tool_instance
            )))?;
        // get the workload's image in this spawn's group
        let image = workload
            .projection(&spawn.req.group)
            .ok_or(Error::new(format!(
                "Tool instance '{}' is not in group '{}'",
                spawn.req.tool_instance, spawn.req.group
            )))?;
        // label this pod with its requisition, and its tool's name so network policies can
        // select it by tool
        let mut labels = spawn.req.labels();
        labels.insert("stage".to_owned(), k8s_label(&workload.name));
        labels.insert("pool".to_owned(), spawn.pool.as_str().to_owned());
        labels.insert("thorium".to_owned(), "true".to_owned());
        labels.insert("name".to_owned(), spawn.name.clone());
        // build base pod spec without a name
        let raw = json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "namespace": &spawn.req.group,
                "name": &spawn.name,
                "labels": labels,
            },
            "spec": {
                "containers": self.containers.generate(cache, spawn, user)?,
                "nodeSelector": {"thorium": "enabled"},
                "nodeName": spawn.node,
                "hostAliases": &self.host_aliases,
            }
        });
        // cast this json into a barebones pod
        let mut pod: Pod = serde_json::from_value(raw)?;
        // get this pod's labels (should never provide default because we always provide labels in raw)
        let pod_labels = pod.metadata.labels.get_or_insert(BTreeMap::default());
        // add the base network policy labels
        for policy in cache.conf_base_network_policies() {
            pod_labels.insert(policy.name.clone(), "base".to_string());
        }
        // add any forced network policies in this group
        if let Some(forced_policies) = cache.forced_network_policies(&image.group)? {
            for policy in forced_policies {
                pod_labels.insert(policy.k8s_name.clone(), "true".to_string());
            }
        }
        // add any user-defined network policies
        for policy_name in &image.network_policies {
            let policy_k8s_name = &cache
                .get_network_policy(&image.group, policy_name)?
                .k8s_name;
            pod_labels.insert(policy_k8s_name.clone(), "true".to_string());
        }
        // get this pods specs or build defaults
        let pod_spec = pod.spec.get_or_insert(PodSpec::default());
        // insert our image specs into this pod
        pod_spec.volumes = Some(
            self.volumes
                .generate(image, user, spawn.req.run.is_none())
                .await?,
        );
        pod_spec.image_pull_secrets = Some(self.secrets.registry_token());
        pod_spec.termination_grace_period_seconds = Some(1);
        pod_spec.restart_policy = Some("Never".to_owned());
        pod_spec.security_context = Some(Self::build_security_ctx(cache, &spawn.req.user, image));
        Ok(pod)
    }

    /// Deploys pods within a specific namespace
    ///
    /// # Arguments
    ///
    /// * `cache` - A cache of info from Thorium
    /// * `req` - The Requisition to spawn
    /// * `scale` - The number of pods to scale up or down
    #[instrument(name = "k8s::Pods::deploy", skip(self, pods, errors))]
    pub async fn deploy<'a>(&self, ns: &str, pods: Vec<Pod>, errors: &mut HashMap<String, Error>) {
        // get a namespaced client for our pods target namespace
        let api: Api<Pod> = Api::namespaced(self.client.clone(), ns);
        // set the default params
        let params = PostParams::default();
        // get the pods names in case of any errors
        let names = pods
            .iter()
            // use map instead of filter map so we don't lose Nones and mess up the order
            .map(|pod| pod.metadata.name.clone())
            .collect::<Vec<Option<String>>>();
        // deploy any new pods 5 at a time
        let deploys = stream::iter(pods)
            .map(|pod| {
                // get references to minimize cloning
                let api_ref = &api;
                let params_ref = &params;
                async move { api_ref.create(params_ref, &pod).await }
            })
            .buffered(5)
            .collect::<Vec<Result<_, _>>>()
            .await;
        // crawl our deployments and determine if any failures occured
        for (name, deploy) in names.into_iter().zip(deploys.into_iter()) {
            // check if our deployment failed
            if let Err(error) = deploy {
                // get the name of this pod
                if let Some(name) = name {
                    // this attempt failed so track this error
                    errors.insert(name.clone(), Error::from(error));
                }
            }
        }
    }

    /// Clear out any failing or completed pods
    ///
    /// # Arguments
    ///
    /// * `namespaces` - The namespaces to check for failing pods in
    /// * `failed` - The names of the failed pods
    /// * `terminal` - The names of the terminal pods
    /// * `error_out` - The pods whose workers we should fail out instead of just resetting
    #[instrument(name = "k8s::Pods::clear_failing", skip_all)]
    pub async fn clear_failing(
        &self,
        namespaces: &HashSet<String>,
        failed: &mut HashSet<String>,
        terminal: &mut HashSet<String>,
        error_out: &mut HashSet<ErrorOutKinds>,
    ) -> Result<(), Error> {
        // iterate over groups
        for namespace in namespaces {
            // get pods in this group
            let pods = self.list(namespace, None).await?;
            // skip any pods not owned by Thorium
            let pods_iter = pods
                .into_iter()
                // filter any pods not owned by Thorium
                .filter(Self::thorium_owned)
                // skip any pods without names
                .filter(|pod| pod.metadata.name.is_some());
            // track all succeeded pods in this namespace
            let mut succeeded = Vec::default();
            // build a list of failed and errored out pods in this namespace
            let mut failed_ns = Vec::default();
            let mut error_out_ns = Vec::default();
            // crawl over these pods and determine if they need to be deleted
            for pod in pods_iter {
                // try to check if this pod is failing
                if let Err(error) =
                    Self::filter_terminal(&pod, &mut failed_ns, &mut succeeded, &mut error_out_ns)
                {
                    // log this error and ignore the pod for now
                    event!(
                        Level::ERROR,
                        pod = pod.metadata.name,
                        error = error.to_string()
                    );
                }
            }
            // only delete failed pods if there exists some to delete
            if !failed_ns.is_empty() {
                // log how many failed pods to delete
                event!(
                    Level::INFO,
                    failing = failed_ns.len(),
                    namespace = namespace
                );
                // extend our failed pod list
                failed.extend(failed_ns.iter().cloned());
                // delete this groups failed pods 10 at a time
                self.delete_many_owned(namespace, failed_ns).await?;
            }
            // only delete error out pods if there exists some to delete
            if !error_out_ns.is_empty() {
                // log how many failed pods to delete
                event!(
                    Level::INFO,
                    error_out = error_out_ns.len(),
                    namespace = namespace
                );
                // build a list of the pods we are deleting
                // we clone the strings to work around some FnOnce is not general enough errors
                let pod_names = error_out_ns
                    .iter()
                    .map(|kind| kind.worker())
                    .collect::<Vec<&String>>();
                // try delete this groups failed pods 10 at a time
                // if we fail thats fine we can try again later
                self.try_delete_many(namespace, pod_names).await;
                // extend our failed pod list
                error_out.extend(error_out_ns);
            }

            // extend our terminal pod list
            terminal.extend(succeeded.iter().cloned());
            // delete any succeeded pods if we have them
            if !succeeded.is_empty() {
                // delete any succeeded pods
                self.delete_many_owned(namespace, succeeded).await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::libs::DockerInfo;
    use crate::libs::schedulers::requisitions::worker_name;
    use crate::libs::test_api::{self, GROUP, MockApi, MockCrane, World, k8s_spec, tool_instance};
    use k8s_openapi::api::core::v1::{
        ContainerState, ContainerStateWaiting, ContainerStatus, PodStatus,
    };
    use k8s_openapi::api::networking::v1::NetworkPolicyIngressRule;
    use thorium::models::{
        ImageScaler, NetworkPolicy, NetworkPolicyRule, Pools, RUN_LABEL, Requisition,
        SecurityContext, SpawnLimits, WorkloadEnvironment, k8s_name,
    };
    use uuid::Uuid;

    /// The tool instance the pods in these tests run
    const GHIDRA: Uuid = Uuid::from_u128(0x6768_6964_7261);

    /// Build a pods wrapper whose kube client is never called
    ///
    /// # Arguments
    ///
    /// * `conf` - The Thorium config
    fn pods(conf: &Conf) -> Pods {
        // kube's client needs a crypto provider like the scaler installs at startup
        let _ = rustls::crypto::ring::default_provider().install_default();
        // a client for a cluster that is never contacted
        let config = kube::Config::new("http://127.0.0.1:9".parse().unwrap());
        let client = kube::Client::try_from(config).unwrap();
        Pods::new(&client, conf, "cluster0", "ctx")
    }

    /// Build a cache holding an inspected `ghidra-cli` tool instance and a user with unix info
    ///
    /// # Arguments
    ///
    /// * `security_context` - The workload's security context
    /// * `network_policies` - The network policies the workload names
    async fn ghidra_cache(
        security_context: SecurityContext,
        network_policies: &[&str],
    ) -> (crate::libs::Cache, MockApi) {
        // serve a K8s ghidra-cli tool instance
        let mut spec = k8s_spec("registry.example/ghidra-cli:11.1", SpawnLimits::Unlimited);
        if let WorkloadEnvironment::K8s(k8s) = &mut spec.environment {
            k8s.security_context = security_context;
            k8s.network_policies = network_policies
                .iter()
                .map(|name| (*name).to_owned())
                .collect();
        }
        let mut world = World::default();
        world.entities.insert(
            GHIDRA,
            tool_instance(GHIDRA, "ghidra-cli", "11.1", &[GROUP], spec),
        );
        let api = MockApi::start(world).await;
        let crane = MockCrane::new();
        let mut cache = test_api::cache(&api, test_api::conf(&crane), ImageScaler::K8s, &[]).await;
        // cache a user with unix info
        let mut user = test_api::user("io", &[GROUP]);
        user.unix = Some(thorium::models::UnixInfo {
            user: 1001,
            group: 1002,
        });
        cache.users.insert("io".to_owned(), user);
        // cache and inspect the tool instance
        let thorium = api.client().await;
        cache
            .prefetch(&thorium, [GHIDRA].into_iter().collect())
            .await;
        cache.docker.insert(
            "registry.example/ghidra-cli:11.1".to_owned(),
            DockerInfo::deserialize(
                r#"{"architecture":"amd64","os":"linux","config":{"Entrypoint":["/run"]}}"#,
            )
            .unwrap(),
        );
        (cache, api)
    }

    /// Build a spawn of the `ghidra-cli` tool instance
    ///
    /// # Arguments
    ///
    /// * `cache` - The cache holding the tool instance
    /// * `run` - The run the spawn is limited to, if any
    fn ghidra_spawn(cache: &crate::libs::Cache, run: Option<Uuid>) -> Spawned {
        let workload = cache.get_workload(&GHIDRA).unwrap();
        let image = workload.projection(GROUP).unwrap();
        Spawned::new(
            "cluster0",
            "node0",
            Requisition::new("io", GROUP, GHIDRA, run),
            &workload,
            image,
            Pools::Deadline,
        )
    }

    /// Get the string labels of a pod
    ///
    /// # Arguments
    ///
    /// * `pod` - The pod to get the labels of
    fn labels(pod: &Pod) -> BTreeMap<String, String> {
        pod.metadata.labels.clone().unwrap()
    }

    /// Get the value of a container env var, if it's set
    ///
    /// # Arguments
    ///
    /// * `container` - The container
    /// * `name` - The env var's name
    fn env_value(container: &k8s_openapi::api::core::v1::Container, name: &str) -> Option<String> {
        container
            .env
            .as_ref()
            .and_then(|env| env.iter().find(|var| var.name == name))
            .and_then(|var| var.value.clone())
    }

    /// A per-run pod gets its run's token, and a harness's its models, as literal env values,
    /// has no keys secret volume or keys argument but an empty dir at the keys path, and carries
    /// its run label; a shared pod keeps the user's keys and gets none of those values, and a
    /// per-run spawn whose env wasn't fetched isn't built
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn per_run_pod_env_and_keys_dir() {
        // a per-run spawn of ghidra-cli with a harness's env, whose image sets its own token var
        let (cache, _api) = ghidra_cache(SecurityContext::default(), &[]).await;
        let run = Uuid::new_v4();
        let mut spawn = ghidra_spawn(&cache, Some(run));
        let models = thorium::models::RunModels {
            models: thorium::models::HarnessModels::default(),
            env: [(
                "ANTHROPIC_API_KEY".to_owned(),
                thorium::models::ModelSecret::from("sk-pod-model-secret"),
            )]
            .into_iter()
            .collect(),
        };
        spawn.run_env = Some(crate::libs::schedulers::RunPodEnv {
            token: "pod-run-token".to_owned(),
            models: Some(models.clone()),
        });
        {
            let mut workloads = cache.workloads.write().unwrap();
            let entry = workloads.get_mut(&GHIDRA).unwrap();
            let mut workload = (*entry.workload).clone();
            if let Some(image) = workload.projections.get_mut(GROUP) {
                image
                    .env
                    .insert("THORIUM_TOKEN".to_owned(), Some("image-token".to_owned()));
            }
            entry.workload = std::sync::Arc::new(workload);
        }
        let pod = pods(&cache.conf).generate(&cache, &spawn).await.unwrap();
        let spec = pod.spec.clone().unwrap();
        let container = &spec.containers[0];
        // the url is the one the user keys secrets name, and the token is the run's
        let url = cache
            .conf
            .thorium
            .scaler
            .k8s
            .resolved_api_url("ctx", &cache.conf.thorium.namespace);
        assert_eq!(env_value(container, "THORIUM_URL"), Some(url));
        assert_eq!(
            env_value(container, "THORIUM_TOKEN"),
            Some("pod-run-token".to_owned())
        );
        let tokens = container
            .env
            .as_ref()
            .unwrap()
            .iter()
            .filter(|var| var.name == "THORIUM_TOKEN")
            .count();
        assert_eq!(tokens, 1);
        assert_eq!(
            env_value(container, "THORIUM_MODELS"),
            Some(serde_json::to_string(&models.models).unwrap())
        );
        assert_eq!(
            env_value(container, "ANTHROPIC_API_KEY"),
            Some("sk-pod-model-secret".to_owned())
        );
        // the agent gets its tool instance and no keys file
        let args = container.args.clone().unwrap();
        assert!(!args.contains(&"--keys".to_owned()));
        let position = args
            .iter()
            .position(|arg| arg == "--tool-instance")
            .unwrap();
        assert_eq!(args[position + 1], GHIDRA.to_string());
        // no keys secret is mounted, only an empty dir at the keys path
        let volumes = spec.volumes.clone().unwrap();
        assert!(volumes.iter().all(|vol| vol.secret.is_none()));
        let keys_dir = volumes
            .iter()
            .find(|vol| vol.name == "thorium-run-keys")
            .unwrap();
        assert!(keys_dir.empty_dir.is_some());
        let mounts = container.volume_mounts.clone().unwrap();
        let keys_mount = mounts
            .iter()
            .find(|mount| mount.mount_path == "/opt/thorium-keys")
            .unwrap();
        assert_eq!(keys_mount.name, "thorium-run-keys");
        assert_ne!(keys_mount.read_only, Some(true));
        assert!(!mounts.iter().any(|mount| mount.name == "thorium-io-keys"));
        // the pod carries its run's label and runs as the user's unix ids
        assert_eq!(labels(&pod)[RUN_LABEL], run.to_string());
        let ctx = spec.security_context.unwrap();
        assert_eq!(
            (ctx.run_as_user, ctx.run_as_group),
            (Some(1001), Some(1002))
        );
        // an Image worker of the run gets the url and token and no models
        spawn.run_env = Some(crate::libs::schedulers::RunPodEnv {
            token: "pod-run-token".to_owned(),
            models: None,
        });
        let pod = pods(&cache.conf).generate(&cache, &spawn).await.unwrap();
        let container = &pod.spec.unwrap().containers[0];
        assert!(env_value(container, "THORIUM_TOKEN").is_some());
        assert!(env_value(container, "THORIUM_MODELS").is_none());
        assert!(env_value(container, "ANTHROPIC_API_KEY").is_none());
        // a per-run spawn whose env wasn't fetched is refused
        spawn.run_env = None;
        assert!(pods(&cache.conf).generate(&cache, &spawn).await.is_err());
        // a shared pod keeps the user's keys and gets no run values
        let shared = ghidra_spawn(&cache, None);
        let pod = pods(&cache.conf).generate(&cache, &shared).await.unwrap();
        let spec = pod.spec.unwrap();
        let container = &spec.containers[0];
        for name in ["THORIUM_URL", "THORIUM_MODELS", "ANTHROPIC_API_KEY"] {
            assert!(env_value(container, name).is_none(), "{name}");
        }
        assert_eq!(
            env_value(container, "THORIUM_TOKEN"),
            Some("image-token".to_owned())
        );
        assert!(
            container
                .args
                .clone()
                .unwrap()
                .contains(&"--keys".to_owned())
        );
        assert!(spec.volumes.unwrap().iter().any(|vol| {
            vol.secret
                .as_ref()
                .is_some_and(|secret| secret.secret_name.as_deref() == Some("thorium-io-keys"))
        }));
    }

    /// A shared workload pod is named after its tool and version, labeled with its tool instance
    /// and its tool's name, runs the agent for its tool instance with the user's keys, and runs as
    /// the user's unix ids unless the workload's security context sets its own
    #[tokio::test]
    async fn shared_workload_pod_shape() {
        // a shared spawn of ghidra-cli 11.1
        let (cache, _api) = ghidra_cache(SecurityContext::default(), &[]).await;
        let spawn = ghidra_spawn(&cache, None);
        let pod = pods(&cache.conf).generate(&cache, &spawn).await.unwrap();
        // the pod is named after the tool and version with a random suffix
        let name = pod.metadata.name.clone().unwrap();
        let suffix = name.strip_prefix("ghidra-cli-11-1-").unwrap();
        assert_eq!(suffix.len(), 8);
        // the labels name the tool instance and tool and no pipeline or run
        let labels = labels(&pod);
        assert_eq!(labels["user"], "io");
        assert_eq!(labels["group"], GROUP);
        assert_eq!(labels["pool"], "Deadline");
        assert_eq!(labels["thorium"], "true");
        assert_eq!(labels["name"], name);
        assert_eq!(labels[TOOL_INSTANCE_LABEL], GHIDRA.to_string());
        assert_eq!(labels["stage"], "ghidra-cli");
        assert!(!labels.contains_key("pipeline"));
        assert!(!labels.contains_key(RUN_LABEL));
        // the container is named after the tool and runs the agent for the tool instance
        let spec = pod.spec.clone().unwrap();
        let container = &spec.containers[0];
        assert_eq!(container.name, "ghidra-cli");
        let args = container.args.clone().unwrap();
        let position = args
            .iter()
            .position(|arg| arg == "--tool-instance")
            .unwrap();
        assert_eq!(args[position + 1], GHIDRA.to_string());
        assert!(args.contains(&"--keys".to_owned()));
        assert!(!args.contains(&"--pipeline".to_owned()));
        assert!(!args.contains(&"--stage".to_owned()));
        // resources come from the workload's scheduling settings
        let requests = container.resources.clone().unwrap().requests.unwrap();
        assert_eq!(requests["cpu"].0, "1000m");
        // the user's keys secret is mounted at the keys path
        let volumes = spec.volumes.clone().unwrap();
        assert!(volumes.iter().any(|vol| {
            vol.secret
                .as_ref()
                .is_some_and(|secret| secret.secret_name.as_deref() == Some("thorium-io-keys"))
        }));
        let mounts = container.volume_mounts.clone().unwrap();
        assert!(mounts.iter().any(
            |mount| mount.name == "thorium-io-keys" && mount.mount_path == "/opt/thorium-keys"
        ));
        // the pod runs as the user's unix ids
        let ctx = spec.security_context.unwrap();
        assert_eq!(
            (ctx.run_as_user, ctx.run_as_group),
            (Some(1001), Some(1002))
        );
        // a workload security context that sets its own ids wins
        let custom = SecurityContext {
            user: Some(7),
            group: Some(8),
            ..SecurityContext::default()
        };
        let (cache, _api) = ghidra_cache(custom, &[]).await;
        let pod = pods(&cache.conf)
            .generate(&cache, &ghidra_spawn(&cache, None))
            .await
            .unwrap();
        let ctx = pod.spec.unwrap().security_context.unwrap();
        assert_eq!((ctx.run_as_user, ctx.run_as_group), (Some(7), Some(8)));
    }

    /// A pod's labels give back its requisition with its tool instance and run, and only pods
    /// with a tool instance label are owned by Thorium
    #[tokio::test]
    async fn pod_labels_round_trip() {
        // a shared and a per-run pod
        let (cache, _api) = ghidra_cache(SecurityContext::default(), &[]).await;
        let run = Uuid::new_v4();
        for run in [None, Some(run)] {
            let mut spawn = ghidra_spawn(&cache, run);
            // a per-run spawn has its env values when it's built
            if run.is_some() {
                spawn.run_env = Some(crate::libs::schedulers::RunPodEnv {
                    token: "token".to_owned(),
                    models: None,
                });
            }
            let pod = pods(&cache.conf).generate(&cache, &spawn).await.unwrap();
            // the labels rebuild the spawn's requisition
            assert_eq!(Requisition::from_labels(&labels(&pod)).unwrap(), spawn.req);
            assert!(Pods::thorium_owned(&pod));
            // a pod without a tool instance label isn't ours
            let mut unowned = pod.clone();
            unowned
                .metadata
                .labels
                .as_mut()
                .unwrap()
                .remove(TOOL_INSTANCE_LABEL);
            assert!(!Pods::thorium_owned(&unowned));
        }
    }

    /// A workload pod carries the labels of the network policies it names and the stage label
    /// holding its tool's name; a policy peer naming a tool instance selects that tool
    /// instance's pods and a peer naming a tool selects its stage label
    #[tokio::test]
    async fn workload_pod_network_policies() {
        // a workload naming a policy in its group
        let (mut cache, _api) = ghidra_cache(SecurityContext::default(), &["models"]).await;
        let policy: NetworkPolicy = serde_json::from_value(serde_json::json!({
            "name": "models",
            "id": Uuid::new_v4(),
            "k8s_name": "models-1234",
            "groups": [GROUP],
            "created": chrono::Utc::now(),
            "forced_policy": false,
            "default_policy": false,
            "used_by": {},
        }))
        .unwrap();
        cache.network_policies.insert(policy);
        let pod = pods(&cache.conf)
            .generate(&cache, &ghidra_spawn(&cache, None))
            .await
            .unwrap();
        let labels = labels(&pod);
        assert_eq!(labels["models-1234"], "true");
        assert_eq!(labels["stage"], "ghidra-cli");
        // allowed tools build peers by tool instance or by the tool's stage label
        let rule: NetworkPolicyRule = serde_json::from_value(serde_json::json!({
            "id": Uuid::new_v4(),
            "allowed_tools": [GHIDRA.to_string(), "Ghidra CLI", "migrated1"],
        }))
        .unwrap();
        let k8s_rule = NetworkPolicyIngressRule::from(rule);
        let selectors = k8s_rule
            .from
            .unwrap()
            .into_iter()
            .filter_map(|peer| peer.pod_selector.and_then(|selector| selector.match_labels))
            .collect::<Vec<_>>();
        assert_eq!(selectors[0][TOOL_INSTANCE_LABEL], GHIDRA.to_string());
        assert_eq!(selectors[1]["stage"], k8s_label("Ghidra CLI"));
        assert_eq!(selectors[1]["stage"], "ghidra-cli");
        // a migrated image's name selects itself unchanged
        assert_eq!(selectors[2]["stage"], "migrated1");
    }

    /// Worker names, container names, and stage labels are valid K8s names for any tool name
    /// and version
    #[tokio::test]
    async fn k8s_names_for_any_tool() {
        // the pattern every K8s name must match
        let valid = |name: &str| {
            name.len() <= 63
                && !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && !name.starts_with('-')
                && !name.ends_with('-')
        };
        // names with uppercase letters, spaces, underscores, and odd version labels
        for (tool, version) in [
            ("Ghidra CLI", "11.1"),
            ("my_tool", "1.0.0+build.5"),
            ("_lead", "v2"),
            ("emoji😄tool", "beta"),
        ] {
            let name = worker_name(tool, version, "abcd1234");
            assert!(valid(&name), "{name} is not a valid K8s name");
            assert!(name.ends_with("-abcd1234"));
            assert!(valid(&k8s_label(tool)), "{tool}");
        }
        assert_eq!(
            worker_name("my_tool", "1.0.0+build.5", "abcd1234"),
            "my-tool-1-0-0-build-5-abcd1234"
        );
        // a migrated image's name is unchanged
        assert_eq!(
            worker_name("husker", "1.0.0", "abcd1234"),
            "husker-1-0-0-abcd1234"
        );
        assert_eq!(k8s_label("husker"), "husker");
        // an 80 character tool name is trimmed so the whole version and suffix fit
        let long = "t".repeat(80);
        let name = worker_name(&long, "2.5.1", "abcd1234");
        assert_eq!(name.len(), 63);
        assert!(name.ends_with("-2-5-1-abcd1234"));
        assert!(valid(&name));
        assert_eq!(k8s_name("A_B", 63), "a-b");
        // the container name and the stage label use the same helper
        let (cache, _api) = ghidra_cache(SecurityContext::default(), &[]).await;
        let pod = pods(&cache.conf)
            .generate(&cache, &ghidra_spawn(&cache, None))
            .await
            .unwrap();
        assert_eq!(
            pod.spec.clone().unwrap().containers[0].name,
            labels(&pod)["stage"]
        );
    }

    /// Build a Pending pod whose single container is waiting with the given reason
    ///
    /// This models an unpullable-image pod as the kubelet actually reports it: phase
    /// `Pending` with `containerStatuses[].state.waiting.reason` set (never a terminated
    /// reason, and never phase `Failed`).
    ///
    /// # Arguments
    ///
    /// * `name` - The name to give the pod
    /// * `waiting_reason` - The container waiting reason to set (e.g. "ImagePullBackOff")
    fn pending_pod_waiting(name: &str, waiting_reason: &str) -> Pod {
        // build the waiting container state carrying the reason
        let waiting = ContainerStateWaiting {
            reason: Some(waiting_reason.to_owned()),
            message: None,
        };
        // wrap it in a container status
        let container_status = ContainerStatus {
            state: Some(ContainerState {
                waiting: Some(waiting),
                running: None,
                terminated: None,
            }),
            ..Default::default()
        };
        // build a Pending pod carrying that container status
        let mut pod = Pod::default();
        pod.metadata.name = Some(name.to_owned());
        pod.status = Some(PodStatus {
            phase: Some("Pending".to_owned()),
            container_statuses: Some(vec![container_status]),
            ..Default::default()
        });
        pod
    }

    /// A pod whose image cannot be pulled stays phase `Pending` with
    /// `state.waiting.reason = "ImagePullBackOff"` (capital O); `filter_terminal` routes such a
    /// pod to `error_out` so its job is failed out instead of the stuck pod never being reaped.
    #[test]
    fn pending_image_pull_backoff_is_errored_out() {
        // a pod stuck pulling its image as the kubelet reports it
        let pod = pending_pod_waiting("stuck-worker", "ImagePullBackOff");
        // buckets the classifier sorts pods into
        let mut failed = Vec::new();
        let mut succeeded = Vec::new();
        let mut error_out = Vec::new();
        // classify the stuck pod
        Pods::filter_terminal(&pod, &mut failed, &mut succeeded, &mut error_out).unwrap();
        // the stuck pod must be scheduled to have its job failed out
        assert!(
            error_out.iter().any(|kind| kind.worker() == "stuck-worker"),
            "a Pending pod in ImagePullBackOff must be routed to error_out, got failed={failed:?} \
             succeeded={succeeded:?} error_out={error_out:?}"
        );
    }

    /// Build a pending delete for a worker with the given name
    ///
    /// # Arguments
    ///
    /// * `name` - The unique name of the worker being deleted
    fn pending_delete(name: &str) -> Spawned {
        // build a spawned record with only the fields classification reads set
        Spawned {
            req: thorium::models::Requisition::new("user", "group", uuid::Uuid::nil(), None),
            cluster: "cluster".to_owned(),
            node: "node".to_owned(),
            name: name.to_owned(),
            resources: thorium::models::Resources::default(),
            pool: thorium::models::Pools::Deadline,
            spawn: false,
            scaled_down: true,
            down_scalable: chrono::Utc::now(),
            run_env: None,
        }
    }

    /// A prior pending delete whose pod is ABSENT from the namespace listing (`condensed`, the
    /// set of pods that STILL EXIST) is reported Deleted and removed from the pending deletes, so
    /// a genuinely deleted worker does not leak in pending_deletes forever.
    #[test]
    fn absent_pending_pod_is_reported_deleted() {
        // a worker "W" was scaled down and is a prior pending delete
        let mut ns_map = HashMap::default();
        ns_map.insert("W".to_owned(), pending_delete("W"));
        // the namespace listing is now empty, so W's pod is genuinely gone
        let condensed: HashSet<&String> = HashSet::new();
        // scratch space and the resulting deletions
        let mut temp = Vec::new();
        let mut results = Vec::new();
        // classify the prior pending deletes against the current namespace listing
        let remaining = Pods::classify_pending_deletes(&condensed, ns_map, &mut temp, &mut results);
        // W's pod is gone so it must be reported deleted
        assert!(
            results.iter().any(
                |deletion| matches!(deletion, WorkerDeletion::Deleted(spawn) if spawn.name == "W")
            ),
            "a pending pod absent from the namespace must be reported Deleted, got {results:?}"
        );
        // and it must not leak back into pending deletes
        assert!(
            !remaining.contains_key("W"),
            "a deleted worker must not remain in pending deletes"
        );
    }

    /// A prior pending delete whose pod is STILL present in the namespace listing remains
    /// pending and is not reported deleted, so a still-running pod does not have its resources
    /// freed while it is still running.
    #[test]
    fn present_pending_pod_stays_pending() {
        // a worker "W" was scaled down and is a prior pending delete
        let mut ns_map = HashMap::default();
        ns_map.insert("W".to_owned(), pending_delete("W"));
        // W's pod is still listed in the namespace, so it has not finished terminating
        let name = "W".to_owned();
        let mut condensed: HashSet<&String> = HashSet::new();
        condensed.insert(&name);
        // scratch space and the resulting deletions
        let mut temp = Vec::new();
        let mut results = Vec::new();
        // classify the prior pending deletes against the current namespace listing
        let remaining = Pods::classify_pending_deletes(&condensed, ns_map, &mut temp, &mut results);
        // W's pod still exists so nothing should be reported deleted
        assert!(
            results.is_empty(),
            "a pod still present in the namespace must not be reported deleted, got {results:?}"
        );
        // and it must stay pending until its pod actually disappears
        assert!(
            remaining.contains_key("W"),
            "a still-present pod must remain in pending deletes"
        );
    }
}
