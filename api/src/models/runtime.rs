//! Workload specs: what Thorium runs as jobs, grouped by the component that reads each setting
//!
//! A [`WorkloadSpec`] holds the settings of an [`Image`] regrouped by reader: the environment it
//! runs in (with the scheduling settings of that environment), how the thorium-agent executes each
//! job, and the data each job receives and produces. Specs convert from an existing [`Image`] and
//! project back into an in-memory [`Image`] for the readers that consume one.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use uuid::Uuid;

use super::conversions::ConversionError;
use super::{
    CacheDependencySettings, ChildFilters, ChildrenDependencySettings, Cleanup, Dependencies,
    DependencyPassStrategy, EphemeralDependencySettings, FileSystemDependencySettings, Image,
    ImageArgs, ImageJobInfo, ImageLifetime, ImageRequest, ImageScaler, Kvm, KwargDependency,
    OutputCollection, OutputDisplayType, RepoDependencySettings, Resources, ResourcesRequest,
    ResultDependencySettings, SampleDependencySettings, SecurityContext, SpawnLimits,
    TagDependencySettings, Volume,
};

#[cfg(test)]
mod tests;

/// The runtime the readers that have no measured runtime give a projected image, in seconds
pub const DEFAULT_PROJECTED_RUNTIME: f64 = 600.0;

/// Something Thorium runs as jobs
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct WorkloadSpec {
    /// Where this workload runs and the settings specific to that environment
    pub environment: WorkloadEnvironment,
    /// How each job of this workload is executed by the thorium-agent
    #[serde(default)]
    pub execution: ExecutionSettings,
    /// The data each job receives before it runs and what is collected from it after it exits
    #[serde(default)]
    pub data: JobDataSettings,
}

/// The environments a workload can run in, each with its own settings
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum WorkloadEnvironment {
    /// A container scheduled by the K8s scaler
    K8s(K8sEnvironment),
    /// A process launched directly on a node by the reactor
    BareMetal(BareMetalEnvironment),
    /// A container launched on a Windows node by the reactor
    Windows(WindowsEnvironment),
    /// A virtual machine launched by the reactor
    Kvm(KvmEnvironment),
    /// Jobs claimed and run by a system outside of Thorium through the API (no settings)
    #[default]
    External,
}

impl WorkloadEnvironment {
    /// Get the scaler responsible for workloads in this environment
    #[must_use]
    pub fn scaler(&self) -> ImageScaler {
        // each environment has its own scaler
        match self {
            WorkloadEnvironment::K8s(_) => ImageScaler::K8s,
            WorkloadEnvironment::BareMetal(_) => ImageScaler::BareMetal,
            WorkloadEnvironment::Windows(_) => ImageScaler::Windows,
            WorkloadEnvironment::Kvm(_) => ImageScaler::Kvm,
            WorkloadEnvironment::External => ImageScaler::External,
        }
    }

    /// Get the scheduling settings of this environment, if Thorium schedules it
    #[must_use]
    pub fn scheduling(&self) -> Option<&SchedulingSettings> {
        // every environment but External is scheduled by Thorium
        match self {
            WorkloadEnvironment::K8s(env) => Some(&env.scheduling),
            WorkloadEnvironment::BareMetal(env) => Some(&env.scheduling),
            WorkloadEnvironment::Windows(env) => Some(&env.scheduling),
            WorkloadEnvironment::Kvm(env) => Some(&env.scheduling),
            WorkloadEnvironment::External => None,
        }
    }
}

/// How the scaler schedules workers for a workload
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct SchedulingSettings {
    /// The resources each worker requires
    pub resources: Resources,
    /// The limit on how many workers can be spawned at once
    pub spawn_limit: SpawnLimits,
    /// The lifetime of a worker
    pub lifetime: Option<ImageLifetime>,
}

impl SchedulingSettings {
    /// Build the scheduling settings of an image source
    ///
    /// # Arguments
    ///
    /// * `source` - The image fields to copy the scheduling settings from
    fn from_source(source: &ImageSource<'_>) -> Self {
        // copy the fields the scaler and the agent read
        SchedulingSettings {
            resources: source.resources,
            spawn_limit: source.spawn_limit.clone(),
            lifetime: source.lifetime.clone(),
        }
    }
}

/// The settings for running a workload as a K8s container
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct K8sEnvironment {
    /// The container image to run, by tag (`repo/tool:1.2`) or pinned by digest (`repo/tool@sha256:...`)
    pub image: String,
    /// How the K8s scaler schedules this workload's workers
    pub scheduling: SchedulingSettings,
    /// The environment variables to set in the container
    #[serde(default)]
    pub env: BTreeMap<String, Option<String>>,
    /// Any K8s volumes to bind in (including secret volumes)
    #[serde(default)]
    pub volumes: Vec<Volume>,
    /// The K8s security context to run the container with
    #[serde(default)]
    pub security_context: SecurityContext,
    /// The K8s network policies applied to the pod (e.g. to allow egress to a model endpoint)
    #[serde(default)]
    pub network_policies: BTreeSet<String>,
}

/// The settings for running a workload directly on a node
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct BareMetalEnvironment {
    /// How the bare metal scaler schedules this workload's workers
    pub scheduling: SchedulingSettings,
    /// The user and group the reactor drops the tool process to
    #[serde(default)]
    pub security_context: SecurityContext,
    /// The script the reactor's `BareMetal` launcher runs when one of this workload's jobs is cancelled
    pub clean_up: Option<Cleanup>,
}

/// The settings for running a workload as a container on a Windows node
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct WindowsEnvironment {
    /// The container image to run
    pub image: String,
    /// How the Windows scaler schedules this workload's workers
    pub scheduling: SchedulingSettings,
}

/// The settings for running a workload in a KVM virtual machine
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct KvmEnvironment {
    /// How the KVM scaler schedules this workload's workers
    pub scheduling: SchedulingSettings,
    /// The golden VM definition and disk image paths (stored only: the launcher finds the golden
    /// files from the run group and the tool's name)
    #[serde(default)]
    pub kvm: Option<Kvm>,
}

/// How the thorium-agent executes each job of a workload
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ExecutionSettings {
    /// The entrypoint/command and how job args are passed
    #[serde(default)]
    pub args: ImageArgs,
    /// The max number of seconds a single job may run for
    pub timeout: Option<u64>,
    /// Whether stdout/stderr should be streamed back to Thorium (stored only: the agent streams every job's logs)
    #[serde(default)]
    pub collect_logs: bool,
    /// Whether this workload is a generator (copied to each job by the API when it creates the job)
    #[serde(default)]
    pub generator: bool,
    /// The path to any modifiers for this workload's results (stored only: nothing reads it)
    pub modifiers: Option<String>,
}

/// The data each job receives before it runs and what is collected from it after it exits
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct JobDataSettings {
    /// How inputs are downloaded and passed in to each job
    #[serde(default)]
    pub dependencies: ToolDependencies,
    /// How outputs are collected from each job
    #[serde(default)]
    pub output_collection: OutputCollection,
    /// Filters for children produced by each job
    #[serde(default)]
    pub child_filters: ChildFilters,
    /// How each job's output is displayed in the UI
    #[serde(default)]
    pub display_type: OutputDisplayType,
}

/// How each job of a workload gets its inputs, naming the tools whose outputs it takes by id
///
/// The same settings as an image's [`Dependencies`], except that the results, children, and
/// filesystems settings name tools by their ids instead of naming images.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ToolDependencies {
    /// The settings the agent uses when passing downloaded samples to tools
    #[serde(default)]
    pub samples: SampleDependencySettings,
    /// The settings the agent uses when passing downloaded ephemeral files to tools
    #[serde(default)]
    pub ephemeral: EphemeralDependencySettings,
    /// The settings the agent uses when passing prior results to tools
    #[serde(default)]
    pub results: ToolResultDependencySettings,
    /// The settings the agent uses when passing prior repos to tools
    #[serde(default)]
    pub repos: RepoDependencySettings,
    /// The settings the agent uses when passing tags to tools
    #[serde(default)]
    pub tags: TagDependencySettings,
    /// The settings the agent uses when passing children files from past tools
    #[serde(default)]
    pub children: ToolChildrenDependencySettings,
    /// The settings the agent uses when reconstructing filesystems dumped by past tools
    #[serde(default)]
    pub filesystems: ToolFileSystemDependencySettings,
    /// The settings to use when getting cache info
    #[serde(default)]
    pub cache: CacheDependencySettings,
}

impl ToolDependencies {
    /// Build a workload's dependency settings from an image's, with no tools named
    ///
    /// An image's settings name images, which have no tool ids, so the tool lists start empty.
    ///
    /// # Arguments
    ///
    /// * `deps` - The image's dependency settings
    #[must_use]
    pub fn from_image(deps: &Dependencies) -> Self {
        // copy every setting but the image names
        ToolDependencies {
            samples: deps.samples.clone(),
            ephemeral: deps.ephemeral.clone(),
            results: ToolResultDependencySettings {
                tools: Vec::new(),
                location: deps.results.location.clone(),
                kwarg: deps.results.kwarg.clone(),
                strategy: deps.results.strategy.clone(),
                names: deps.results.names.clone(),
            },
            repos: deps.repos.clone(),
            tags: deps.tags.clone(),
            children: ToolChildrenDependencySettings {
                enabled: deps.children.enabled,
                tools: Vec::new(),
                location: deps.children.location.clone(),
                kwarg: deps.children.kwarg.clone(),
                strategy: deps.children.strategy.clone(),
            },
            filesystems: ToolFileSystemDependencySettings {
                enabled: deps.filesystems.enabled,
                tools: Vec::new(),
                location: deps.filesystems.location.clone(),
                kwarg: deps.filesystems.kwarg.clone(),
                strategy: deps.filesystems.strategy.clone(),
            },
            cache: deps.cache.clone(),
        }
    }

    /// Project these settings into an image's, with no images named
    ///
    /// Tools are named by id, which an image's settings can't hold, so the image lists start
    /// empty; the thorium-agent fills them with the listed tools' current names.
    #[must_use]
    pub fn project(&self) -> Dependencies {
        // copy every setting but the tool ids
        Dependencies {
            samples: self.samples.clone(),
            ephemeral: self.ephemeral.clone(),
            results: ResultDependencySettings {
                images: Vec::new(),
                location: self.results.location.clone(),
                kwarg: self.results.kwarg.clone(),
                strategy: self.results.strategy.clone(),
                names: self.results.names.clone(),
            },
            repos: self.repos.clone(),
            tags: self.tags.clone(),
            children: ChildrenDependencySettings {
                enabled: self.children.enabled,
                images: Vec::new(),
                location: self.children.location.clone(),
                kwarg: self.children.kwarg.clone(),
                strategy: self.children.strategy.clone(),
            },
            filesystems: FileSystemDependencySettings {
                enabled: self.filesystems.enabled,
                images: Vec::new(),
                location: self.filesystems.location.clone(),
                kwarg: self.filesystems.kwarg.clone(),
                strategy: self.filesystems.strategy.clone(),
            },
            cache: self.cache.clone(),
        }
    }

    /// Get every tool these settings name, each once and sorted
    #[must_use]
    pub fn tools(&self) -> Vec<Uuid> {
        // gather the tools of every list
        let tools = self
            .results
            .tools
            .iter()
            .chain(&self.children.tools)
            .chain(&self.filesystems.tools)
            .copied()
            .collect::<BTreeSet<Uuid>>();
        tools.into_iter().collect()
    }
}

/// Get where prior result files are downloaded by default
fn default_results_location() -> String {
    // use the image settings' default
    ResultDependencySettings::default().location
}

/// Get where prior children are downloaded by default
fn default_children_location() -> String {
    // use the image settings' default
    ChildrenDependencySettings::default().location
}

/// Get how prior children are passed by default
fn default_children_strategy() -> DependencyPassStrategy {
    // use the image settings' default
    ChildrenDependencySettings::default().strategy
}

/// Get where prior filesystems are reconstructed by default
fn default_filesystems_location() -> String {
    // use the image settings' default
    FileSystemDependencySettings::default().location
}

/// Get how reconstructed filesystems are passed by default
fn default_filesystems_strategy() -> DependencyPassStrategy {
    // use the image settings' default
    FileSystemDependencySettings::default().strategy
}

/// The settings for the agent downloading prior results for a workload's jobs
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ToolResultDependencySettings {
    /// The tools whose results are downloaded (none downloads no results)
    #[serde(default)]
    pub tools: Vec<Uuid>,
    /// Where the agent stores downloaded prior result files
    #[serde(default = "default_results_location")]
    pub location: String,
    /// The kwarg to pass these files in with if one is set (otherwise use positional args)
    #[serde(default)]
    pub kwarg: KwargDependency,
    /// The strategy the agent uses when passing downloaded results to jobs
    #[serde(default)]
    pub strategy: DependencyPassStrategy,
    /// Any files to limit the download to
    #[serde(default)]
    pub names: Vec<String>,
}

impl Default for ToolResultDependencySettings {
    /// Build the default prior result settings, which name no tools
    fn default() -> Self {
        // match the image settings' defaults
        ToolResultDependencySettings {
            tools: Vec::new(),
            location: default_results_location(),
            kwarg: KwargDependency::default(),
            strategy: DependencyPassStrategy::default(),
            names: Vec::new(),
        }
    }
}

/// The settings for the agent downloading children of prior tools for a workload's jobs
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ToolChildrenDependencySettings {
    /// Whether children dependencies are enabled
    pub enabled: bool,
    /// The tools whose children are downloaded (none downloads the children of every tool)
    #[serde(default)]
    pub tools: Vec<Uuid>,
    /// Where the agent stores downloaded children
    #[serde(default = "default_children_location")]
    pub location: String,
    /// The kwarg to pass these children in with if one is set (otherwise use positional args)
    pub kwarg: Option<String>,
    /// The strategy the agent uses when passing downloaded children to jobs
    #[serde(default = "default_children_strategy")]
    pub strategy: DependencyPassStrategy,
}

impl Default for ToolChildrenDependencySettings {
    /// Build the default children settings, which are disabled and name no tools
    fn default() -> Self {
        // match the image settings' defaults
        ToolChildrenDependencySettings {
            enabled: false,
            tools: Vec::new(),
            location: default_children_location(),
            kwarg: None,
            strategy: default_children_strategy(),
        }
    }
}

/// The settings for the agent reconstructing filesystems of prior tools for a workload's jobs
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ToolFileSystemDependencySettings {
    /// Whether filesystem dependencies are enabled
    pub enabled: bool,
    /// The tools whose filesystems are reconstructed (none reconstructs every tool's)
    #[serde(default)]
    pub tools: Vec<Uuid>,
    /// Where the agent reconstructs filesystems
    #[serde(default = "default_filesystems_location")]
    pub location: String,
    /// The kwarg to pass these filesystems in with if one is set (otherwise use positional args)
    pub kwarg: Option<String>,
    /// The strategy the agent uses when passing reconstructed filesystems to jobs
    #[serde(default = "default_filesystems_strategy")]
    pub strategy: DependencyPassStrategy,
}

impl Default for ToolFileSystemDependencySettings {
    /// Build the default filesystem settings, which are disabled and name no tools
    fn default() -> Self {
        // match the image settings' defaults
        ToolFileSystemDependencySettings {
            enabled: false,
            tools: Vec::new(),
            location: default_filesystems_location(),
            kwarg: None,
            strategy: default_filesystems_strategy(),
        }
    }
}

/// An image field set to a non-default value that a conversion to a workload spec drops, because
/// the image's environment never reads it
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DroppedImageField {
    /// The container image reference
    Image,
    /// The resources each worker requires
    Resources,
    /// The limit on how many workers can be spawned at once
    SpawnLimit,
    /// The lifetime of a worker
    Lifetime,
    /// The environment variables, by name only (sorted) so their values never leave the image
    Env(Vec<String>),
    /// The volumes to bind in
    Volumes,
    /// The network policies applied to the pod
    NetworkPolicies,
    /// The security context to run with
    SecurityContext,
    /// The script to run when a job is cancelled
    CleanUp,
    /// The golden VM definition and disk image paths
    Kvm,
}

impl DroppedImageField {
    /// Get the name of the image field this entry names
    #[must_use]
    pub fn field(&self) -> &'static str {
        // use the image field's serialized name
        match self {
            DroppedImageField::Image => "image",
            DroppedImageField::Resources => "resources",
            DroppedImageField::SpawnLimit => "spawn_limit",
            DroppedImageField::Lifetime => "lifetime",
            DroppedImageField::Env(_) => "env",
            DroppedImageField::Volumes => "volumes",
            DroppedImageField::NetworkPolicies => "network_policies",
            DroppedImageField::SecurityContext => "security_context",
            DroppedImageField::CleanUp => "clean_up",
            DroppedImageField::Kvm => "kvm",
        }
    }
}

impl fmt::Display for DroppedImageField {
    /// Write the dropped field's name, followed by the variable names for `env`
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // name the env variables, never their values
            DroppedImageField::Env(names) => write!(f, "env ({})", names.join(", ")),
            // every other field is listed by name alone
            field => f.write_str(field.field()),
        }
    }
}

/// The workload spec an image converts to and the non-default image fields the conversion drops
#[derive(Debug, Clone, PartialEq)]
pub struct ImageConversion {
    /// The workload spec built from the image
    pub spec: WorkloadSpec,
    /// The non-default image fields the spec's environment never reads, in a fixed order
    pub dropped: Vec<DroppedImageField>,
}

/// Which environment-specific image fields an environment keeps
// each flag independently selects one image field an environment keeps
#[allow(clippy::struct_excessive_bools)]
struct KeptFields {
    /// Whether the container image reference is kept
    image: bool,
    /// Whether the scheduling settings (resources, spawn limit, and lifetime) are kept
    scheduling: bool,
    /// Whether the K8s pod settings (env, volumes, and network policies) are kept
    pod: bool,
    /// Whether the security context is kept
    security_context: bool,
    /// Whether the clean up script is kept
    clean_up: bool,
    /// Whether the kvm settings are kept
    kvm: bool,
}

impl KeptFields {
    /// Get the environment-specific image fields a scaler's environment keeps
    ///
    /// # Arguments
    ///
    /// * `scaler` - The scaler whose environment to describe
    fn for_scaler(scaler: ImageScaler) -> Self {
        // every scheduled environment keeps its scheduling settings
        let scheduling = scaler != ImageScaler::External;
        KeptFields {
            image: matches!(scaler, ImageScaler::K8s | ImageScaler::Windows),
            scheduling,
            pod: scaler == ImageScaler::K8s,
            security_context: matches!(scaler, ImageScaler::K8s | ImageScaler::BareMetal),
            clean_up: scaler == ImageScaler::BareMetal,
            kvm: scaler == ImageScaler::Kvm,
        }
    }

    /// List the non-default fields of an image that this environment drops, in a fixed order
    ///
    /// # Arguments
    ///
    /// * `image` - The image fields being converted
    fn dropped(&self, image: &ImageSource<'_>) -> Vec<DroppedImageField> {
        // collect the dropped fields in the order the checks below run
        let mut dropped = Vec::new();
        // the image reference is read only by the container environments
        if !self.image && image.image.is_some() {
            dropped.push(DroppedImageField::Image);
        }
        // scheduling settings are dropped only by environments Thorium doesn't schedule
        if !self.scheduling {
            // compare each scheduling field with the default a spec would hold
            let defaults = SchedulingSettings::default();
            if !unset_resources(&image.resources) {
                dropped.push(DroppedImageField::Resources);
            }
            // a limited spawn count is a non-default limit
            if *image.spawn_limit != defaults.spawn_limit {
                dropped.push(DroppedImageField::SpawnLimit);
            }
            // any lifetime is a non-default lifetime
            if *image.lifetime != defaults.lifetime {
                dropped.push(DroppedImageField::Lifetime);
            }
        }
        // the pod settings are read only by the K8s pod builder
        if !self.pod {
            // name the env variables but never their values
            if !image.env.is_empty() {
                let mut names = image.env.keys().cloned().collect::<Vec<String>>();
                names.sort_unstable();
                dropped.push(DroppedImageField::Env(names));
            }
            // list any volumes
            if !image.volumes.is_empty() {
                dropped.push(DroppedImageField::Volumes);
            }
            // list any network policies
            if !image.network_policies.is_empty() {
                dropped.push(DroppedImageField::NetworkPolicies);
            }
        }
        // the security context is read only by the K8s pod builder and the BareMetal launcher
        if !self.security_context && image.security_context != SecurityContext::default() {
            dropped.push(DroppedImageField::SecurityContext);
        }
        // the clean up script is read only by the BareMetal launcher
        if !self.clean_up && image.clean_up.is_some() {
            dropped.push(DroppedImageField::CleanUp);
        }
        // the kvm settings are only ever stored with a Kvm workload
        if !self.kvm && image.kvm.is_some() {
            dropped.push(DroppedImageField::Kvm);
        }
        dropped
    }
}

/// Check whether an image's resources are ones it holds when its creator set none
///
/// Images get the default resource request when their request names none, and zero resources
/// are the other way to say none. Worker slots are set by Thorium, never by an image's creator.
///
/// # Arguments
///
/// * `resources` - The image's resources
fn unset_resources(resources: &Resources) -> bool {
    // ignore the worker slots Thorium fills in
    let resources = Resources {
        worker_slots: 0,
        ..*resources
    };
    // compare with zero resources and with the resources an image is created with
    let created = Resources {
        worker_slots: 0,
        ..Resources::from(ResourcesRequest::default())
    };
    resources == Resources::default() || resources == created
}

/// The image fields a workload spec is built from
///
/// A stored [`Image`] and an [`ImageRequest`] both reduce to this view, so the conversion to a
/// spec has one implementation. The request's resources and security context are resolved the
/// way the API resolves them when it stores the image.
struct ImageSource<'a> {
    /// The group the image is in
    group: &'a str,
    /// The image's name
    name: &'a str,
    /// The scaler that runs the image
    scaler: ImageScaler,
    /// The container image reference
    image: &'a Option<String>,
    /// The worker lifetime
    lifetime: &'a Option<ImageLifetime>,
    /// The job timeout in seconds
    timeout: Option<u64>,
    /// The resources each worker requires
    resources: Resources,
    /// The limit on how many workers can be spawned at once
    spawn_limit: &'a SpawnLimits,
    /// The volumes to bind into the container
    volumes: &'a Vec<Volume>,
    /// The environment variables to set
    env: &'a HashMap<String, Option<String>>,
    /// The arguments for the image's jobs
    args: &'a ImageArgs,
    /// The path to the modifier folders
    modifiers: &'a Option<String>,
    /// The security context
    security_context: SecurityContext,
    /// Whether the agent streams stdout and stderr back
    collect_logs: bool,
    /// Whether the image is a generator
    generator: bool,
    /// The image's dependency settings
    dependencies: &'a Dependencies,
    /// The display type of the image's output
    display_type: OutputDisplayType,
    /// The settings for collecting results
    output_collection: &'a OutputCollection,
    /// The filters applied to children
    child_filters: &'a ChildFilters,
    /// The settings for cleaning up canceled jobs
    clean_up: &'a Option<Cleanup>,
    /// The Kvm settings
    kvm: &'a Option<Kvm>,
    /// The network policies applied to spawned pods
    network_policies: &'a HashSet<String>,
}

impl<'a> ImageSource<'a> {
    /// View a stored image as the fields a spec is built from
    ///
    /// # Arguments
    ///
    /// * `image` - The stored image
    fn from_image(image: &'a Image) -> Self {
        // borrow every field a spec reads
        ImageSource {
            group: &image.group,
            name: &image.name,
            scaler: image.scaler,
            image: &image.image,
            lifetime: &image.lifetime,
            timeout: image.timeout,
            resources: image.resources,
            spawn_limit: &image.spawn_limit,
            volumes: &image.volumes,
            env: &image.env,
            args: &image.args,
            modifiers: &image.modifiers,
            security_context: image.security_context.clone(),
            collect_logs: image.collect_logs,
            generator: image.generator,
            dependencies: &image.dependencies,
            display_type: image.display_type,
            output_collection: &image.output_collection,
            child_filters: &image.child_filters,
            clean_up: &image.clean_up,
            kvm: &image.kvm,
            network_policies: &image.network_policies,
        }
    }

    /// View an image request as the fields a spec is built from
    ///
    /// # Arguments
    ///
    /// * `request` - The image request
    fn from_request(request: &'a ImageRequest) -> Self {
        // borrow every field a spec reads, resolving what the API resolves on creation
        ImageSource {
            group: &request.group,
            name: &request.name,
            scaler: request.scaler,
            image: &request.image,
            lifetime: &request.lifetime,
            timeout: request.timeout,
            resources: Resources::from(request.resources.clone()),
            spawn_limit: &request.spawn_limit,
            volumes: &request.volumes,
            env: &request.env,
            args: &request.args,
            modifiers: &request.modifiers,
            security_context: request.security_context.clone().unwrap_or_default(),
            collect_logs: request.collect_logs,
            generator: request.generator,
            dependencies: &request.dependencies,
            display_type: request.display_type,
            output_collection: &request.output_collection,
            child_filters: &request.child_filters,
            clean_up: &request.clean_up,
            kvm: &request.kvm,
            network_policies: &request.network_policies,
        }
    }
}

/// Get the container image reference of an image whose environment requires one
///
/// # Arguments
///
/// * `image` - The image fields to read the reference from
fn required_image(image: &ImageSource<'_>) -> Result<String, ConversionError> {
    match image.image {
        // a non-empty reference is what the pod builder and the Windows launcher run
        Some(reference) if !reference.is_empty() => Ok(reference.clone()),
        // without one the workload can't run
        _ => Err(ConversionError::new(format!(
            "The {} image '{}' in group '{}' has no container image to run",
            image.scaler, image.name, image.group
        ))),
    }
}

impl WorkloadSpec {
    /// Convert an image into a workload spec for the environment of its scaler
    ///
    /// Each environment keeps the image fields it reads, and the execution and job data settings
    /// are copied for every environment. Fields the environment never reads are dropped, and the
    /// non-default ones are listed in the result (`env` by variable name only). An image's runtime,
    /// bans, scaler, group, name, creator, description, version, and pipelines are never part of
    /// a spec, so they aren't listed. The dependency settings name tools by id and images by
    /// name, so the spec's tool lists start empty; a migration fills them from its map of image
    /// names to tool ids.
    ///
    /// # Arguments
    ///
    /// * `image` - The image to convert
    pub fn from_image(image: &Image) -> Result<ImageConversion, ConversionError> {
        // build the spec from the stored image's fields
        Self::build(&ImageSource::from_image(image))
    }

    /// Convert an image request into the workload spec its image would convert to
    ///
    /// The request's resources and security context are resolved the way the API resolves them
    /// when it stores the image, so the result equals [`WorkloadSpec::from_image`] of the stored
    /// image. Fields the environment never reads are dropped and listed as in that conversion.
    ///
    /// # Arguments
    ///
    /// * `request` - The image request to convert
    pub fn from_image_request(request: &ImageRequest) -> Result<ImageConversion, ConversionError> {
        // build the spec from the request's fields
        Self::build(&ImageSource::from_request(request))
    }

    /// Build a spec from the image fields every conversion shares
    ///
    /// # Arguments
    ///
    /// * `image` - The image fields to convert
    fn build(image: &ImageSource<'_>) -> Result<ImageConversion, ConversionError> {
        // build the environment the image's scaler picks
        let scheduling = SchedulingSettings::from_source(image);
        let environment = match image.scaler {
            ImageScaler::K8s => WorkloadEnvironment::K8s(K8sEnvironment {
                image: required_image(image)?,
                scheduling,
                env: image
                    .env
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
                volumes: image.volumes.clone(),
                security_context: image.security_context.clone(),
                network_policies: image.network_policies.iter().cloned().collect(),
            }),
            ImageScaler::BareMetal => WorkloadEnvironment::BareMetal(BareMetalEnvironment {
                scheduling,
                security_context: image.security_context.clone(),
                clean_up: image.clean_up.clone(),
            }),
            ImageScaler::Windows => WorkloadEnvironment::Windows(WindowsEnvironment {
                image: required_image(image)?,
                scheduling,
            }),
            ImageScaler::Kvm => WorkloadEnvironment::Kvm(KvmEnvironment {
                scheduling,
                kvm: image.kvm.clone(),
            }),
            ImageScaler::External => WorkloadEnvironment::External,
        };
        // copy the execution settings, including the stored but unread ones
        let execution = ExecutionSettings {
            args: image.args.clone(),
            timeout: image.timeout,
            collect_logs: image.collect_logs,
            generator: image.generator,
            modifiers: image.modifiers.clone(),
        };
        // copy the job data settings
        let data = JobDataSettings {
            dependencies: ToolDependencies::from_image(image.dependencies),
            output_collection: image.output_collection.clone(),
            child_filters: image.child_filters.clone(),
            display_type: image.display_type,
        };
        // list the non-default fields this environment drops
        let dropped = KeptFields::for_scaler(image.scaler).dropped(image);
        // build the spec from its three groups of settings
        let spec = WorkloadSpec {
            environment,
            execution,
            data,
        };
        Ok(ImageConversion { spec, dropped })
    }

    /// Project this spec into the in-memory image the scaler, the agent, and the reactor read
    ///
    /// The projection is never stored. Its scaler comes from the environment, and fields the
    /// environment doesn't hold are left at their defaults (no bans, creator, version, or
    /// description). Its dependency image lists are empty, since the spec names tools by id.
    ///
    /// # Arguments
    ///
    /// * `group` - The group the worker runs in
    /// * `name` - The tool's name
    /// * `runtime` - The average runtime in seconds (only the scaler reads it; other readers
    ///   pass [`DEFAULT_PROJECTED_RUNTIME`])
    #[must_use]
    pub fn project_image(&self, group: &str, name: &str, runtime: f64) -> Image {
        // start from the scheduling settings, or their defaults for an unscheduled environment
        let scheduling = self.environment.scheduling().cloned().unwrap_or_default();
        // pull out the settings only some environments hold
        let mut image = None;
        let mut env = HashMap::new();
        let mut volumes = Vec::new();
        let mut security_context = SecurityContext::default();
        let mut network_policies = HashSet::new();
        let mut clean_up = None;
        let mut kvm = None;
        // fill in the settings the environment holds
        match &self.environment {
            WorkloadEnvironment::K8s(k8s) => {
                image = Some(k8s.image.clone());
                env = k8s
                    .env
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect();
                volumes.clone_from(&k8s.volumes);
                security_context = k8s.security_context.clone();
                network_policies = k8s.network_policies.iter().cloned().collect();
            }
            WorkloadEnvironment::BareMetal(bare_metal) => {
                security_context = bare_metal.security_context.clone();
                clean_up.clone_from(&bare_metal.clean_up);
            }
            WorkloadEnvironment::Windows(windows) => image = Some(windows.image.clone()),
            WorkloadEnvironment::Kvm(kvm_env) => kvm.clone_from(&kvm_env.kvm),
            WorkloadEnvironment::External => (),
        }
        // build the image the existing readers expect
        Image {
            group: group.to_owned(),
            name: name.to_owned(),
            creator: String::new(),
            version: None,
            scaler: self.environment.scaler(),
            image,
            lifetime: scheduling.lifetime,
            timeout: self.execution.timeout,
            resources: scheduling.resources,
            spawn_limit: scheduling.spawn_limit,
            env,
            runtime,
            volumes,
            args: self.execution.args.clone(),
            modifiers: self.execution.modifiers.clone(),
            description: None,
            security_context,
            used_by: Vec::new(),
            collect_logs: self.execution.collect_logs,
            generator: self.execution.generator,
            dependencies: self.data.dependencies.project(),
            display_type: self.data.display_type,
            output_collection: self.data.output_collection.clone(),
            child_filters: self.data.child_filters.clone(),
            clean_up,
            kvm,
            bans: HashMap::new(),
            network_policies,
        }
    }

    /// Get the info the API copies onto each job of this workload when it creates the job
    #[must_use]
    pub fn job_info(&self) -> ImageJobInfo {
        // jobs take the generator flag from the spec and the scaler from its environment
        ImageJobInfo {
            generator: self.execution.generator,
            scaler: self.environment.scaler(),
        }
    }
}
