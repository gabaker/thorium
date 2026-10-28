use chrono::prelude::*;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::{Display, Formatter};
use std::ops::Add;
use std::path::PathBuf;
use uuid::Uuid;

use crate::models::conversions;
use crate::{Conf, matches_adds, matches_removes, matches_update};

use super::{
    Group, GroupStats, Image, ImageScaler, InvalidEnum, ModelProvider, ModelProviderKind, Pipeline,
    Requisition, Resources, ScopedToken, User,
};

/// The default IFF to use when initializing Thorium
pub const DEFAULT_IFF: &str = "Thorium";
/// The Redis key that signals whether the K8's cache needs to be updated
pub const K8S_CACHE_KEY: &str = "k8s_cache";
/// The Redis key that signals whether the bare metal cache needs to be updated
pub const BARE_METAL_CACHE_KEY: &str = "bare_metal_cache";
/// The Redis key that signals whether the Windows cache needs to be updated
pub const WINDOWS_CACHE_KEY: &str = "windows_cache";
/// The Redis key that signals whether the KVM cache needs to be updated
pub const KVM_CACHE_KEY: &str = "kvm_cache";
/// The Redis key that signals whether the external cache needs to be updated
pub const EXTERNAL_CACHE_KEY: &str = "external_cache";

/// The query params for getting system info
#[derive(Deserialize, Serialize, Debug)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "api", derive(utoipa::IntoParams))]
#[cfg_attr(feature = "api", into_params(parameter_in = Query))]
pub struct SystemInfoParams {
    /// The scaler to reset any system info flags for
    pub reset: Option<ImageScaler>,
}

/// Info about data in the current backend
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct SystemInfo {
    /// Whether the k8s scaler cache needs to be updated
    pub k8s_cache: bool,
    /// Whether the bare_metal scaler cache needs to be updated
    pub bare_metal_cache: bool,
    /// Whether the windows scaler cache needs to be updated
    pub windows_cache: bool,
    /// Whether this kvm scaler cache needs to be updated
    pub kvm_cache: bool,
    /// Whether the external scaler cache needs to be updated
    pub external_cache: bool,
    /// The Thorium namespace this API serves (its `thorium.namespace`); `None` from an API that
    /// doesn't report it
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

impl SystemInfo {
    /// Check whether our scalers cache needs to be updated
    #[must_use]
    pub fn expired_cache(&self, scaler: ImageScaler) -> bool {
        match scaler {
            ImageScaler::K8s => self.k8s_cache,
            ImageScaler::BareMetal => self.bare_metal_cache,
            ImageScaler::Windows => self.windows_cache,
            ImageScaler::Kvm => self.kvm_cache,
            ImageScaler::External => self.external_cache,
        }
    }
}

/// The number of jobs running and in the deadline queue for each scaler
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ScalerStats {
    /// The number of jobs curerntin the deadline queue
    pub deadlines: i64,
    /// The number of jobs currently running
    pub running: i64,
}

impl ScalerStats {
    #[must_use]
    pub fn new(deadlines: i64, running: i64) -> Self {
        ScalerStats { deadlines, running }
    }
}

/// A map of spawned requisitions
pub type SpawnMap<'a> = HashMap<&'a String, BTreeMap<u64, Vec<(Requisition, u64)>>>;

/// Statistics about the current state of Thorium
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct SystemStats {
    /// The total number of deadlines currently in the sytem across all scalers
    pub deadlines: i64,
    /// The total number of running jobs currently in the sytem across all scalers
    pub running: i64,
    /// The number of users currently in the sytem
    pub users: i64,
    /// The stats for jobs under the k8s scaler
    pub k8s: ScalerStats,
    /// The stats for jobs under the baremetal scaler
    pub baremetal: ScalerStats,
    /// The stats for jobs under the external scaler
    pub external: ScalerStats,
    /// The stats for jobs under the windows scaler
    #[serde(default)]
    pub windows: ScalerStats,
    /// The stats for jobs under the kvm scaler
    #[serde(default)]
    pub kvm: ScalerStats,
    /// Detailed stats reports for each group
    pub groups: HashMap<String, GroupStats>,
}

impl SystemStats {
    /// gets a total count for the number of stages in use by all groups, pipelines and users
    #[must_use]
    pub fn total(&self) -> usize {
        // add all the number of stage up for each group, user and pipeline
        self.groups.values().map(|map| map.total()).sum()
    }

    /// Builds a vector of unique names for each of the group/tool instance/user combos
    #[must_use]
    pub fn unique(&self) -> HashSet<String> {
        // build an empty list to store our unique names
        let mut unique = HashSet::with_capacity(self.groups.len() * 10);
        // crawl over each group
        for (group, stats) in &self.groups {
            // crawl over each tool instance and its user map
            for (tool_instance, user_map) in &stats.workloads {
                // crawl over each user with jobs for this tool instance
                for user in user_map.keys() {
                    unique.insert(format!("{group}:{tool_instance}:{user}"));
                }
            }
        }
        unique
    }

    /// Gets each users shared workload requisitions ranked by their running jobs
    ///
    /// Each requisition is paired with its number of created jobs. Jobs of runs are never in the
    /// workload stats, so no requisition names a run.
    #[must_use]
    pub fn users_jobs(&self) -> SpawnMap<'_> {
        // build a map of all users jobs
        let mut map: SpawnMap = HashMap::default();
        // crawl over each group
        for (group, stats) in &self.groups {
            // crawl over each tool instance and its users
            for (tool_instance, user_map) in &stats.workloads {
                // add each user's jobs for this tool instance to our map
                for (user, stage_stats) in user_map {
                    // get an entry to this users map
                    let user_entry = map.entry(user).or_default();
                    // build a shared requisition for this tool instance
                    let req = Requisition::new(user, group, *tool_instance, None);
                    // add it to the rank group of its running jobs
                    let entry = user_entry.entry(stage_stats.running).or_default();
                    entry.push((req, stage_stats.created));
                }
            }
        }
        map
    }
}

// TODO: remove once serde allows for default values or we move this to a helper function
/// Helps serde default a value to true
const fn default_true() -> bool {
    true
}

/// The params for resetting system settings
#[derive(Serialize, Deserialize, Debug)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "api", derive(utoipa::IntoParams))]
#[cfg_attr(feature = "api", into_params(parameter_in = Query))]
pub struct SystemSettingsResetParams {
    /// Whether to run the consistency scan after resetting system settings
    #[serde(default = "default_true")]
    pub scan: bool,
}

impl Default for SystemSettingsResetParams {
    /// Provide default `SystemSettingsResetParams`
    ///
    /// The consistency scan is run by default
    fn default() -> Self {
        Self { scan: true }
    }
}

impl SystemSettingsResetParams {
    /// Skip the consistency scan after resetting system settings
    #[must_use]
    pub fn no_scan(mut self) -> Self {
        self.scan = false;
        self
    }
}

/// The params for updating system settings
#[derive(Serialize, Deserialize, Debug)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "api", derive(utoipa::IntoParams))]
#[cfg_attr(feature = "api", into_params(parameter_in = Query))]
pub struct SystemSettingsUpdateParams {
    /// Whether to run the consistency scan after updating system settings
    #[serde(default = "default_true")]
    pub scan: bool,
}

impl Default for SystemSettingsUpdateParams {
    /// Provide default `SystemSettingsUpdateParams`
    ///
    /// The consistency scan is run by default
    fn default() -> Self {
        Self { scan: true }
    }
}

impl SystemSettingsUpdateParams {
    /// Skip the consistency scan after updating system settings
    #[must_use]
    pub fn no_scan(mut self) -> Self {
        self.scan = false;
        self
    }
}

/// An update to the host path whitelist in Thorium [`SystemSettings`]
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct HostPathWhitelistUpdate {
    /// The set of paths to add to the whitelist
    #[serde(default)]
    #[cfg_attr(feature = "api", schema(value_type = HashSet<String>))]
    pub add_paths: HashSet<PathBuf>,
    /// The set of paths to remove from the whitelist
    #[serde(default)]
    #[cfg_attr(feature = "api", schema(value_type = HashSet<String>))]
    pub remove_paths: HashSet<PathBuf>,
}

impl HostPathWhitelistUpdate {
    /// Add a path to add to to the host path whitelist
    ///
    /// # Arguments
    ///
    /// * `paths` - The paths to add
    #[must_use]
    pub fn add_path<T: Into<PathBuf>>(mut self, path: T) -> Self {
        self.add_paths.insert(path.into());
        self
    }

    /// Add multiple paths to add to the host path whitelist
    ///
    /// # Arguments
    ///
    /// * `path` - The path to add
    #[must_use]
    pub fn add_paths<I, T>(mut self, paths: I) -> Self
    where
        T: Into<PathBuf>,
        I: IntoIterator<Item = T>,
    {
        self.add_paths.extend(paths.into_iter().map(Into::into));
        self
    }

    /// Remove a path to add to to the host path whitelist
    ///
    /// # Arguments
    ///
    /// * `path` - The path to remove
    #[must_use]
    pub fn remove_path<T: Into<PathBuf>>(mut self, path: T) -> Self {
        self.remove_paths.insert(path.into());
        self
    }

    /// Add multiple paths to remove from the host path whitelist
    ///
    /// # Arguments
    ///
    /// * `paths` - The paths to remove
    #[must_use]
    pub fn remove_paths<I, T>(mut self, paths: I) -> Self
    where
        T: Into<PathBuf>,
        I: IntoIterator<Item = T>,
    {
        self.remove_paths.extend(paths.into_iter().map(Into::into));
        self
    }
}

/// An update to Thorium's dynamic [`SystemSettings`]
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct SystemSettingsUpdate {
    /// The amount of millicpu to reserve for things outside of Thorium
    pub reserved_cpu: Option<String>,
    /// The amount of memory to reserve for things outside of Thorium
    pub reserved_memory: Option<String>,
    /// The amount of ephemeral storage to reserve for things outside of Thorium
    pub reserved_storage: Option<String>,
    /// The amount of millicpu to use in the fairshare pass if possible
    pub fairshare_cpu: Option<String>,
    /// The amount of memory to use in the fairshare pass if possible
    pub fairshare_memory: Option<String>,
    /// The amount of ephemeral storage to use in the fairshare pass if possible
    pub fairshare_storage: Option<String>,
    /// An update to the host path whitelist
    #[serde(default)]
    pub host_path_whitelist: HostPathWhitelistUpdate,
    #[serde(default)]
    pub clear_host_path_whitelist: bool,
    /// Allow users to create any host path
    pub allow_unrestricted_host_paths: Option<bool>,
    /// An update to the allowed model endpoints
    #[serde(default)]
    pub models: ModelsConfUpdate,
}

impl SystemSettingsUpdate {
    /// Sets the amount of reserved CPU in millicpu or cores that cannot be used by Thorium
    ///
    /// If you place an m at the end it will be in millicpu while no unit will be treated as whole
    /// cores.
    ///
    /// # Arguments
    ///
    /// * `cpu` - The amount of cpu to reserve
    #[must_use]
    pub fn reserved_cpu<T: Into<String>>(mut self, cpu: T) -> Self {
        self.reserved_cpu = Some(cpu.into());
        self
    }

    /// Sets the amount of reserved memory that cannot be used by Thorium
    ///
    /// If no unit is specified then bytes are assumed.
    ///
    /// # Arguments
    ///
    /// * `memory` - The amount of memory to reserve
    #[must_use]
    pub fn reserved_memory<T: Into<String>>(mut self, memory: T) -> Self {
        self.reserved_memory = Some(memory.into());
        self
    }

    /// Sets the amount of reserved ephemeral storage that cannot be used by Thorium
    ///
    /// If no unit is specified then bytes are assumed.
    ///
    /// # Arguments
    ///
    /// * `reserved_storage` - The amount of ephemeral storage to reserve
    #[must_use]
    pub fn reserved_storage<T: Into<String>>(mut self, storage: T) -> Self {
        self.reserved_storage = Some(storage.into());
        self
    }

    /// Sets the amount of CPU in millicpu or cores that will be fairly shared if possible
    ///
    /// If you place an m at the end it will be in millicpu while no unit will be treated as whole
    /// cores.
    ///
    /// # Arguments
    ///
    /// * `cpu` - The amount of cpu to use for fair share
    #[must_use]
    pub fn fairshare_cpu<T: Into<String>>(mut self, cpu: T) -> Self {
        self.fairshare_cpu = Some(cpu.into());
        self
    }

    /// Sets the amount of memory that will be fairly shared if possible
    ///
    /// If no unit is specified then bytes are assumed.
    ///
    /// # Arguments
    ///
    /// * `memory` - The amount of memory to use for fair share
    #[must_use]
    pub fn fairshare_memory<T: Into<String>>(mut self, memory: T) -> Self {
        self.fairshare_memory = Some(memory.into());
        self
    }

    /// Sets the amount of ephemeral storage that will be fairly shared if possible
    ///
    /// If no unit is specified then bytes are assumed.
    ///
    /// # Arguments
    ///
    /// * `storage` - The amount of ephemeral storage to use for fair share
    #[must_use]
    pub fn fairshare_storage<T: Into<String>>(mut self, storage: T) -> Self {
        self.fairshare_storage = Some(storage.into());
        self
    }

    /// Sets a new host path whitelist to replace existing one
    ///
    /// # Arguments
    ///
    /// * `host_path_whitelist` - The new whitelist to set
    #[must_use]
    pub fn host_path_whitelist(mut self, host_path_whitelist: HostPathWhitelistUpdate) -> Self {
        self.host_path_whitelist = host_path_whitelist;
        self
    }

    /// Clear the host path whitelist
    ///
    /// Overrides any other add/remove settings
    #[must_use]
    pub fn clear_host_path_whitelist(mut self) -> Self {
        self.clear_host_path_whitelist = true;
        self
    }

    /// Set whether to allow all host paths, ignoring the host path whitelist
    ///
    /// # Arguments
    ///
    /// * `value` - The value to set
    #[must_use]
    pub fn allow_unrestricted_host_paths(mut self, value: bool) -> Self {
        self.allow_unrestricted_host_paths = Some(value);
        self
    }

    /// Set the update to the allowed model endpoints
    ///
    /// # Arguments
    ///
    /// * `models` - The update to the allowed model endpoints
    #[must_use]
    pub fn models(mut self, models: ModelsConfUpdate) -> Self {
        self.models = models;
        self
    }
}

/// Get the form whitelist entries and endpoint urls are compared in
///
/// A url and the same url with one trailing `/` are the same address, and nothing else is
/// normalized, so a url ending in a single `/` loses it and a url ending in more than one keeps
/// them all. Applying this more than once changes nothing.
///
/// # Arguments
///
/// * `url` - The url to normalize
fn canonical_url(url: &str) -> &str {
    // drop a single trailing slash, leaving a url ending in more than one alone
    match url.strip_suffix('/') {
        Some(rest) if !rest.ends_with('/') => rest,
        _ => url,
    }
}

/// The model endpoints users and groups may configure (admin managed)
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ModelsConf {
    /// The allowed endpoints (empty = none can be configured)
    #[serde(default)]
    pub allowed_endpoints: BTreeSet<AllowedEndpoint>,
}

impl ModelsConf {
    /// Whether an endpoint's provider and url are on the allowed list
    ///
    /// # Arguments
    ///
    /// * `provider` - The endpoint's provider and connection settings
    #[must_use]
    pub fn allows(&self, provider: &ModelProvider) -> bool {
        // some entry must match the provider and its url exactly
        self.allowed_endpoints
            .iter()
            .any(|entry| entry.matches(provider))
    }

    /// Make sure an endpoint is on the allowed list, naming it and its provider's entries if not
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the endpoint, for the refusal
    /// * `provider` - The endpoint's provider and connection settings
    pub fn check(&self, name: &str, provider: &ModelProvider) -> Result<(), String> {
        // an allowed endpoint passes
        if self.allows(provider) {
            return Ok(());
        }
        // list the entries for this provider
        let kind = provider.kind();
        let entries = self
            .allowed_endpoints
            .iter()
            .filter(|entry| entry.provider == kind)
            .map(|entry| match &entry.base_url {
                Some(url) => format!("`{url}`"),
                None => "its default url".to_owned(),
            })
            .collect::<Vec<String>>();
        let entries = if entries.is_empty() {
            "none".to_owned()
        } else {
            entries.join(", ")
        };
        let url = match provider.configured_url() {
            Some(url) => format!("`{url}`"),
            None => "its default url".to_owned(),
        };
        Err(format!(
            "model endpoint `{name}` ({kind} at {url}) isn't on the deployment's allowed endpoint list; the allowed {kind} endpoints are: {entries}"
        ))
    }
}

/// One allowed model endpoint
#[derive(Serialize, Deserialize, Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct AllowedEndpoint {
    /// The provider
    pub provider: ModelProviderKind,
    /// The exact base url allowed (unset = only the provider's default public url; for `Bedrock`
    /// and `Vertex` the regional default urls)
    pub base_url: Option<String>,
}

impl AllowedEndpoint {
    /// Create an allowed endpoint
    ///
    /// # Arguments
    ///
    /// * `provider` - The provider
    /// * `base_url` - The exact base url allowed, or `None` for the provider's default urls
    #[must_use]
    pub fn new(provider: ModelProviderKind, base_url: Option<String>) -> Self {
        AllowedEndpoint { provider, base_url }.normalized()
    }

    /// Get this entry with its url in the form urls are compared in
    ///
    /// Urls match apart from one trailing slash, so entries are kept in this form and one
    /// spelling of a url always finds the entry of the other.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        // drop a single trailing slash from the url
        if let Some(url) = &mut self.base_url {
            let trimmed = canonical_url(url).len();
            url.truncate(trimmed);
        }
        self
    }

    /// Create the allowed endpoint that matches a provider's configured url
    ///
    /// # Arguments
    ///
    /// * `provider` - The provider to allow
    #[must_use]
    pub fn for_provider(provider: &ModelProvider) -> Self {
        // allow the url the provider is configured with, or its default urls
        let base_url = provider.configured_url().map(ToOwned::to_owned);
        AllowedEndpoint::new(provider.kind(), base_url)
    }

    /// Whether this entry allows a provider
    ///
    /// # Arguments
    ///
    /// * `provider` - The provider to check
    #[must_use]
    pub fn matches(&self, provider: &ModelProvider) -> bool {
        // the provider must be the same
        if self.provider != provider.kind() {
            return false;
        }
        // the urls must match apart from one trailing slash, or both use the default
        match (&self.base_url, provider.configured_url()) {
            (None, None) => true,
            (Some(allowed), Some(url)) => canonical_url(allowed) == canonical_url(url),
            _ => false,
        }
    }
}

/// An update to the allowed model endpoints
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ModelsConfUpdate {
    /// The endpoints to allow
    #[serde(default)]
    pub add_endpoints: BTreeSet<AllowedEndpoint>,
    /// The endpoints to stop allowing
    #[serde(default)]
    pub remove_endpoints: BTreeSet<AllowedEndpoint>,
    /// Remove every allowed endpoint
    #[serde(default)]
    pub clear_endpoints: bool,
}

impl ModelsConfUpdate {
    /// Allow an endpoint
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The endpoint to allow
    #[must_use]
    pub fn add_endpoint(mut self, endpoint: AllowedEndpoint) -> Self {
        self.add_endpoints.insert(endpoint);
        self
    }

    /// Stop allowing an endpoint
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The endpoint to stop allowing
    #[must_use]
    pub fn remove_endpoint(mut self, endpoint: AllowedEndpoint) -> Self {
        self.remove_endpoints.insert(endpoint);
        self
    }

    /// Remove every allowed endpoint before any are added
    #[must_use]
    pub fn clear(mut self) -> Self {
        self.clear_endpoints = true;
        self
    }
}

/// Settings that can be dynamically changed in Thorium
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct SystemSettings {
    /// The amount of millicpu to reserve for things outside of Thorium
    pub reserved_cpu: u64,
    /// The amount of memory to reserve for things outside of Thorium
    pub reserved_memory: u64,
    /// The amount of ephemeral storage to reserve for things outside of Thorium
    pub reserved_storage: u64,
    /// The amount of millicpu to use in the fairshare pass if possible
    pub fairshare_cpu: u64,
    /// The amount of memory to use in the fairshare pass if possible
    pub fairshare_memory: u64,
    /// The amount of ephemeral storage to use in the fairshare pass if possible
    pub fairshare_storage: u64,
    /// A whitelist of host paths users can mount in their tools with no admin intervention
    ///
    /// Users can mount to any of the paths in the whitelist or to a path whose parent is
    /// in the whitelist.
    #[serde(default)]
    #[cfg_attr(feature = "api", schema(value_type = HashSet<String>))]
    pub host_path_whitelist: HashSet<PathBuf>,
    /// Allow users to create any host path, ignoring the whitelist; defaults to false
    #[serde(default)]
    pub allow_unrestricted_host_paths: bool,
    /// The model endpoints users and groups may configure
    #[serde(default)]
    pub models: ModelsConf,
}

impl PartialEq<SystemSettingsUpdate> for SystemSettings {
    /// Ensure all updates in a [`SystemSettingsUpdate`] were set
    ///
    /// # Arguments
    ///
    /// * `update` - The update to compare against
    #[rustfmt::skip]
    fn eq(&self, update: &SystemSettingsUpdate) -> bool {
        matches_update!(self.reserved_cpu, update.reserved_cpu, conversions::cpu);
        matches_update!(self.reserved_memory, update.reserved_memory, conversions::storage);
        matches_update!(self.reserved_storage, update.reserved_storage, conversions::storage);
        matches_update!(self.fairshare_cpu, update.fairshare_cpu, conversions::cpu);
        matches_update!(self.fairshare_memory, update.fairshare_memory, conversions::storage);
        matches_update!(self.fairshare_storage, update.fairshare_storage, conversions::storage);
        matches_adds!(self.host_path_whitelist, update.host_path_whitelist.add_paths);
        matches_removes!(self.host_path_whitelist, update.host_path_whitelist.remove_paths);
        matches_update!(self.allow_unrestricted_host_paths, update.allow_unrestricted_host_paths);
        // compare the entries an update names the way they're kept
        let models = &update.models;
        let adds = models.add_endpoints.iter().cloned().map(AllowedEndpoint::normalized).collect::<Vec<_>>();
        let removes = models.remove_endpoints.iter().cloned().map(AllowedEndpoint::normalized).collect::<Vec<_>>();
        // the endpoints an update leaves allowed are allowed and the ones it removes aren't
        let allowed = &self.models.allowed_endpoints;
        let mut kept = adds.iter().filter(|entry| !removes.contains(*entry));
        if !kept.all(|entry| allowed.contains(entry)) {
            return false;
        }
        matches_removes!(allowed, removes);
        // a cleared list holds only what the update added
        if models.clear_endpoints && allowed.iter().any(|entry| !adds.contains(entry)) {
            return false;
        }
        true
    }
}

/// A struct containing a full backup of users/groups/images/pipelines of the server
#[derive(Serialize, Deserialize, Debug)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct Backup {
    /// The system settings
    pub settings: SystemSettings,
    /// The users for this server
    pub users: Vec<User>,
    /// The groups on this server
    pub groups: Vec<Group>,
    /// The images on this server
    pub images: Vec<Image>,
    /// The pipelines on this server
    pub pipelines: Vec<Pipeline>,
    /// Every user's scoped tokens; empty in backups taken before scoped tokens were backed up
    #[serde(default)]
    pub scoped_tokens: Vec<ScopedToken>,
    /// The network policies each group holds; empty in backups taken before these sets were
    /// backed up
    #[serde(default)]
    pub network_policy_sets: Vec<NetworkPolicySet>,
    /// The average runtime of each tool instance, in seconds, keyed by the tool instance's id;
    /// empty in backups taken before these were backed up
    #[serde(default)]
    pub workload_runtimes: BTreeMap<Uuid, f64>,
    /// The dangling references a backup taken with `prune_dangling` dropped, and the owners it
    /// added; left out when nothing was pruned, which is always the case without the param
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pruned: Vec<PrunedReference>,
}

/// The params for taking a backup
#[derive(Serialize, Deserialize, Debug, Default, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "api", derive(utoipa::IntoParams))]
#[cfg_attr(feature = "api", into_params(parameter_in = Query))]
pub struct BackupParams {
    /// Drop the user and group references that are still dangling after every read of the
    /// backup and whose referent is confirmed gone, instead of failing the backup
    #[serde(default)]
    pub prune_dangling: bool,
}

impl BackupParams {
    /// Drops confirmed-dangling user and group references instead of failing the backup
    #[must_use]
    pub fn prune_dangling(mut self) -> Self {
        // ask the API to prune what stays dangling
        self.prune_dangling = true;
        self
    }
}

/// The params for restoring a backup
#[derive(Serialize, Deserialize, Debug, Default, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "api", derive(utoipa::IntoParams))]
#[cfg_attr(feature = "api", into_params(parameter_in = Query))]
pub struct RestoreParams {
    /// Delete every key in the API's Redis database with `FLUSHDB ASYNC` before writing the
    /// backup, other namespaces' keys included, instead of only this namespace's keys
    #[serde(default)]
    pub flush_db: bool,
}

impl RestoreParams {
    /// Flushes the API's whole Redis database instead of deleting only this namespace's keys
    #[must_use]
    pub fn flush_db(mut self) -> Self {
        // ask the API to flush its database before writing the backup
        self.flush_db = true;
        self
    }
}

/// What a backup did to a record when it pruned a dangling reference
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum PruneAction {
    /// A name of a record or group that is gone was dropped from the record
    Dropped,
    /// A name was added to the record, so it stays valid after a drop
    Added,
    /// The whole record was deleted, since a drop left it invalid
    Deleted,
}

/// The kind of record a pruned name refers to
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum PrunedReferent {
    /// A user
    User,
    /// A group
    Group,
}

/// One change a backup taken with `prune_dangling` made to a record; it names records and
/// referents, never a stored secret value
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct PrunedReference {
    /// The record changed: `groups/<group>`, `users/<user>`, or `scoped_tokens/<owner>/<name>`
    pub record: String,
    /// The field changed: a group role (`owners`, `managers`, `users`, `monitors`) or `groups`
    pub field: String,
    /// What was done
    pub action: PruneAction,
    /// The name dropped or added; `None` when the whole record was deleted
    pub name: Option<String>,
    /// The kind of record the name refers to; `None` when the whole record was deleted
    pub referent: Option<PrunedReferent>,
    /// Why the change was made
    pub reason: String,
}

/// The network policies available in one group (its `netpols` set in Redis)
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct NetworkPolicySet {
    /// The group
    pub group: String,
    /// The names of the network policies in the group
    pub policies: Vec<String>,
}

/// One reason a backup can't be built or restored, returned in the error body
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct BackupProblem {
    /// The Redis key, or the record's type and name for a validation problem
    pub key: String,
    /// What is wrong with it
    pub error: String,
}

impl BackupProblem {
    /// Builds a problem with a record or key
    ///
    /// # Arguments
    ///
    /// * `key` - The Redis key, or the record's type and name
    /// * `error` - What is wrong with it
    pub fn new<K: Into<String>, E: Into<String>>(key: K, error: E) -> Self {
        // the record and what is wrong with it
        BackupProblem {
            key: key.into(),
            error: error.into(),
        }
    }
}

/// The body of an error that lists every problem a backup or restore found
///
/// Its `Debug` output counts the problems without listing them, so an error logged through
/// `Debug` doesn't repeat a list its message already logs.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct BackupProblems {
    /// What failed
    pub error: String,
    /// Every problem found
    pub problems: Vec<BackupProblem>,
}

impl BackupProblems {
    /// The most problems a log line lists; the rest are only counted
    pub const LOGGED: usize = 100;

    /// Gets the problems a log line lists and how many more were found
    #[must_use]
    pub fn logged(&self) -> (&[BackupProblem], usize) {
        // list the first problems and count the rest
        let listed = self.problems.len().min(Self::LOGGED);
        (&self.problems[..listed], self.problems.len() - listed)
    }
}

impl std::fmt::Debug for BackupProblems {
    /// Writes what failed and how many problems were found
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        // count the problems rather than listing them
        f.debug_struct("BackupProblems")
            .field("error", &self.error)
            .field(
                "problems",
                &format_args!("{} problems", self.problems.len()),
            )
            .finish()
    }
}

/// An update for a specific streamer info
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct StreamerInfoUpdate {
    /// The new most recent timestamp a streamer has tried to stream
    pub latest: Option<DateTime<Utc>>,
    /// A list of new cursor ids this streamer is crawling
    pub add_cursors: Vec<Uuid>,
    /// a list of exhausted cursors this streamer is no longer using
    pub remove_cursors: Vec<Uuid>,
}

/// The information needed to register a node
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct NodeRegistration {
    /// The cluster this node is in
    pub cluster: String,
    /// This nodes name
    pub name: String,
    /// The amount of resources this node has
    pub resources: Resources,
}

impl NodeRegistration {
    /// Create a new node registration object
    ///
    /// # Arguments
    ///
    /// * `cluster` - The cluster this node is in
    /// * `name` - The name of this node
    /// * `resources` - The resources this node has
    pub fn new<C: Into<String>, N: Into<String>>(
        cluster: C,
        name: N,
        resources: Resources,
    ) -> Self {
        NodeRegistration {
            cluster: cluster.into(),
            name: name.into(),
            resources,
        }
    }
}

/// The current health of this node
///
/// Archived backups store each variant as its position in this list, so variants are
/// append-only: never reorder, remove, or insert one; a new variant goes at the end. The
/// golden-bytes test of archived enums in thoradm's offline suite fails if the order changes.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(
    feature = "rkyv-support",
    derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)
)]
#[cfg_attr(
    feature = "rkyv-support",
    archive_attr(derive(Debug, bytecheck::CheckBytes))
)]
#[cfg_attr(feature = "scylla-utils", derive(thorium_derive::ScyllaStoreJson))]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum NodeHealth {
    /// This node is healthy and able to schedule jobs
    Healthy,
    /// This node is unhealthy and cannot schedule new jobs
    Unhealthy,
    /// This node has been deactivated
    Disabled(Option<String>),
    /// This node has been registered but has never completed a health check
    Registered,
}

impl std::fmt::Display for NodeHealth {
    /// Allow [`NodeHealth`] to be displayed
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            NodeHealth::Healthy => write!(f, "Healthy"),
            NodeHealth::Unhealthy => write!(f, "Unhealthy"),
            NodeHealth::Disabled(_) => write!(f, "Disabled"),
            NodeHealth::Registered => write!(f, "Registered"),
        }
    }
}

/// Information regarding a single node in use by Thorium
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct Node {
    /// The cluster this node is in
    pub cluster: String,
    /// This nodes name
    pub name: String,
    /// This nodes current health
    pub health: NodeHealth,
    /// The amount of resources this node has in total
    pub resources: Resources,
    /// The workers currently assigned to this node
    pub workers: HashMap<String, Worker>,
    /// The last time this node completed a health check
    pub heart_beat: Option<DateTime<Utc>>,
}

// A heartbeat for a nodes info
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct NodeUpdate {
    /// The updated health of this node
    pub health: NodeHealth,
    /// The updated resources to set
    pub resources: Resources,
    /// Whether this update is a heart beat or not
    #[serde(default)]
    pub heart_beat: bool,
}

impl NodeUpdate {
    /// Create a new node update
    ///
    /// * `health` - The new health to set
    /// * `resources` - The new resources this node has available
    #[must_use]
    pub fn new(health: NodeHealth, resources: Resources) -> Self {
        NodeUpdate {
            health,
            resources,
            heart_beat: false,
        }
    }

    /// Set that this update should update the heart beat timestamp
    #[must_use]
    pub fn heart_beat(mut self) -> Self {
        // set our heart beat flag
        self.heart_beat = true;
        self
    }

    /// Set that this update should update the heart beat timestamp
    pub fn heart_beat_mut(&mut self) {
        // set our heart beat flag
        self.heart_beat = true;
    }
}

/// Helps serde default the node list scalers to all possible options
#[must_use]
pub fn default_scalers() -> Vec<ImageScaler> {
    vec![
        ImageScaler::K8s,
        ImageScaler::Windows,
        ImageScaler::BareMetal,
        ImageScaler::Kvm,
        ImageScaler::External,
    ]
}

/// The parameters for a node get request
#[derive(Serialize, Deserialize, Debug)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "api", derive(utoipa::IntoParams))]
#[cfg_attr(feature = "api", into_params(parameter_in = Query))]
pub struct NodeGetParams {
    /// The scalers to limit our worker listing too
    #[serde(default = "default_scalers")]
    pub scalers: Vec<ImageScaler>,
}

impl Default for NodeGetParams {
    /// Create a default [`NodeGetParams`]
    fn default() -> Self {
        NodeGetParams {
            scalers: Vec::default(),
        }
    }
}
impl NodeGetParams {
    /// Add a new scaler to limit our listing too
    ///
    /// # Arguments
    ///
    /// * `scaler` - The scaler to add
    #[must_use]
    pub fn scaler(mut self, scaler: ImageScaler) -> Self {
        // add our scaler
        self.scalers.push(scaler);
        self
    }

    /// Adds new scalers to limit our listing too
    ///
    /// # Arguments
    ///
    /// * `scaler` - The scaler to add
    #[must_use]
    pub fn scalers(mut self, scaler: &[ImageScaler]) -> Self {
        // add our scalers
        self.scalers.extend(scaler);
        self
    }
}

/// Helps serde default the node list limit to 50
fn default_list_page_size() -> usize {
    50
}

/// The parameters for a node list request
#[derive(Serialize, Deserialize, Debug)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "api", derive(utoipa::IntoParams))]
#[cfg_attr(feature = "api", into_params(parameter_in = Query))]
pub struct NodeListParams {
    /// The cursor id to user if one exists
    pub cursor: Option<Uuid>,
    /// The max amount of data to retrieve on a single page
    #[serde(default = "default_list_page_size")]
    pub page_size: usize,
    /// The max amount of nodes to list in this cursor
    pub limit: Option<usize>,
    /// The clusters to limit our listing too
    #[serde(default)]
    pub clusters: Vec<String>,
    /// The nodes to limit our listing too
    #[serde(default)]
    pub nodes: Vec<String>,
    /// The scalers to limit our worker listing too
    #[serde(default = "default_scalers")]
    pub scalers: Vec<ImageScaler>,
}

impl Default for NodeListParams {
    /// Create a default [`NodeListParams`]
    fn default() -> Self {
        NodeListParams {
            cursor: None,
            page_size: default_list_page_size(),
            limit: None,
            clusters: Vec::default(),
            nodes: Vec::default(),
            scalers: Vec::default(),
        }
    }
}

impl NodeListParams {
    /// Set the cursor for listing the next page of nodes
    ///
    /// # Arguments
    ///
    /// * `cursor` - The cursor to set
    #[must_use]
    pub fn cursor(mut self, cursor: Uuid) -> Self {
        // set the cursor id
        self.cursor = Some(cursor);
        self
    }

    /// Set the max number of nodes to list on a single page
    ///
    /// # Arguments
    ///
    /// * `page_size` - The page size to set
    #[must_use]
    pub fn page_size(mut self, page_size: usize) -> Self {
        // set the page_size
        self.page_size = page_size;
        self
    }

    /// Set the max number of nodes to list in this cursor
    ///
    /// # Arguments
    ///
    /// * `limit` - The limit to set
    #[must_use]
    pub fn limit(mut self, limit: usize) -> Self {
        // set the limit
        self.limit = Some(limit);
        self
    }

    /// Add a new cluster to limit our listing too
    ///
    /// # Arguments
    ///
    /// * `cluster` - The cluster to add
    #[must_use]
    pub fn cluster<C: Into<String>>(mut self, cluster: C) -> Self {
        // add our cluster
        self.clusters.push(cluster.into());
        self
    }

    /// Adds new clusters to limit our listing too
    ///
    /// # Arguments
    ///
    /// * `cluster` - The clusters to add
    #[must_use]
    pub fn clusters<C: Into<String>>(mut self, cluster: Vec<C>) -> Self {
        // add our clusters
        self.clusters.extend(cluster.into_iter().map(Into::into));
        self
    }

    /// Add a new node to limit our listing too
    ///
    /// # Arguments
    ///
    /// * `node` - The node to add
    #[must_use]
    pub fn node<N: Into<String>>(mut self, node: N) -> Self {
        // add our node
        self.nodes.push(node.into());
        self
    }

    /// Adds new nodes to limit our listing too
    ///
    /// # Arguments
    ///
    /// * `node` - The nodes to add
    #[must_use]
    pub fn nodes<N: Into<String>>(mut self, node: impl Iterator<Item = N>) -> Self {
        // add our nodes
        self.nodes.extend(node.into_iter().map(Into::into));
        self
    }

    /// Add a new scaler to limit our listing too
    ///
    /// # Arguments
    ///
    /// * `scaler` - The scaler to add
    #[must_use]
    pub fn scaler(mut self, scaler: ImageScaler) -> Self {
        // add our scaler
        self.scalers.push(scaler);
        self
    }

    /// Adds new scalers to limit our listing too
    ///
    /// # Arguments
    ///
    /// * `scaler` - The scaler to add
    #[must_use]
    pub fn scalers(mut self, scaler: &[ImageScaler]) -> Self {
        // add our scalers
        self.scalers.extend(scaler);
        self
    }

    /// Add the clusters in a config for configured scalers
    ///
    /// Clusters are only added if no clusters were requested, so a requested cluster filter is
    /// respected.
    ///
    /// # Arguments
    ///
    /// * `scaler` - The scaler to add clusters for
    /// * `config` - The Thorium config
    pub fn clusters_from_config(mut self, config: &Conf) -> Self {
        // respect any clusters that were already requested
        if !self.clusters.is_empty() {
            return self;
        }
        // add the clusters for our scalers from our config
        for scaler in &self.scalers {
            // add the right cluster names for each scaler
            match *scaler {
                ImageScaler::K8s => {
                    // get our k8s cluster names or aliased names
                    for (name, cluster) in &config.thorium.scaler.k8s.clusters {
                        // use either our cluster name or our alias
                        match &cluster.alias {
                            Some(alias) => self.clusters.push(alias.clone()),
                            None => self.clusters.push(name.clone()),
                        };
                    }
                }
                ImageScaler::BareMetal => {
                    // get our bare metal cluster names
                    let names = config.thorium.scaler.bare_metal.clusters.keys();
                    // add in our bare metal clusters
                    self.clusters.extend(names.cloned());
                }
                ImageScaler::Windows => {
                    // get our windows cluster names
                    let names = config.thorium.scaler.windows.clusters.iter();
                    // add in our windows clusters
                    self.clusters.extend(names.cloned());
                }
                ImageScaler::Kvm => {
                    // get our kvm cluster names
                    let names = config.thorium.scaler.kvm.clusters.keys();
                    // add in our kvm clusters
                    self.clusters.extend(names.cloned());
                }
                // The external scaler has no clusters
                ImageScaler::External => (),
            }
        }
        self
    }

    /// Add the clusters in a config for configured scalers
    ///
    /// Clusters are only added if no clusters were requested, so a requested cluster filter is
    /// respected.
    ///
    /// # Arguments
    ///
    /// * `scaler` - The scaler to add clusters for
    /// * `config` - The Thorium config
    pub fn clusters_from_config_ref(&mut self, config: &Conf) {
        // respect any clusters that were already requested
        if !self.clusters.is_empty() {
            return;
        }
        // add the clusters for our scalers from our config
        for scaler in self.scalers.clone() {
            // add the right cluster names for each scaler
            match scaler {
                ImageScaler::K8s => {
                    // get our k8s cluster names or aliased names
                    for (name, cluster) in &config.thorium.scaler.k8s.clusters {
                        // use either our cluster name or our alias
                        match &cluster.alias {
                            Some(alias) => self.clusters.push(alias.clone()),
                            None => self.clusters.push(name.clone()),
                        };
                    }
                }
                ImageScaler::BareMetal => {
                    // get our bare metal cluster names
                    let names = config.thorium.scaler.bare_metal.clusters.keys();
                    // add in our bare metal clusters
                    self.clusters.extend(names.cloned());
                }
                ImageScaler::Windows => {
                    // get our windows cluster names
                    let names = config.thorium.scaler.windows.clusters.iter();
                    // add in our windows clusters
                    self.clusters.extend(names.cloned());
                }
                ImageScaler::Kvm => {
                    // get our kvm cluster names
                    let names = config.thorium.scaler.kvm.clusters.keys();
                    // add in our kvm clusters
                    self.clusters.extend(names.cloned());
                }
                // The external scaler has no clusters
                ImageScaler::External => (),
            }
        }
    }
}

/// A list of nodes and their clusters
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(
    feature = "scylla-utils",
    derive(crate::models::schema::ScyllaProjection, scylla::SerializeRow)
)]
#[cfg_attr(
    feature = "scylla-utils",
    scylla(flavor = "enforce_order", skip_name_checks)
)]
#[cfg_attr(
    feature = "scylla-utils",
    thorium_projection(of = crate::models::schema::scylla::NodesTable)
)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct NodeListLine {
    /// The cluster this node is from
    pub cluster: String,
    /// The name of this node
    pub node: String,
}

/// The different types of pools in the scaler
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[cfg_attr(feature = "trace", derive(valuable::Valuable))]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum Pools {
    /// The fair share pool
    FairShare,
    /// The deadline pool
    Deadline,
}

impl Pools {
    /// Cast our pool kind to a str
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Pools::FairShare => "FairShare",
            Pools::Deadline => "Deadline",
        }
    }
}

#[cfg(feature = "client")]
#[allow(
    clippy::try_from_instead_of_from_str,
    reason = "public thorium API; replacing the TryFrom impl with FromStr changes which trait callers use"
)]
impl TryFrom<&str> for Pools {
    type Error = crate::client::Error;

    /// Get our pool kind from a str
    ///
    /// # Arguments
    ///
    /// * `raw` - The str to get our pool from
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "FairShare" => Ok(Pools::FairShare),
            "Deadline" => Ok(Pools::Deadline),
            _ => Err(crate::client::Error::new(format!(
                "Uknown pool kind: {value}",
            ))),
        }
    }
}

/// The info needed to register this worker
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct WorkerRegistration {
    /// The cluster this worker is assigned to
    pub cluster: String,
    /// The node this worker is on
    pub node: String,
    /// The unique name of this worker
    pub name: String,
    /// The user this worker is executing a job for
    pub user: String,
    /// The group this worker is executing a job in
    pub group: String,
    /// The tool instance whose jobs this worker claims
    pub tool_instance: Uuid,
    /// The run (reaction) this worker is limited to if it is a per-run worker (for a harness's
    /// tool calls, the parent workflow run)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<Uuid>,
    /// The resources used to spawn this worker
    pub resources: Resources,
    /// The pool this worker was spawned in
    pub pool: Pools,
}

impl WorkerRegistration {
    /// Create a new worker registration object
    ///
    /// # Arguments
    ///
    /// * `cluster` - The cluster this worker is assigned to
    /// * `node` - The node this worker is on
    /// * `name` - The unique name for this worker
    /// * `user` - The user this worker is executing jobs for
    /// * `group` - The group this worker is executing jobs in
    /// * `tool_instance` - The tool instance whose jobs this worker claims
    /// * `run` - The run this worker is limited to if it is a per-run worker
    /// * `resources` - The resources used to spawn this worker
    /// * `pool` - The pool this worker was spawned in
    #[allow(clippy::too_many_arguments)]
    pub fn new<
        C: Into<String>,
        N: Into<String>,
        T: Into<String>,
        U: Into<String>,
        G: Into<String>,
    >(
        cluster: C,
        node: N,
        name: T,
        user: U,
        group: G,
        tool_instance: Uuid,
        run: Option<Uuid>,
        resources: Resources,
        pool: Pools,
    ) -> Self {
        WorkerRegistration {
            cluster: cluster.into(),
            node: node.into(),
            name: name.into(),
            user: user.into(),
            group: group.into(),
            tool_instance,
            run,
            resources,
            pool,
        }
    }

    /// Set the run this worker serves, making it a per-run worker
    ///
    /// # Arguments
    ///
    /// * `run` - The run this worker serves
    #[must_use]
    pub fn run(mut self, run: Uuid) -> Self {
        // set the run this worker serves
        self.run = Some(run);
        self
    }
}

impl From<Worker> for WorkerRegistration {
    /// Convert a worker to a worker registration object
    ///
    /// # Arguments
    ///
    /// * `worker` - The worker object to convert
    fn from(worker: Worker) -> Self {
        WorkerRegistration {
            cluster: worker.cluster,
            node: worker.node,
            name: worker.name,
            user: worker.user,
            group: worker.group,
            tool_instance: worker.tool_instance,
            run: worker.run,
            resources: worker.resources,
            pool: worker.pool,
        }
    }
}

/// The possible statuses of workers
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum WorkerStatus {
    /// This worker is being setup
    Spawning,
    /// This worker is currently executing jobs
    Running,
    /// This worker is being ordered to shutdown
    Shutdown,
}

impl WorkerStatus {
    /// Convert this status to a str
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            WorkerStatus::Spawning => "Spawning",
            WorkerStatus::Running => "Running",
            WorkerStatus::Shutdown => "Shutdown",
        }
    }
}

impl Display for WorkerStatus {
    /// Display this worker's status
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter in use
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        // write this as a str
        write!(f, "{}", self.as_str())
    }
}

// only implement the CQL value conversion if the API feature is enabled
impl TryFrom<&String> for WorkerStatus {
    type Error = InvalidEnum;
    // try to convert our string to a [`WorkerStatus`]
    fn try_from(raw: &String) -> Result<Self, Self::Error> {
        match raw.as_str() {
            "Spawning" => Ok(WorkerStatus::Spawning),
            "Running" => Ok(WorkerStatus::Running),
            "Shutdown" => Ok(WorkerStatus::Shutdown),
            _ => Err(InvalidEnum(format!("Uknown Worker Status: {raw}",))),
        }
    }
}

/// The current active job info for a worker
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ActiveJob {
    /// The reaction this worker is executing a job in
    pub reaction: Uuid,
    /// The job this worker is executing
    pub job: Uuid,
}

/// A active worker for a specific cluster and node
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct Worker {
    /// The cluster this worker is assigned to
    pub cluster: String,
    /// The node this worker is on
    pub node: String,
    /// The scaler this worker was spawned under
    pub scaler: ImageScaler,
    /// The name of this worker
    pub name: String,
    /// The user this worker is executing a job for
    pub user: String,
    /// The group this worker is executing a job in
    pub group: String,
    /// The tool instance whose jobs this worker claims
    pub tool_instance: Uuid,
    /// The run (reaction) this worker is limited to if it is a per-run worker (for a harness's
    /// tool calls, the parent workflow run)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<Uuid>,
    /// The current status of this worker
    pub status: WorkerStatus,
    /// When this worker was spawned
    pub spawned: DateTime<Utc>,
    /// The last time this worker checked in with Thorium
    pub heart_beat: Option<DateTime<Utc>>,
    /// The resources used to spawn this worker
    pub resources: Resources,
    /// The pool this worker was spawned in
    pub pool: Pools,
    /// The current active job info for this worker if it has one
    pub active: Option<ActiveJob>,
}

/// A list of all active workers in Thorium
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkerList {
    /// The currently active workers for this scaler
    pub workers: Vec<Worker>,
}

/// The workers to add or remove from scylla
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct WorkerRegistrationList {
    /// The workers to add or remove from scylla
    pub workers: Vec<WorkerRegistration>,
}

impl WorkerRegistrationList {
    /// Adds a new worker to this registration list
    ///
    /// # Arguments
    ///
    /// * `cluster` - The cluster this worker is assigned to
    /// * `node` - The node this worker is on
    /// * `name` - The unique name for this worker
    /// * `user` - The user this worker is executing jobs for
    /// * `group` - The group this worker is executing jobs in
    /// * `tool_instance` - The tool instance whose jobs this worker claims
    /// * `run` - The run this worker is limited to if it is a per-run worker
    /// * `resources` - The resources used to spawn this worker
    /// * `pool` - The pool this worker was spawned in
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn add<
        C: Into<String>,
        N: Into<String>,
        T: Into<String>,
        U: Into<String>,
        G: Into<String>,
    >(
        mut self,
        cluster: C,
        node: N,
        name: T,
        user: U,
        group: G,
        tool_instance: Uuid,
        run: Option<Uuid>,
        resources: Resources,
        pool: Pools,
    ) -> Self {
        // build our worker
        let worker = WorkerRegistration::new(
            cluster,
            node,
            name,
            user,
            group,
            tool_instance,
            run,
            resources,
            pool,
        );
        // add this worker
        self.workers.push(worker);
        self
    }

    /// Adds a new worker to this registraiton list
    ///
    /// # Arguments
    ///
    /// * `worker` - The worker to add or remove
    pub fn add_mut(&mut self, worker: WorkerRegistration) {
        self.workers.push(worker);
    }
}

impl Add<WorkerRegistration> for WorkerRegistrationList {
    type Output = Self;
    /// Adds a new worker to add or remove
    ///
    /// # Arguments
    ///
    /// * `worker` - The worker to add or remove
    fn add(mut self, worker: WorkerRegistration) -> Self {
        self.workers.push(worker);
        self
    }
}

impl From<WorkerList> for WorkerRegistrationList {
    /// Convert a list of workers to a list of workers to add or remove
    ///
    /// # Arguments
    ///
    /// * `workers` - The workers to add or remove
    fn from(list: WorkerList) -> Self {
        // convert our list of workers to a list of worker registrations
        let workers = list
            .workers
            .into_iter()
            .map(WorkerRegistration::from)
            .collect::<Vec<WorkerRegistration>>();
        // build our worker registration list object
        WorkerRegistrationList { workers }
    }
}

/// The updates to apply to a worker
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct WorkerUpdate {
    /// The new status to set
    pub status: WorkerStatus,
}

impl WorkerUpdate {
    /// Create a new worker update
    ///
    /// # Arguments
    ///
    /// * `status` - The new status to set
    #[must_use]
    pub fn new(status: WorkerStatus) -> Self {
        WorkerUpdate { status }
    }
}

/// A map of clusters to delete workers from
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct WorkerDeleteMap {
    /// The names of the workers to delete
    pub workers: Vec<String>,
}

impl WorkerDeleteMap {
    /// Create a worker map with some capacity
    ///
    /// # Arguments
    ///
    /// * `capacity` - The capacity to allocate
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        // instance our vec with the right capacity
        let workers = Vec::with_capacity(capacity);
        WorkerDeleteMap { workers }
    }

    /// Add a worker to be deleted
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the worker to delete
    #[must_use]
    #[allow(
        clippy::should_implement_trait,
        reason = "public builder method of the thorium client; renaming it or implementing Add is an API change"
    )]
    pub fn add<W: Into<String>>(mut self, name: W) -> Self {
        // add our worker
        self.workers.push(name.into());
        self
    }

    /// Add a worker to be deleted
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the worker to delete
    pub fn add_mut<W: Into<String>>(&mut self, name: W) {
        // add our worker
        self.workers.push(name.into());
    }
}

impl From<HashMap<String, Worker>> for WorkerDeleteMap {
    /// Build a worker delete map from a map of workers
    fn from(workers: HashMap<String, Worker>) -> Self {
        // build a vec of deletes for our map
        let workers = workers.keys().cloned().collect();
        // build our worker delete map
        WorkerDeleteMap { workers }
    }
}

/// A worker to delete
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct WorkerDelete {
    /// The cluster this worker is from
    pub cluster: String,
    /// The node this worker is on
    pub node: String,
    /// The name of this worker
    pub name: String,
}

impl WorkerDelete {
    /// Add a worker to be deleted
    ///
    /// # Arguments
    ///
    /// * `cluster` - The cluster to delete a worker from
    /// * `node` - The node to delete a worker from
    /// * `name` - The name of the worker to delete
    pub fn new<C: Into<String>, N: Into<String>, W: Into<String>>(
        cluster: C,
        node: N,
        name: W,
    ) -> Self {
        // build our worker delete
        WorkerDelete {
            cluster: cluster.into(),
            node: node.into(),
            name: name.into(),
        }
    }
}

/// The parameters for a worker list request
#[derive(Serialize, Deserialize, Debug)]
pub struct WorkerListParams {
    /// The cursor id to user if one exists
    pub cursor: Option<Uuid>,
    /// The max amount of data to retrieve on a single page
    #[serde(default = "default_list_page_size")]
    pub page_size: usize,
    /// The max amount of workers to list in this cursor
    pub limit: Option<usize>,
    /// The clusters to limit our listing too
    #[serde(default)]
    pub clusters: Vec<String>,
    /// The scalers to limit our worker listing too
    #[serde(default = "default_scalers")]
    pub scalers: Vec<ImageScaler>,
}

/// The different components in Thorium
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum SystemComponents {
    Api,
    Scaler(ImageScaler),
    EventHandler,
    SearchStreamer,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use clap::ValueEnum;

    /// `default_scalers` (the serde default for node list/get params) covers every scaler,
    /// including `ImageScaler::Kvm`, so node and worker listings that don't name their scalers
    /// include KVM workers and KVM clusters.
    #[test]
    fn default_scalers_include_every_scaler() {
        let defaults = default_scalers();
        for scaler in ImageScaler::value_variants() {
            assert!(
                defaults.contains(scaler),
                "the default node scalers must include {scaler:?}"
            );
        }
    }

    /// The default scalers include K8s.
    #[test]
    fn default_scalers_include_k8s() {
        assert!(default_scalers().contains(&ImageScaler::K8s));
    }

    /// Build node list params for the windows scaler limited to some clusters
    ///
    /// # Arguments
    ///
    /// * `clusters` - The clusters to limit our listing too
    fn windows_params(clusters: &[&str]) -> NodeListParams {
        NodeListParams {
            clusters: clusters
                .iter()
                .map(|cluster| (*cluster).to_owned())
                .collect(),
            scalers: vec![ImageScaler::Windows],
            ..NodeListParams::default()
        }
    }

    /// Load the test config with two windows clusters, prod and dev
    fn windows_conf() -> Conf {
        let mut conf = Conf::new("tests/thorium-testing.yml").unwrap();
        conf.thorium.scaler.windows.clusters = vec!["prod".to_owned(), "dev".to_owned()];
        conf
    }

    /// `clusters_from_config_ref` keeps a user supplied cluster filter as is, so a node listing
    /// filtered to `["prod"]` only returns nodes from prod rather than every configured cluster.
    #[test]
    fn cluster_filter_is_respected() {
        let mut params = windows_params(&["prod"]);
        params.clusters_from_config_ref(&windows_conf());
        assert_eq!(
            params.clusters,
            vec!["prod".to_owned()],
            "a user supplied cluster filter must not be expanded to every configured cluster"
        );
    }

    /// With no cluster filter every configured cluster is listed.
    #[test]
    fn empty_cluster_filter_expands_to_configured_clusters() {
        let mut params = windows_params(&[]);
        params.clusters_from_config_ref(&windows_conf());
        assert_eq!(params.clusters, vec!["prod".to_owned(), "dev".to_owned()]);
    }

    /// Build an allowed gateway entry
    ///
    /// # Arguments
    ///
    /// * `url` - The allowed base url
    fn gateway_entry(url: &str) -> crate::models::AllowedEndpoint {
        crate::models::AllowedEndpoint::new(
            crate::models::ModelProviderKind::OpenAiCompatible,
            Some(url.to_owned()),
        )
    }

    /// Build an `OpenAiCompatible` provider at a url
    ///
    /// # Arguments
    ///
    /// * `url` - The provider's base url
    fn compatible(url: &str) -> crate::models::ModelProvider {
        crate::models::ModelProvider::OpenAiCompatible {
            base_url: url.to_owned(),
            extra_headers: BTreeMap::new(),
            key_header: None,
        }
    }

    /// The model whitelist round-trips inside the system settings and loads empty when absent
    #[test]
    fn models_conf_round_trips() {
        // settings with two allowed endpoints
        let settings = SystemSettings {
            models: crate::models::ModelsConf {
                allowed_endpoints: std::collections::BTreeSet::from([
                    gateway_entry("https://b.example/v1"),
                    crate::models::AllowedEndpoint::new(
                        crate::models::ModelProviderKind::Bedrock,
                        None,
                    ),
                ]),
            },
            ..SystemSettings::default()
        };
        let raw = serde_json::to_string(&settings).unwrap();
        let back: SystemSettings = serde_json::from_str(&raw).unwrap();
        assert_eq!(back, settings);
        // settings JSON without models loads an empty list
        let mut value = serde_json::to_value(SystemSettings::default()).unwrap();
        value.as_object_mut().unwrap().remove("models");
        let loaded: SystemSettings = serde_json::from_value(value).unwrap();
        assert_eq!(loaded.models, crate::models::ModelsConf::default());
    }

    /// Whitelist entries match the provider and url exactly apart from one trailing slash, and an
    /// unset url matches only the provider's default urls
    #[test]
    fn whitelist_matching() {
        use crate::models::{
            AllowedEndpoint, BedrockEndpointKind, ModelProvider, ModelProviderKind, ModelsConf,
        };
        // an empty list allows nothing
        assert!(!ModelsConf::default().allows(&compatible("https://gw.example/v1")));
        // a gateway entry allows its url with or without one trailing slash
        let conf = ModelsConf {
            allowed_endpoints: std::collections::BTreeSet::from([
                gateway_entry("https://gw.example/v1"),
                AllowedEndpoint::new(ModelProviderKind::Bedrock, None),
                AllowedEndpoint::new(ModelProviderKind::Anthropic, None),
            ]),
        };
        assert!(conf.allows(&compatible("https://gw.example/v1")));
        assert!(conf.allows(&compatible("https://gw.example/v1/")));
        for url in [
            "https://gw.example/v1//",
            "https://GW.example/v1",
            "https://gw.example/v2",
        ] {
            assert!(!conf.allows(&compatible(url)), "{url} was allowed");
        }
        // the same url under another provider isn't allowed
        let litellm = ModelProvider::LiteLlm {
            base_url: "https://gw.example/v1".to_owned(),
            key_header: None,
        };
        assert!(!conf.allows(&litellm));
        // a Bedrock entry without a url allows every region's default url and no custom one
        let bedrock = |region: &str, endpoint_url: Option<&str>| ModelProvider::Bedrock {
            region: region.to_owned(),
            endpoint_kind: BedrockEndpointKind::Runtime,
            endpoint_url: endpoint_url.map(ToOwned::to_owned),
            workspace_id: None,
        };
        assert!(conf.allows(&bedrock("us-east-1", None)));
        assert!(conf.allows(&bedrock("eu-west-3", None)));
        assert!(!conf.allows(&bedrock("us-east-1", Some("https://vpce.example"))));
        // an Anthropic entry without a url allows only the default url
        let anthropic = |base_url: Option<&str>| ModelProvider::Anthropic {
            base_url: base_url.map(ToOwned::to_owned),
            version: "2023-06-01".to_owned(),
            betas: std::collections::BTreeSet::new(),
            workspace_id: None,
        };
        assert!(conf.allows(&anthropic(None)));
        assert!(!conf.allows(&anthropic(Some("https://proxy.example"))));
        // a refusal names the endpoint and the provider's entries
        let err = conf
            .check("my-gateway", &compatible("https://gw.example/v2"))
            .unwrap_err();
        assert!(err.contains("my-gateway"), "{err}");
        assert!(err.contains("https://gw.example/v1"), "{err}");
    }
}

#[cfg(all(test, feature = "api"))]
mod schema_tests {
    use serde_json::{Value, json};
    use utoipa::PartialSchema;

    use super::{HostPathWhitelistUpdate, SystemSettings};
    use crate::models::{Ipv4Block, Ipv6Block};

    /// Returns one property of a schema without its description, serialized to JSON
    ///
    /// # Arguments
    ///
    /// * `schema` - The schema holding the property
    /// * `name` - The property's name
    fn property(schema: &Value, name: &str) -> Value {
        // the generated schema lists every field under properties, with its doc comment
        let mut property = schema["properties"][name].clone();
        if let Some(fields) = property.as_object_mut() {
            fields.remove("description");
        }
        property
    }

    /// Host path sets are described as arrays of distinct strings, matching how they serialize
    #[test]
    fn host_path_sets_are_arrays_of_distinct_strings() {
        // a set of paths serializes as a JSON array whose order means nothing
        let set = json!({"type": "array", "items": {"type": "string"}, "uniqueItems": true});
        let settings = serde_json::to_value(SystemSettings::schema()).unwrap();
        assert_eq!(property(&settings, "host_path_whitelist"), set);
        let update = serde_json::to_value(HostPathWhitelistUpdate::schema()).unwrap();
        assert_eq!(property(&update, "add_paths"), set);
        assert_eq!(property(&update, "remove_paths"), set);
    }

    /// The CIDRs a network policy block excludes are described as an optional list of strings
    #[test]
    fn excluded_cidrs_are_optional_lists_of_strings() {
        // an absent exclusion list serializes as nothing or null, a present one as an array
        let list = json!({"type": ["array", "null"], "items": {"type": "string"}});
        for block in [
            serde_json::to_value(Ipv4Block::schema()).unwrap(),
            serde_json::to_value(Ipv6Block::schema()).unwrap(),
        ] {
            assert_eq!(property(&block, "except"), list, "{block}");
        }
    }
}
