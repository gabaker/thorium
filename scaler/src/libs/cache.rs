//! A cache of data in Thorium

use chrono::prelude::*;
use futures::stream::{self, StreamExt};
use hashbrown::HashMap;
use reqwest::StatusCode;
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use thorium::conf::{BaseNetworkPolicy, Conf};
use thorium::models::{
    Entity, EntityMetadata, Image, ImageScaler, NetworkPolicy, NetworkPolicyListOpts, ScrubbedUser,
    SystemSettings, ToolKind, ToolSpec, WorkloadEnvironment, WorkloadRuntimes, WorkloadSpec,
};
use thorium::{Error, Keys, Thorium};
use tracing::{Level, Span, event, span};
use uuid::Uuid;

use crate::{raw_entry_map_insert, raw_entry_vec_push};

use super::{DockerInfo, helpers::assert_send_stream, tasks::TaskResult};

/// Docker info for the images of cached workloads by image reference
pub type DockerInfoCache = HashMap<String, DockerInfo>;

/// The max number of workload specs fetched at once
const SPEC_FETCHES_IN_FLIGHT: usize = 10;

/// The runtime in seconds a tool instance without a recorded runtime is scheduled with
const DEFAULT_WORKLOAD_RUNTIME: f64 = 600.0;

/// A tool instance's workload as the scaler caches it
#[derive(Debug, Clone)]
pub struct CachedWorkload {
    /// The tool's name
    pub name: String,
    /// The tool instance's version label
    pub version: String,
    /// The tool instance's groups
    pub groups: Vec<String>,
    /// The kind of tool this instance is
    pub kind: ToolKind,
    /// The tool instance's workload spec
    pub spec: WorkloadSpec,
    /// The in-memory image projection of the spec in each of the tool instance's groups, built
    /// with the current runtime
    pub projections: BTreeMap<String, Image>,
}

impl CachedWorkload {
    /// Build the cached workload of a tool instance entity
    ///
    /// Returns `None` if the entity isn't a tool instance that runs a workload.
    ///
    /// # Arguments
    ///
    /// * `entity` - The tool instance entity
    /// * `runtime` - The tool instance's runtime in seconds
    #[must_use]
    pub fn new(entity: &Entity, runtime: f64) -> Option<Self> {
        // only Image and Harness tool instances run a workload
        let EntityMetadata::ToolInstance(meta) = &entity.metadata else {
            return None;
        };
        let (kind, spec) = match &meta.spec {
            ToolSpec::Image(spec) => (ToolKind::Image, spec.clone()),
            ToolSpec::Harness(harness) => (ToolKind::Harness, harness.workload.clone()),
            ToolSpec::Agentic(_) => return None,
        };
        // a tool instance is named after its tool and version
        let version = meta.version.to_string();
        let name = entity
            .name
            .strip_suffix(&format!("@{version}"))
            .unwrap_or(&entity.name)
            .to_owned();
        // build the workload and project it in each of its groups
        let mut workload = CachedWorkload {
            name,
            version,
            groups: entity.groups.clone(),
            kind,
            spec,
            projections: BTreeMap::default(),
        };
        workload.project(runtime);
        Some(workload)
    }

    /// Rebuild this workload's projections with a runtime
    ///
    /// # Arguments
    ///
    /// * `runtime` - The tool instance's runtime in seconds
    fn project(&mut self, runtime: f64) {
        // project the spec into an image in each group the tool instance is in
        self.projections = self
            .groups
            .iter()
            .map(|group| {
                let image = self.spec.project_image(group, &self.name, runtime);
                (group.clone(), image)
            })
            .collect();
    }

    /// Get the image projection of this workload in a group
    ///
    /// # Arguments
    ///
    /// * `group` - The group the worker runs in
    #[must_use]
    pub fn projection(&self, group: &str) -> Option<&Image> {
        self.projections.get(group)
    }

    /// Get the scaler that schedules this workload
    #[must_use]
    pub fn scaler(&self) -> ImageScaler {
        self.spec.environment.scaler()
    }

    /// Get the container image reference of a K8s workload
    #[must_use]
    pub fn image_ref(&self) -> Option<&str> {
        // only K8s workloads have an image the scaler inspects
        match &self.spec.environment {
            WorkloadEnvironment::K8s(k8s) => Some(&k8s.image),
            _ => None,
        }
    }
}

/// A cached workload and the last time it was in demand
#[derive(Debug, Clone)]
pub struct WorkloadEntry {
    /// The cached workload
    pub workload: Arc<CachedWorkload>,
    /// The last time a deadline or the stats named this workload's tool instance
    pub seen: DateTime<Utc>,
}

/// Determine whether the Thorium client token should be refreshed early
///
/// This is the token-expiry check in [`refresh_client`] (it never needs a live client)
/// extracted so its logic can be unit tested without a Thorium client. A token that expires
/// within the next week (or whose expiration is unknown) is refreshed early.
///
/// # Arguments
///
/// * `expires` - When the current token expires, if known
/// * `now` - The current timestamp to compare the expiry against
fn should_refresh_client(expires: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    // refresh our token if it expires within the next week
    expires < Some(now + chrono::Duration::weeks(1))
}

/// Reads in the Thorium keys and refreshes the client if its needed
///
/// # Arguments
///
/// * `thorium` - The Thorium client to refresh
/// * `keys` - The keys to use when refreshing our client
/// * `span` - The span to log traces under
async fn refresh_client(
    thorium: &mut Arc<Thorium>,
    keys: &String,
    span: &Span,
) -> Result<(), Error> {
    // check to see if our Thorium client token will expire in the next week and refresh it early
    // we refresh it a week early since the Thorium client is shared and we may not get a
    // mutable reference on the first try
    if should_refresh_client(thorium.expires, Utc::now()) {
        // start our Thorium auth refresh span
        let span = span!(parent: span, Level::INFO, "Thorium Client Refresh");
        // reload auth keys
        let keys = Keys::new(keys).expect("Failed to load auth keys");
        // panic if username is not known
        assert!(
            keys.username.is_some() && keys.password.is_some(),
            "keys.yml must contain username/password"
        );
        // try to get a mutable reference to our Thorium client
        if let Some(thorium) = Arc::get_mut(thorium) {
            thorium
                .refresh(keys.username.unwrap(), keys.password.unwrap())
                .await?;
        } else {
            // keep trying to refresh our token until it expires then panic
            if thorium.expires > Some(Utc::now()) {
                event!(
                    parent: &span,
                    Level::WARN,
                    msg = "Failed to get mutable ref to client"
                );
            } else {
                event!(
                    parent: &span,
                    Level::ERROR,
                    msg = "Failed to refresh expired token"
                );
            }
        }
    }
    Ok(())
}

/// A cache of network policy info in Thorium
#[derive(Debug, Default)]
pub struct NetworkPolicyInfoCache {
    // Hash map of network policy info by id
    pub policies_by_id: HashMap<Uuid, NetworkPolicy>,
    /// Hash map of network policies' ids by their group and name
    ids_by_group_name: HashMap<String, HashMap<String, Uuid>>,
    /// Hash map of network policies' ids by their group and K8's name
    pub ids_by_group_k8s_name: HashMap<String, HashMap<String, Uuid>>,
    /// Hash map of network policy's ids that should always be applied by group
    forced_ids_by_group: HashMap<String, Vec<Uuid>>,
    /// ID's of policies that were added this cache reload
    pub policies_added: Vec<Uuid>,
    /// K8's names of policies that were removed this cache reload,
    /// mapped by group/namespace
    pub policies_removed: HashMap<String, Vec<String>>,
}

impl NetworkPolicyInfoCache {
    /// Add a policy to this cache as a reload would
    ///
    /// # Arguments
    ///
    /// * `policy` - The policy to add
    #[cfg(test)]
    pub fn insert(&mut self, policy: NetworkPolicy) {
        // index the policy by each of its groups
        for group in &policy.groups {
            raw_entry_map_insert!(
                self.ids_by_group_name,
                group,
                policy.name.clone(),
                policy.id
            );
            raw_entry_map_insert!(
                self.ids_by_group_k8s_name,
                group,
                policy.k8s_name.clone(),
                policy.id
            );
            // forced policies always apply in their groups
            if policy.forced_policy {
                raw_entry_vec_push!(self.forced_ids_by_group, group, policy.id);
            }
        }
        self.policies_by_id.insert(policy.id, policy);
    }
}

/// A local cache of data in the Thorium DB
///
/// This cache is reset based on both time and events in the API itself. It
/// will reset every 10 minutes by default or whenever an update is made in
/// the API.
pub struct Cache {
    /// The Thorium config
    pub conf: Conf,
    /// A client for Thorium
    thorium: Arc<Thorium>,
    /// System settings for Thorium
    pub settings: SystemSettings,
    /// A map of users in Thorium
    pub users: HashMap<String, ScrubbedUser>,
    /// A set of groups in Thorium
    pub groups: HashSet<String>,
    /// The scaler this cache is for
    pub scaler: ImageScaler,
    /// The workloads of the tool instances in demand, filled the first time each is in demand
    pub workloads: RwLock<HashMap<Uuid, WorkloadEntry>>,
    /// The tool instances in demand that have no workload, until the next reload
    pub missing: RwLock<HashSet<Uuid>>,
    /// The cached tool instances fetched again because they were in demand in a new group,
    /// until the next reload
    refetched: RwLock<HashSet<Uuid>>,
    /// The runtime of each tool instance
    pub runtimes: WorkloadRuntimes,
    /// Whether a workload was added whose image needs a reload to be inspected
    reload_wanted: AtomicBool,
    /// A cache of network policy info in Thorium
    pub network_policies: NetworkPolicyInfoCache,
    /// A map of docker image info for our images
    pub docker: DockerInfoCache,
    // The timestamp this cache will be invalidated and reloaded at
    pub expires: DateTime<Utc>,
    /// The path to reload our auth keys from
    pub auth_keys: String,
}

impl std::fmt::Debug for Cache {
    /// Write the parts of this cache worth logging
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cache")
            .field("settings", &self.settings)
            .field("users", &self.users)
            .field("groups", &self.groups)
            .field("scaler", &self.scaler)
            .field("workloads", &self.workloads)
            .field("missing", &self.missing)
            .field("runtimes", &self.runtimes)
            .field("network_policies", &self.network_policies)
            .field("docker", &self.docker)
            .field("expires", &self.expires)
            .finish_non_exhaustive()
    }
}

impl Cache {
    /// Creates a new cache object
    ///
    /// # Arguments
    ///
    /// * `thorium` - A Thorium client
    /// * `conf` - The Thorium config
    /// * `auth_keys` - The path to reload our auth keys from
    /// * `scaler` - The scaler this cache is for
    /// * `span` - The span to log traces under
    pub async fn new(
        thorium: Arc<Thorium>,
        conf: Conf,
        auth_keys: String,
        scaler: ImageScaler,
        span: &Span,
    ) -> Result<Cache, Error> {
        // create a new empty Cache instance
        let mut cache = Cache::blank(thorium, conf, auth_keys, scaler);
        // load this cache with data
        cache.load_data(scaler, span).await?;
        Ok(cache)
    }

    /// Creates a new cache object holding no data
    ///
    /// # Arguments
    ///
    /// * `thorium` - A Thorium client
    /// * `conf` - The Thorium config
    /// * `auth_keys` - The path to reload our auth keys from
    /// * `scaler` - The scaler this cache is for
    #[must_use]
    pub fn blank(
        thorium: Arc<Thorium>,
        conf: Conf,
        auth_keys: String,
        scaler: ImageScaler,
    ) -> Self {
        Cache {
            conf,
            thorium,
            settings: SystemSettings::default(),
            users: HashMap::new(),
            groups: HashSet::new(),
            scaler,
            workloads: RwLock::default(),
            missing: RwLock::default(),
            refetched: RwLock::default(),
            runtimes: WorkloadRuntimes::default(),
            reload_wanted: AtomicBool::new(false),
            network_policies: NetworkPolicyInfoCache::default(),
            docker: HashMap::new(),
            expires: Utc::now(),
            auth_keys,
        }
    }

    /// Reload data on all users in Thorium
    async fn load_users(&mut self, span: &Span) -> Result<(), Error> {
        // start our user info reload span
        let span = span!(parent: span, Level::INFO, "Reloading User Info");
        // get details on all users
        let users = self.thorium.users.list_details().await?;
        // log how many users we got info on
        event!(parent: &span, Level::INFO, users = users.len());
        for user in users {
            // skip the Thorium system user
            if user.username != "thorium" {
                self.users.insert(user.username.clone(), user);
            }
        }
        Ok(())
    }

    /// Build the set of groups we have users for
    fn load_groups(&mut self) {
        // build a set of all groups we have users for
        for user in self.users.values() {
            for name in &user.groups {
                // skip any system groups
                if !["system", "thorium"].contains(&&name[..]) {
                    self.groups.insert(name.to_owned());
                }
            }
        }
    }

    /// Inspect the image of every cached K8s workload and rebuild each workload's projections
    /// with its current runtime
    ///
    /// # Arguments
    ///
    /// * `scaler` - The scaler's type
    /// * `span` - The span to log traces under
    async fn load_workloads(&mut self, scaler: ImageScaler, span: &Span) {
        // start our workload info reload span
        let span = span!(parent: span, Level::INFO, "Reloading Workload Info");
        // get the workloads we have cached
        let workloads = self
            .workloads
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // rebuild each workload's projections with its current runtime
        for (id, entry) in workloads.iter_mut() {
            // get this tool instance's runtime or the default
            let runtime = self
                .runtimes
                .runtimes
                .get(id)
                .copied()
                .unwrap_or(DEFAULT_WORKLOAD_RUNTIME);
            let mut workload = entry.workload.as_ref().clone();
            workload.project(runtime);
            entry.workload = Arc::new(workload);
        }
        // only K8s scalers inspect images
        if scaler != ImageScaler::K8s {
            return;
        }
        // get the unique images of our cached K8s workloads
        let images = workloads
            .values()
            .filter_map(|entry| entry.workload.image_ref())
            .map(str::to_owned)
            .collect::<HashSet<String>>();
        // inspect each unique image, a bounded number at once
        let crane = &self.conf.thorium.scaler.crane;
        let concurrency = self.conf.thorium.scaler.cache_concurrency;
        let span = &span;
        let inspected = assert_send_stream(
            stream::iter(images)
                .map(|image| async move {
                    let docker = DockerInfo::inspect(crane, &image, span).await;
                    (image, docker)
                })
                .buffer_unordered(concurrency),
        )
        .filter_map(|(image, docker)| async move {
            match docker {
                // docker image info retrieved sucessfully
                Ok(docker) => Some((image, docker)),
                Err(e) => {
                    event!(
                        parent: span,
                        Level::WARN,
                        msg = "Failed to inspect image",
                        image = image,
                        error = e.msg(),
                    );
                    None
                }
            }
        })
        .collect::<Vec<(String, DockerInfo)>>()
        .await;
        // save the docker info of each inspected image
        self.docker = inspected.into_iter().collect();
    }

    /// Get Thorium network policy info
    ///
    /// # Arguments
    ///
    /// * `scaler` - The scaler's type
    /// * `span` - The span to log traces under
    async fn load_network_policies(
        &mut self,
        scaler: ImageScaler,
        span: &Span,
    ) -> Result<(), Error> {
        // start our user info reload span
        let span = span!(parent: span, Level::INFO, "Reloading Network Policy Info");
        if scaler != ImageScaler::K8s {
            // network policies only apply to the K8's scaler, so return early if we're not K8's
            return Ok(());
        }
        // create the maps containing our data
        let mut policies_by_id: HashMap<Uuid, NetworkPolicy> = HashMap::new();
        let mut ids_by_group_name: HashMap<String, HashMap<String, Uuid>> = HashMap::new();
        let mut ids_by_group_k8s_name: HashMap<String, HashMap<String, Uuid>> = HashMap::new();
        let mut forced_ids_by_group: HashMap<String, Vec<Uuid>> = HashMap::new();
        // list all of the network policies in all Thorium groups that we have
        let mut cursor = self
            .thorium
            .network_policies
            .list_details(&NetworkPolicyListOpts::default().groups(self.groups.clone()))
            .await?;
        loop {
            for policy in cursor.data.drain(..) {
                // add all of the policy's groups to the ids by group/name map
                for group in &policy.groups {
                    raw_entry_map_insert!(ids_by_group_name, group, policy.name.clone(), policy.id);
                    raw_entry_map_insert!(
                        ids_by_group_k8s_name,
                        group,
                        policy.k8s_name.clone(),
                        policy.id
                    );
                }
                if policy.forced_policy {
                    // add the policy to forced cache if it should always be applied in its group(s)
                    for group in &policy.groups {
                        raw_entry_vec_push!(forced_ids_by_group, group, policy.id);
                    }
                }
                // add the policy to the policies by id map
                policies_by_id.insert(policy.id, policy);
            }
            if cursor.exhausted() {
                break;
            }
            cursor.refill().await?;
        }
        // log how many network policies we got info on
        event!(parent: &span, Level::INFO, network_policies = policies_by_id.len());
        // update our cache
        self.network_policies.policies_by_id = policies_by_id;
        self.network_policies.ids_by_group_name = ids_by_group_name;
        self.network_policies.ids_by_group_k8s_name = ids_by_group_k8s_name;
        self.network_policies.forced_ids_by_group = forced_ids_by_group;
        Ok(())
    }

    /// Load data into the cache
    ///
    /// # Arguments
    ///
    /// * `scaler` - The scaler's type
    /// * `span` - The span to log traces under
    pub async fn load_data(&mut self, scaler: ImageScaler, span: &Span) -> Result<(), Error> {
        // reload the system settings
        self.settings = self.thorium.system.get_settings().await?;
        // reload all cache data
        self.load_users(span).await?;
        // now that we have users build the set of their groups
        self.load_groups();
        // load all the network policies from Thorium
        self.load_network_policies(scaler, span).await?;
        // load the tool instance runtimes
        self.runtimes = self.thorium.jobs.workload_runtimes().await?;
        // inspect the images of our cached workloads and project them with their runtimes
        self.load_workloads(scaler, span).await;
        // set the new cache expiration timestamp
        self.expires =
            Utc::now() + chrono::Duration::seconds(self.conf.thorium.scaler.cache_lifetime as i64);
        Ok(())
    }

    /// Generates a new cache and loads data into it
    ///
    /// # Arguments
    ///
    /// * `thorium` - The Thorium client to refresh if its close to expiring
    /// * `conf` - A Thorium config
    /// * `auth_keys` - The path to our Thorium auth info
    /// * `scaler` - The scaler we are using
    pub async fn refresh(
        stale_cache: Arc<Self>,
        mut thorium: Arc<Thorium>,
        scaler: ImageScaler,
    ) -> Result<TaskResult, Error> {
        // start our cache refresh span
        let span = span!(Level::INFO, "Refreshing Cache");
        // clone data from our stale cache that needs to be carried over
        let conf = stale_cache.conf.clone();
        let auth_keys = stale_cache.auth_keys.clone();
        // refresh our Thorium client if its about to expire
        refresh_client(&mut thorium, &auth_keys, &span).await?;
        // build a new cache that keeps the workloads we have cached, since specs never change
        let mut cache = Cache::blank(thorium, conf, auth_keys, scaler);
        *cache
            .workloads
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = stale_cache.workload_entries();
        // load the new cache's data
        cache.load_data(scaler, &span).await?;
        // calculate any important differences between the stale cache and the new cache
        // (what's been added, deleted, updated, etc.)
        cache.diff(&stale_cache);
        Ok(TaskResult::Cache(cache))
    }

    /// Computes differences between this cache and a stale cache and
    /// saves those differences in this cache
    ///
    /// # Arguments
    ///
    /// * `stale_cache` - The stale cache we're comparing the cache to
    fn diff(&mut self, stale_cache: &Self) {
        self.diff_network_policies(&stale_cache.network_policies);
    }

    /// Computes the network policies that have been added, removed, or updated
    /// compared to a stale cache and saves that info in this cache
    ///
    /// # Arguments
    ///
    /// * `stale_cache` - The stale cache of network policy info we're comparing
    ///                   the cache to
    fn diff_network_policies(&mut self, stale_cache: &NetworkPolicyInfoCache) {
        // check each policy in the new cache
        for (policy_id, policy) in &self.network_policies.policies_by_id {
            // check if this is a new/updated policy based on the contents of the old cache
            match stale_cache.policies_by_id.get(policy_id) {
                Some(old_policy) => {
                    if policy.needs_k8s_update(old_policy) {
                        // this policy was updated, so we need to delete and re-add it to K8's
                        for group in &old_policy.groups {
                            raw_entry_vec_push!(
                                self.network_policies.policies_removed,
                                group,
                                old_policy.k8s_name.clone()
                            );
                        }
                        self.network_policies.policies_added.push(policy.id);
                    }
                }
                // this is a completely new policy, so we need to add it to K8's
                None => {
                    self.network_policies.policies_added.push(policy.id);
                }
            }
        }
        // check each policy in the old cache
        for (id, policy) in &stale_cache.policies_by_id {
            // check if this policy no longer exists based on the contents of the new cache
            if !self.network_policies.policies_by_id.contains_key(id) {
                // this policy no longer exists, so add it to be deleted from all of its groups
                for group in &policy.groups {
                    raw_entry_vec_push!(
                        self.network_policies.policies_removed,
                        group,
                        policy.k8s_name.clone()
                    );
                }
            }
        }
    }

    /// Get a copy of every cached workload entry
    #[must_use]
    pub fn workload_entries(&self) -> HashMap<Uuid, WorkloadEntry> {
        // copy the entries, which only clones the pointers to each workload
        self.workloads
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Get the cached workload of a tool instance this scaler schedules
    ///
    /// # Arguments
    ///
    /// * `tool_instance` - The tool instance to get the workload for
    #[must_use]
    pub fn get_workload(&self, tool_instance: &Uuid) -> Option<Arc<CachedWorkload>> {
        // get this tool instance's workload if we have it
        let workloads = self
            .workloads
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = workloads.get(tool_instance)?;
        // only workloads in our scaler's environment are ours to schedule
        if entry.workload.scaler() == self.scaler {
            Some(entry.workload.clone())
        } else {
            None
        }
    }

    /// Check if a tool instance's workload can be scheduled in a group by this scaler
    ///
    /// The workload must be cached and in this group, and a K8s workload's image must have been
    /// inspected.
    ///
    /// # Arguments
    ///
    /// * `tool_instance` - The tool instance to check
    /// * `group` - The group the worker would run in
    #[must_use]
    pub fn workload_ready(&self, tool_instance: &Uuid, group: &str) -> bool {
        // get this tool instance's workload if we have it
        let workloads = self
            .workloads
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(entry) = workloads.get(tool_instance) else {
            return false;
        };
        // the workload must be ours to schedule in this group
        if entry.workload.scaler() != self.scaler || entry.workload.projection(group).is_none() {
            return false;
        }
        // a K8s workload's image must have been inspected
        match entry.workload.image_ref() {
            Some(image) if self.scaler == ImageScaler::K8s => self.docker.contains_key(image),
            _ => true,
        }
    }

    /// Fetch the workloads of the tool instances in demand that aren't cached, without naming
    /// the groups they are in demand in
    ///
    /// # Arguments
    ///
    /// * `thorium` - A client for the Thorium API
    /// * `demand` - The tool instances in demand
    #[cfg(test)]
    pub async fn prefetch(&self, thorium: &Thorium, demand: HashSet<Uuid>) -> usize {
        // the tool instances are in demand without naming any group
        let demand = demand.into_iter().map(|id| (id, HashSet::new())).collect();
        self.prefetch_groups(thorium, demand).await
    }

    /// Fetch the workloads of the tool instances in demand that aren't cached
    ///
    /// Up to 10 specs are fetched at once. A tool instance that has no workload is remembered
    /// until the next reload, and a workload not in demand for one cache lifetime is dropped.
    /// A cached workload in demand in a group it has no projection for is fetched again, since
    /// its tool may have joined that group after it was cached; each tool instance is fetched
    /// again at most once per reload. Returns the number of specs fetched.
    ///
    /// # Arguments
    ///
    /// * `thorium` - A client for the Thorium API
    /// * `demand` - The tool instances in demand and the groups each is in demand in
    pub async fn prefetch_groups(
        &self,
        thorium: &Thorium,
        demand: HashMap<Uuid, HashSet<String>>,
    ) -> usize {
        // get the current timestamp
        let now = Utc::now();
        // get how long a workload not in demand is kept
        let lifetime = chrono::Duration::seconds(
            i64::try_from(self.conf.thorium.scaler.cache_lifetime).unwrap_or(i64::MAX),
        );
        // update when each cached workload was last in demand and find the uncached ones and
        // the cached ones to fetch again for a new group
        let fetches = {
            let mut workloads = self
                .workloads
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let missing = self
                .missing
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut refetched = self
                .refetched
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // mark the cached workloads in demand as seen
            for id in demand.keys() {
                if let Some(entry) = workloads.get_mut(id) {
                    entry.seen = now;
                }
            }
            // drop the workloads that haven't been in demand for a cache lifetime
            workloads.retain(|_, entry| now - entry.seen < lifetime);
            // get the tool instances we have no workload for yet, and the cached ones in demand
            // in a group they have no projection for that weren't fetched again this reload
            demand
                .into_iter()
                .filter(|(id, groups)| match workloads.get(id) {
                    Some(entry) => {
                        groups
                            .iter()
                            .any(|group| entry.workload.projection(group).is_none())
                            && refetched.insert(*id)
                    }
                    None => !missing.contains(id),
                })
                .map(|(id, _)| id)
                .collect::<Vec<Uuid>>()
        };
        // count the specs we are fetching
        let count = fetches.len();
        // fetch the uncached tool instances 10 at a time
        let mut results = stream::iter(fetches)
            .map(|id| async move { (id, thorium.entities.get(id).await) })
            .buffer_unordered(SPEC_FETCHES_IN_FLIGHT);
        // cache each fetched workload
        while let Some((id, result)) = results.next().await {
            self.insert_fetched(id, result, now);
        }
        count
    }

    /// Cache the result of fetching a tool instance's workload
    ///
    /// # Arguments
    ///
    /// * `id` - The tool instance that was fetched
    /// * `result` - The result of fetching it
    /// * `now` - When the tool instance was in demand
    fn insert_fetched(&self, id: Uuid, result: Result<Entity, Error>, now: DateTime<Utc>) {
        // build the workload of this tool instance
        let workload = match result {
            Ok(entity) => {
                // get this tool instance's runtime or the default
                let runtime = self
                    .runtimes
                    .runtimes
                    .get(&id)
                    .copied()
                    .unwrap_or(DEFAULT_WORKLOAD_RUNTIME);
                CachedWorkload::new(&entity, runtime)
            }
            // a tool instance that doesn't exist has no workload
            Err(error) if error.status() == Some(StatusCode::NOT_FOUND) => None,
            Err(error) => {
                // try again on the next tick
                event!(
                    Level::ERROR,
                    msg = "Failed to get tool instance",
                    tool_instance = id.to_string(),
                    error = error.msg(),
                );
                return;
            }
        };
        match workload {
            Some(workload) => {
                // check if this is a K8s workload whose image we haven't inspected
                let uninspected = self.scaler == ImageScaler::K8s
                    && workload.scaler() == ImageScaler::K8s
                    && workload
                        .image_ref()
                        .is_some_and(|image| !self.docker.contains_key(image));
                // cache this workload
                let entry = WorkloadEntry {
                    workload: Arc::new(workload),
                    seen: now,
                };
                let previous = self
                    .workloads
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(id, entry);
                // a new uninspected workload needs a reload; a refetched one keeps its image, so
                // the reload that cached it already tried to inspect it
                if uninspected && previous.is_none() {
                    self.reload_wanted.store(true, Ordering::Relaxed);
                }
            }
            None => {
                // remember this tool instance has no workload until the next reload
                self.missing
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(id);
            }
        }
    }

    /// Check whether a reload was requested to inspect a new workload's image
    #[must_use]
    pub fn reload_requested(&self) -> bool {
        self.reload_wanted.load(Ordering::Relaxed)
    }

    /// Clear a request for a reload
    pub fn clear_reload_request(&self) {
        self.reload_wanted.store(false, Ordering::Relaxed);
    }

    /// Keep the workloads a stale cache cached while this cache was being reloaded
    ///
    /// A K8s workload whose image this cache didn't inspect requests another reload.
    ///
    /// # Arguments
    ///
    /// * `stale` - The cache this cache replaces
    pub fn adopt_workloads(&mut self, stale: &Cache) {
        // get the stale cache's workloads
        let stale_entries = stale.workload_entries();
        // get our own workloads
        let workloads = self
            .workloads
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // add every workload we don't have
        let mut uninspected = false;
        for (id, entry) in stale_entries {
            if let hashbrown::hash_map::Entry::Vacant(vacant) = workloads.entry(id) {
                // check if this workload's image still needs to be inspected
                uninspected |= self.scaler == ImageScaler::K8s
                    && entry
                        .workload
                        .image_ref()
                        .is_some_and(|image| !self.docker.contains_key(image));
                vacant.insert(entry);
            }
        }
        // request a reload to inspect any uninspected images
        if uninspected {
            self.reload_wanted.store(true, Ordering::Relaxed);
        }
    }

    /// Return the base network policies defined in the Thorium config
    pub fn conf_base_network_policies(&self) -> &[BaseNetworkPolicy] {
        &self.conf.thorium.base_network_policies
    }

    /// Attempt to retrieve a network policy in the given group with the given name
    ///
    /// # Arguments
    ///
    /// * `group` - The group the network policy should be in
    /// * `name` - The name of the network policy
    pub fn get_network_policy(
        &self,
        group: &str,
        policy_name: &str,
    ) -> Result<&NetworkPolicy, Error> {
        // get the policy's id from our map
        let policy_id = self
            .network_policies
            .ids_by_group_name
            .get(group)
            .ok_or(Error::new(format!(
                "{group}:{policy_name} not in group level of network policy cache"
            )))?
            .get(policy_name)
            .ok_or(Error::new(format!(
                "{group}:{policy_name} not in network policy level of network policy cache"
            )))?;
        // return the policy itself or an error
        self.network_policies
            .policies_by_id
            .get(policy_id)
            .ok_or(Error::new(format!(
                "policy with id '{policy_id}' not in network policy cache"
            )))
    }

    /// Get the list of policies that should always be applied in the given group
    ///
    /// # Arguments
    ///
    /// * `group` - The group to get the policies for
    pub fn forced_network_policies(
        &self,
        group: &str,
    ) -> Result<Option<Vec<&NetworkPolicy>>, Error> {
        self.network_policies
            .forced_ids_by_group
            .get(group)
            .map(|ids| {
                ids.iter()
                    .map(|id| {
                        self.network_policies
                            .policies_by_id
                            .get(id)
                            .ok_or(Error::new(format!(
                                "forced network policy with id '{id}' not in network policy cache"
                            )))
                    })
                    .collect::<Vec<Result<&NetworkPolicy, Error>>>()
                    .into_iter()
                    .collect()
            })
            .transpose()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::libs::BanSets;
    use crate::libs::test_api::{
        self, GROUP, MockApi, MockCrane, World, deadline, k8s_spec, tool_instance,
    };
    use thorium::models::SpawnLimits;

    /// Build a world with K8s tool instances in our group
    ///
    /// # Arguments
    ///
    /// * `count` - The number of tool instances
    /// * `image` - Builds each tool instance's image from its index
    fn k8s_world(count: usize, image: impl Fn(usize) -> String) -> (World, Vec<Uuid>) {
        // build each tool instance
        let mut world = World::default();
        let ids = (0..count)
            .map(|i| Uuid::from_u128(1000 + i as u128))
            .collect::<Vec<Uuid>>();
        for (i, id) in ids.iter().enumerate() {
            let spec = k8s_spec(&image(i), SpawnLimits::Unlimited);
            let entity = tool_instance(*id, &format!("tool-{i}"), "1.0.0", &[GROUP], spec);
            world.entities.insert(*id, entity);
        }
        (world, ids)
    }

    /// The workloads of 25 uncached tool instances are each fetched once with at most 10 reads at
    /// once, and a second prefetch of the same demand fetches nothing
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn prefetch_fetches_each_workload_once() {
        // 25 tool instances whose reads are slow enough to overlap
        let (mut world, ids) = k8s_world(25, |i| format!("registry.example/tool-{i}:1"));
        world.entity_delay = std::time::Duration::from_millis(30);
        let api = MockApi::start(world).await;
        let crane = MockCrane::new();
        let cache = test_api::cache(&api, test_api::conf(&crane), ImageScaler::K8s, &["io"]).await;
        let thorium = api.client().await;
        // the first prefetch fetches every tool instance once
        let fetched = cache
            .prefetch(&thorium, ids.iter().copied().collect())
            .await;
        assert_eq!(fetched, 25);
        assert_eq!(api.count("GET", "/api/entities/"), 25);
        // at most 10 reads were in flight, and they did overlap
        let max = api.max_entity_reads.load(Ordering::SeqCst);
        assert!(max <= 10 && max > 1, "{max} reads were in flight at once");
        // every workload is cached and no image was inspected
        assert!(ids.iter().all(|id| cache.get_workload(id).is_some()));
        assert_eq!(crane.inspected(), Vec::<String>::new());
        // a second prefetch fetches nothing
        assert_eq!(
            cache
                .prefetch(&thorium, ids.iter().copied().collect())
                .await,
            0
        );
        assert_eq!(api.count("GET", "/api/entities/"), 25);
    }

    /// A tool instance that doesn't exist is fetched once, stays unknown until the next reload,
    /// and is fetched again after it
    #[tokio::test]
    async fn missing_workload_fetched_once_per_reload() {
        // an API that knows no tool instances
        let api = MockApi::start(World::default()).await;
        let crane = MockCrane::new();
        let cache = test_api::cache(&api, test_api::conf(&crane), ImageScaler::K8s, &["io"]).await;
        let thorium = api.client().await;
        let missing = Uuid::new_v4();
        // demand on two ticks reads the tool instance once
        for _ in 0..2 {
            cache
                .prefetch(&thorium, [missing].into_iter().collect())
                .await;
        }
        assert_eq!(api.count("GET", "/api/entities/"), 1);
        // its deadlines are filtered without a panic
        assert!(cache.get_workload(&missing).is_none());
        assert!(!cache.workload_ready(&missing, GROUP));
        // a reload forgets that it was missing so it is read again
        let TaskResult::Cache(reloaded) =
            Cache::refresh(Arc::new(cache), Arc::new(thorium.clone()), ImageScaler::K8s)
                .await
                .unwrap()
        else {
            panic!("a refresh returns a cache");
        };
        reloaded
            .prefetch(&thorium, [missing].into_iter().collect())
            .await;
        assert_eq!(api.count("GET", "/api/entities/"), 2);
    }

    /// A workload not in demand for a cache lifetime is dropped and fetched again when a later
    /// deadline names it
    #[tokio::test]
    async fn idle_workloads_are_dropped() {
        // one tool instance
        let (world, ids) = k8s_world(1, |_| "registry.example/idle:1".to_owned());
        let api = MockApi::start(world).await;
        let crane = MockCrane::new();
        let cache = test_api::cache(&api, test_api::conf(&crane), ImageScaler::K8s, &["io"]).await;
        let thorium = api.client().await;
        cache
            .prefetch(&thorium, ids.iter().copied().collect())
            .await;
        // age the workload past one cache lifetime
        let lifetime = i64::try_from(cache.conf.thorium.scaler.cache_lifetime).unwrap();
        cache
            .workloads
            .write()
            .unwrap()
            .get_mut(&ids[0])
            .unwrap()
            .seen = Utc::now() - chrono::Duration::seconds(lifetime + 1);
        // a tick without it in demand drops it
        cache.prefetch(&thorium, HashSet::new()).await;
        assert!(cache.get_workload(&ids[0]).is_none());
        // a later deadline fetches it again
        cache
            .prefetch(&thorium, ids.iter().copied().collect())
            .await;
        assert!(cache.get_workload(&ids[0]).is_some());
        assert_eq!(api.count("GET", "/api/entities/"), 2);
    }

    /// A reload keeps every cached workload without fetching its spec again, inspects each K8s
    /// workload's image keyed by its reference, and projects each workload with its reloaded
    /// runtime; images that fail to inspect are left out
    #[tokio::test]
    async fn reload_keeps_workloads_and_inspects_images() {
        // three tool instances, one of which has an image that can't be inspected
        let (world, ids) = k8s_world(3, |i| {
            if i == 2 {
                "registry.example/broken:1".to_owned()
            } else {
                format!("registry.example/tool-{i}:1")
            }
        });
        let api = MockApi::start(world).await;
        let crane = MockCrane::new();
        let cache = test_api::cache(&api, test_api::conf(&crane), ImageScaler::K8s, &["io"]).await;
        let thorium = Arc::new(api.client().await);
        cache
            .prefetch(&thorium, ids.iter().copied().collect())
            .await;
        // nothing is inspected or ready before a reload, and the default runtime is used
        assert_eq!(crane.inspected(), Vec::<String>::new());
        assert!(!cache.workload_ready(&ids[0], GROUP));
        let projection = cache.get_workload(&ids[0]).unwrap();
        assert!((projection.projection(GROUP).unwrap().runtime - 600.0).abs() < f64::EPSILON);
        // the new instance's runtime is recorded before the reload
        api.world
            .lock()
            .unwrap()
            .runtimes
            .runtimes
            .insert(ids[0], 42.0);
        api.clear();
        let TaskResult::Cache(reloaded) =
            Cache::refresh(Arc::new(cache), thorium.clone(), ImageScaler::K8s)
                .await
                .unwrap()
        else {
            panic!("a refresh returns a cache");
        };
        // no spec was fetched again and the runtimes were read once
        assert_eq!(api.count("GET", "/api/entities/"), 0);
        assert_eq!(api.count("GET", "/api/jobs/workloads/runtimes"), 1);
        assert_eq!(api.count("GET", "/api/images"), 0);
        // each image was inspected once and is keyed by its reference
        assert_eq!(crane.inspected().len(), 3);
        assert!(reloaded.docker.contains_key("registry.example/tool-0:1"));
        assert!(!reloaded.docker.contains_key("registry.example/broken:1"));
        // inspected workloads are ready and the broken one stays filtered
        assert!(reloaded.workload_ready(&ids[0], GROUP));
        assert!(reloaded.workload_ready(&ids[1], GROUP));
        assert!(!reloaded.workload_ready(&ids[2], GROUP));
        // projections carry the reloaded runtime, or the default for unrecorded instances
        let first = reloaded.get_workload(&ids[0]).unwrap();
        assert!((first.projection(GROUP).unwrap().runtime - 42.0).abs() < f64::EPSILON);
        let second = reloaded.get_workload(&ids[1]).unwrap();
        assert!((second.projection(GROUP).unwrap().runtime - 600.0).abs() < f64::EPSILON);
    }

    /// A group added to a tool instance while it stays in demand is seen after one more read of
    /// that tool instance, and that read happens at most once per reload
    #[tokio::test]
    async fn demand_in_a_new_group_refetches_once_per_reload() {
        // one K8s tool instance in our group
        let (world, ids) = k8s_world(1, |_| "registry.example/widened:1".to_owned());
        let api = MockApi::start(world).await;
        let crane = MockCrane::new();
        let mut cache = Arc::new(
            test_api::cache(&api, test_api::conf(&crane), ImageScaler::K8s, &["io"]).await,
        );
        let thorium = Arc::new(api.client().await);
        cache
            .prefetch_groups(&thorium, [(ids[0], [GROUP.to_owned()].into())].into())
            .await;
        assert_eq!(api.count("GET", "/api/entities/"), 1);
        // the tool instance is added to another group in the API
        api.world
            .lock()
            .unwrap()
            .entities
            .get_mut(&ids[0])
            .unwrap()
            .groups
            .push("vesta".to_owned());
        // demand in both groups across reloads and ticks
        let demand: HashMap<Uuid, HashSet<String>> =
            [(ids[0], [GROUP.to_owned(), "vesta".to_owned()].into())].into();
        for _ in 0..3 {
            let TaskResult::Cache(reloaded) =
                Cache::refresh(cache.clone(), thorium.clone(), ImageScaler::K8s)
                    .await
                    .unwrap()
            else {
                panic!("a refresh returns a cache");
            };
            cache = Arc::new(reloaded);
            for _ in 0..2 {
                cache.prefetch_groups(&thorium, demand.clone()).await;
            }
        }
        // the new group is ready and the tool instance was read again only once
        let workload = cache.get_workload(&ids[0]).unwrap();
        assert_eq!(workload.groups, vec![GROUP.to_owned(), "vesta".to_owned()]);
        assert!(cache.workload_ready(&ids[0], GROUP));
        assert!(cache.workload_ready(&ids[0], "vesta"));
        assert_eq!(api.count("GET", "/api/entities/"), 2);
        // demand in a group the tool instance never joins reads it once per reload
        let ghost: HashMap<Uuid, HashSet<String>> = [(ids[0], ["ghost".to_owned()].into())].into();
        for _ in 0..2 {
            let TaskResult::Cache(reloaded) =
                Cache::refresh(cache.clone(), thorium.clone(), ImageScaler::K8s)
                    .await
                    .unwrap()
            else {
                panic!("a refresh returns a cache");
            };
            cache = Arc::new(reloaded);
            for _ in 0..3 {
                cache.prefetch_groups(&thorium, ghost.clone()).await;
            }
        }
        assert_eq!(api.count("GET", "/api/entities/"), 4);
        assert!(!cache.workload_ready(&ids[0], "ghost"));
    }

    /// A cached K8s workload whose image can't be inspected requests an early reload only when
    /// it is first fetched, not each time demand in a new group fetches it again
    #[tokio::test]
    async fn refetching_an_uninspectable_image_requests_no_reload() {
        // one K8s tool instance whose image fails inspection
        let (world, ids) = k8s_world(1, |_| "registry.example/broken:1".to_owned());
        let api = MockApi::start(world).await;
        let crane = MockCrane::new();
        let mut cache = Arc::new(
            test_api::cache(&api, test_api::conf(&crane), ImageScaler::K8s, &["io"]).await,
        );
        let thorium = Arc::new(api.client().await);
        // the first fetch of the new workload requests a reload
        cache
            .prefetch_groups(&thorium, [(ids[0], [GROUP.to_owned()].into())].into())
            .await;
        let mut requested = vec![cache.reload_requested()];
        // demand in a group the tool instance never joins fetches it again after each reload
        let ghost: HashMap<Uuid, HashSet<String>> =
            [(ids[0], [GROUP.to_owned(), "ghost".to_owned()].into())].into();
        for _ in 0..3 {
            let TaskResult::Cache(mut reloaded) =
                Cache::refresh(cache.clone(), thorium.clone(), ImageScaler::K8s)
                    .await
                    .unwrap()
            else {
                panic!("a refresh returns a cache");
            };
            reloaded.adopt_workloads(&cache);
            cache = Arc::new(reloaded);
            cache.prefetch_groups(&thorium, ghost.clone()).await;
            requested.push(cache.reload_requested());
        }
        // each reload read the tool instance again but only the first fetch requested a reload
        assert_eq!(api.count("GET", "/api/entities/"), 4);
        assert_eq!(requested, vec![true, false, false, false]);
        assert!(!cache.workload_ready(&ids[0], GROUP));
    }

    /// A prefetch that adds a K8s workload whose image isn't inspected requests an early reload,
    /// and a workload added to the old cache while a reload runs is kept by the new cache
    #[tokio::test]
    async fn new_k8s_images_request_a_reload() {
        // two tool instances
        let (world, ids) = k8s_world(2, |i| format!("registry.example/tool-{i}:1"));
        let api = MockApi::start(world).await;
        let crane = MockCrane::new();
        let stale = Arc::new(
            test_api::cache(&api, test_api::conf(&crane), ImageScaler::K8s, &["io"]).await,
        );
        let thorium = Arc::new(api.client().await);
        // prefetching a new K8s workload requests a reload
        assert!(!stale.reload_requested());
        stale
            .prefetch(&thorium, [ids[0]].into_iter().collect())
            .await;
        assert!(stale.reload_requested());
        // a reload copies the cached workloads and inspects their images
        let TaskResult::Cache(mut reloaded) =
            Cache::refresh(stale.clone(), thorium.clone(), ImageScaler::K8s)
                .await
                .unwrap()
        else {
            panic!("a refresh returns a cache");
        };
        // the old cache adds another workload before the new cache replaces it
        stale
            .prefetch(&thorium, [ids[1]].into_iter().collect())
            .await;
        reloaded.adopt_workloads(&stale);
        // the new cache has both, inspected only the first, and requests a reload for the second
        assert!(reloaded.get_workload(&ids[0]).is_some());
        assert!(reloaded.get_workload(&ids[1]).is_some());
        assert!(reloaded.workload_ready(&ids[0], GROUP));
        assert!(!reloaded.workload_ready(&ids[1], GROUP));
        assert!(reloaded.reload_requested());
    }

    /// Workloads only schedule on the scaler of their environment, and a deadline in a group the
    /// tool instance isn't in is never ready
    #[tokio::test]
    async fn workloads_schedule_only_on_their_scaler() {
        // a K8s tool instance
        let (world, ids) = k8s_world(1, |_| "registry.example/k8s-only:1".to_owned());
        let api = MockApi::start(world).await;
        let crane = MockCrane::new();
        let cache = test_api::cache(
            &api,
            test_api::conf(&crane),
            ImageScaler::BareMetal,
            &["io"],
        )
        .await;
        let thorium = api.client().await;
        cache
            .prefetch(&thorium, ids.iter().copied().collect())
            .await;
        // a bare metal scaler caches it but never schedules it
        assert!(cache.get_workload(&ids[0]).is_none());
        assert!(!cache.workload_ready(&ids[0], GROUP));
        // and a deadline for it is filtered
        let dl = deadline("io", GROUP, ids[0], None, 5);
        assert!(!BanSets::new().filter_deadlines(&cache, &dl));
        assert!(!cache.workload_ready(&ids[0], "vesta"));
    }

    /// A token that expires within the next week is refreshed early, so a token expiring in
    /// three days triggers a refresh now rather than after it has already expired.
    #[test]
    fn refreshes_token_a_week_before_expiry() {
        // the current time
        let now = Utc::now();
        // a token that will expire in three days is inside the one-week early-refresh window
        let expires = Some(now + chrono::Duration::days(3));
        // a token expiring within the next week must be refreshed early
        assert!(
            should_refresh_client(expires, now),
            "a token expiring within the next week must be refreshed early"
        );
    }

    /// A token that is not close to expiring is not refreshed, so the early-refresh window does
    /// not turn into refreshing on every single call.
    #[test]
    fn does_not_refresh_token_far_from_expiry() {
        // the current time
        let now = Utc::now();
        // a token that expires in a month is well outside the one-week early-refresh window
        let expires = Some(now + chrono::Duration::days(30));
        // a token with plenty of life left must not be refreshed yet
        assert!(
            !should_refresh_client(expires, now),
            "a token expiring far in the future must not be refreshed early"
        );
    }
}
