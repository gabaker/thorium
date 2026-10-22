use chrono::prelude::*;
use hashbrown::HashMap;
use std::collections::BTreeMap;
use std::fmt;
use thorium::models::{
    Image, K8S_NAME_MAX_CHARS, Pools, Requisition, Resources, RunModels, SpawnedUpdate, Worker,
    k8s_name,
};
use thorium::same;

use crate::libs::cache::CachedWorkload;
use crate::libs::helpers;

/// The length of the random suffix that makes each worker's name unique
const NAME_SUFFIX_LEN: usize = 8;

/// Build a worker's name from its tool's name and version and a unique suffix
///
/// Each part keeps only the characters K8s allows, and the tool part is trimmed so the whole name
/// fits in a K8s name and starts with a letter or digit.
///
/// # Arguments
///
/// * `tool` - The tool's name
/// * `version` - The tool instance's version label
/// * `suffix` - The unique suffix to end the name with
#[must_use]
pub fn worker_name(tool: &str, version: &str, suffix: &str) -> String {
    // keep the version short enough to leave room for at least one character of the tool
    let version_max = K8S_NAME_MAX_CHARS.saturating_sub(suffix.len() + 3);
    let version = k8s_name(version, version_max);
    // trim the tool part so the whole name fits
    let tool_max = K8S_NAME_MAX_CHARS.saturating_sub(version.len() + suffix.len() + 2);
    let tool = k8s_name(tool, usize::MAX);
    let mut tool = tool
        .trim_start_matches('-')
        .chars()
        .take(tool_max)
        .collect::<String>();
    // a tool name with no allowed characters still needs a leading letter
    if tool.is_empty() {
        tool.push('t');
    }
    format!("{tool}-{version}-{suffix}")
}

/// The env var a per-run pod gets its API url in
pub const URL_ENV: &str = "THORIUM_URL";

/// The env var a per-run pod gets its run-scoped token in
pub const TOKEN_ENV: &str = "THORIUM_TOKEN";

/// The env var a harness's per-run pod gets every agent's model in
pub const MODELS_ENV: &str = "THORIUM_MODELS";

/// The values a per-run worker's pod gets as literal env values: its run's token and, for a
/// harness, its run's models
#[derive(Clone)]
pub struct RunPodEnv {
    /// The run-scoped token's value
    pub token: String,
    /// The run's models and the env values that carry them, for a harness's worker
    pub models: Option<RunModels>,
}

impl RunPodEnv {
    /// Get the names of the env vars this pod gets, in the order they're written
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        // the url and token always, the models and their values for a harness
        let mut names = vec![URL_ENV.to_owned(), TOKEN_ENV.to_owned()];
        if let Some(models) = &self.models {
            names.push(MODELS_ENV.to_owned());
            names.extend(models.env.keys().cloned());
        }
        names
    }

    /// Get the env vars this pod gets, by name and value, in the order they're written
    ///
    /// # Arguments
    ///
    /// * `url` - The API url the pod's agent talks to
    pub fn vars(&self, url: &str) -> Result<Vec<(String, String)>, serde_json::Error> {
        // the url and the run's token
        let mut vars = vec![
            (URL_ENV.to_owned(), url.to_owned()),
            (TOKEN_ENV.to_owned(), self.token.clone()),
        ];
        // a harness also gets every agent's model and its entry agent's values
        if let Some(models) = &self.models {
            vars.push((
                MODELS_ENV.to_owned(),
                serde_json::to_string(&models.models)?,
            ));
            vars.extend(
                models
                    .env
                    .iter()
                    .map(|(name, value)| (name.clone(), value.expose().to_owned())),
            );
        }
        Ok(vars)
    }
}

impl fmt::Debug for RunPodEnv {
    /// Write the names of this pod's env vars, never their values
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // only the names are safe to log
        f.debug_struct("RunPodEnv")
            .field("vars", &self.names())
            .finish()
    }
}

/// A currently active and spawned resource
#[derive(Debug, Clone)]
pub struct Spawned {
    /// The req that was tied to this pod
    pub req: Requisition,
    /// The cluster this req was spawned on
    pub cluster: String,
    /// The node this req was spawned on
    pub node: String,
    /// The unique name for this resource
    pub name: String,
    /// The resources in use by this spawned resource
    pub resources: Resources,
    /// What pool this worker was spawned with
    pub pool: Pools,
    /// Whether this resource needs to be spawned
    pub spawn: bool,
    /// Whether this resource has been told to scale down yet or not
    pub scaled_down: bool,
    /// When this resource can be scaled down to prevent flapping
    pub down_scalable: DateTime<Utc>,
    /// The literal env values of a per-run worker's pod, fetched right before it's spawned
    pub run_env: Option<RunPodEnv>,
}

impl Spawned {
    /// Create a new spawned object
    ///
    /// # Arguments
    ///
    /// * `cluster` - The cluster this worker was spawned on
    /// * `node` - The node this wokrer was spawned on
    /// * `req` - The requisition that led to this worker
    /// * `workload` - The workload of the tool instance this worker runs
    /// * `image` - The workload's projected image in the requisition's group
    /// * `pool` - The pool this worker was spawned in
    #[expect(clippy::cast_possible_truncation)]
    pub fn new<T: Into<String>>(
        cluster: T,
        node: T,
        req: Requisition,
        workload: &CachedWorkload,
        image: &Image,
        pool: Pools,
    ) -> Self {
        // generate a unique name from the tool's name and version
        let append = helpers::gen_string(NAME_SUFFIX_LEN);
        let name = worker_name(&workload.name, &workload.version, &append);
        // set a minimum bound on downscaling for at least 2 minutes
        let single_run_budget = std::cmp::max(image.runtime.ceil() as i64, 120);
        // calculate when this worker can be safely scaled down
        let down_scalable = helpers::seconds_from_now(single_run_budget);
        // create our spawned object
        Spawned {
            req,
            cluster: cluster.into(),
            node: node.into(),
            name,
            resources: image.resources,
            pool,
            spawn: true,
            scaled_down: false,
            down_scalable,
            run_env: None,
        }
    }
}

impl PartialEq<Worker> for Spawned {
    /// Check if a worker and a Spawned worker are the same
    ///
    /// # Arguments
    ///
    /// * `worker` - The worker to compare against
    fn eq(&self, other: &Worker) -> bool {
        // make sure this is for the correct cluster
        same!(self.name, other.name);
        same!(self.cluster, other.cluster);
        same!(self.node, other.node);
        true
    }
}

impl PartialEq<Spawned> for Spawned {
    /// Check if a worker and a Spawned worker are the same
    ///
    /// # Arguments
    ///
    /// * `worker` - The worker to compare against
    fn eq(&self, other: &Spawned) -> bool {
        // make sure this is for the correct cluster
        same!(self.name, other.name);
        same!(self.cluster, other.cluster);
        same!(self.node, other.node);
        true
    }
}

impl From<&Spawned> for SpawnedUpdate {
    /// Cast our spawned worker to a spawned update object
    ///
    /// # Arguments
    ///
    /// * `worker` - The worker to build a spawned update for
    fn from(spawned: &Spawned) -> Self {
        SpawnedUpdate {
            req: spawned.req.clone(),
            node: spawned.node.clone(),
            name: spawned.name.clone(),
            pool: spawned.pool,
            resources: spawned.resources,
            scaled_down: spawned.scaled_down,
        }
    }
}

/// A Map of requestions along with the number of pods possible to spawn
#[derive(Default, Debug, Clone)]
pub struct ReqMap {
    /// The resources to spawn
    pub spawns: HashMap<String, BTreeMap<DateTime<Utc>, Vec<Spawned>>>,
    /// The spawned resources to scale down
    pub scale_down: HashMap<String, Vec<Spawned>>,
}

impl ReqMap {
    /// Adds a resource to be scaled down
    ///
    /// # Arguments
    ///
    /// * `spawn` - The spawn to set to be scaled down
    pub fn scale_down(&mut self, spawn: Spawned) {
        // get an entry to the correct clusters scale down list
        let entry = self
            .scale_down
            .entry(spawn.cluster.clone())
            .or_insert_with(|| Vec::with_capacity(1));
        // add this spawn to this clusters scale down list
        entry.push(spawn);
    }
}

#[cfg(test)]
mod tests {
    use super::Spawned;
    use crate::libs::cache::CachedWorkload;
    use std::collections::BTreeMap;
    use thorium::models::{
        Image, Pools, Requisition, Resources, SecurityContext, SpawnLimits, ToolKind, WorkloadSpec,
    };
    use uuid::Uuid;

    /// Build the cached workload of a tool instance named `harvest`
    fn workload() -> CachedWorkload {
        // only the name and version are read when spawning
        CachedWorkload {
            name: "harvest".to_owned(),
            version: "1.0".to_owned(),
            groups: vec!["minerva".to_owned()],
            kind: ToolKind::Image,
            spec: WorkloadSpec::default(),
            projections: BTreeMap::new(),
        }
    }

    /// Build an image with a given average runtime
    ///
    /// # Arguments
    ///
    /// * `runtime` - The image's average runtime in seconds
    fn image(runtime: f64) -> Image {
        // build the image from JSON so only the runtime needs a real value
        serde_json::from_value(serde_json::json!({
            "group": "minerva",
            "name": "harvest",
            "creator": "io",
            "version": null,
            "image": null,
            "lifetime": null,
            "timeout": null,
            "resources": Resources::default(),
            "spawn_limit": SpawnLimits::default(),
            "runtime": runtime,
            "volumes": [],
            "modifiers": null,
            "description": null,
            "security_context": SecurityContext::default(),
            "used_by": [],
            "collect_logs": false,
            "generator": false,
            "clean_up": null,
            "kvm": null,
            "bans": {},
        }))
        .unwrap()
    }

    /// Spawning a worker for an image whose stored runtime is too large for a timestamp
    /// saturates its scale down time instead of panicking
    #[test]
    fn spawned_survives_huge_runtimes() {
        for runtime in [1e13, 1e16, f64::MAX] {
            // spawn a worker for an image with this runtime
            let req = Requisition::new("io", "minerva", Uuid::nil(), None);
            let spawned = Spawned::new(
                "cluster0",
                "node0",
                req,
                &workload(),
                &image(runtime),
                Pools::FairShare,
            );
            // it scales down at some point in the future
            assert!(spawned.down_scalable > chrono::Utc::now());
        }
    }

    /// A normal runtime still sets the scale down time that far out
    #[test]
    fn spawned_uses_the_runtime() {
        // spawn a worker for an image with a ten minute runtime
        let req = Requisition::new("io", "minerva", Uuid::nil(), None);
        let before = chrono::Utc::now();
        let spawned = Spawned::new(
            "cluster0",
            "node0",
            req,
            &workload(),
            &image(600.0),
            Pools::FairShare,
        );
        // it scales down ten minutes after it was spawned
        assert!(spawned.down_scalable >= before + chrono::TimeDelta::seconds(600));
        assert!(spawned.down_scalable <= chrono::Utc::now() + chrono::TimeDelta::seconds(600));
    }
}
