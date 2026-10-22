//! Workload specs: what Thorium runs as jobs, grouped by the component that reads each setting
//!
//! A [`WorkloadSpec`] holds the settings of an [`Image`] regrouped by reader: the environment it
//! runs in (with the scheduling settings of that environment), how the thorium-agent executes each
//! job, and the data each job receives and produces. Specs convert from an existing [`Image`] and
//! project back into an in-memory [`Image`] for the readers that consume one.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;

use super::conversions::ConversionError;
use super::{
    ChildFilters, Cleanup, Dependencies, Image, ImageArgs, ImageJobInfo, ImageLifetime,
    ImageScaler, Kvm, OutputCollection, OutputDisplayType, Resources, ResourcesRequest,
    SecurityContext, SpawnLimits, Volume,
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
    /// Build the scheduling settings of an image
    ///
    /// # Arguments
    ///
    /// * `image` - The image to copy the scheduling settings from
    fn from_image(image: &Image) -> Self {
        // copy the fields the scaler and the agent read
        SchedulingSettings {
            resources: image.resources,
            spawn_limit: image.spawn_limit.clone(),
            lifetime: image.lifetime.clone(),
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
    pub dependencies: Dependencies,
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
    /// * `image` - The image being converted
    fn dropped(&self, image: &Image) -> Vec<DroppedImageField> {
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
            if image.spawn_limit != defaults.spawn_limit {
                dropped.push(DroppedImageField::SpawnLimit);
            }
            // any lifetime is a non-default lifetime
            if image.lifetime != defaults.lifetime {
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

/// Get the container image reference of an image whose environment requires one
///
/// # Arguments
///
/// * `image` - The image to read the reference from
fn required_image(image: &Image) -> Result<String, ConversionError> {
    match &image.image {
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
    /// a spec, so they aren't listed.
    ///
    /// # Arguments
    ///
    /// * `image` - The image to convert
    pub fn from_image(image: &Image) -> Result<ImageConversion, ConversionError> {
        // build the environment the image's scaler picks
        let scheduling = SchedulingSettings::from_image(image);
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
            dependencies: image.dependencies.clone(),
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
    /// description).
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
            dependencies: self.data.dependencies.clone(),
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
