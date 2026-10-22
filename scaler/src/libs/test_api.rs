//! A mock Thorium API and fixtures for the scaler's unit tests

use chrono::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thorium::models::{
    BareMetalEnvironment, Deadline, Entity, EntityKinds, EntityMetadata, GroupStats, ImageScaler,
    K8sEnvironment, NetworkPolicy, Reaction, Resources, RunModels, ScalerStats, SchedulingSettings,
    ScopedToken, ScopedTokenRole, ScrubbedUser, SpawnLimits, StageStats, SystemSettings,
    SystemStats, ToolInstanceEntity, ToolSpec, ToolVersion, UserRole, WorkloadEnvironment,
    WorkloadRuntimes, WorkloadSpec,
};
use thorium::{Conf, Thorium};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

use super::{Cache, Scaler};
use crate::libs::schedulers::Allocatable;

/// The group most fixtures run in
pub const GROUP: &str = "ceres";

/// The state the mock API answers from
#[derive(Default)]
pub struct World {
    /// The users the API lists
    pub users: Vec<ScrubbedUser>,
    /// The system stats the API returns
    pub stats: Option<SystemStats>,
    /// The deadline window the API returns
    pub deadlines: Vec<Deadline>,
    /// The tool instances the API returns by id; every other id is a `404`
    pub entities: HashMap<Uuid, Entity>,
    /// The tool instance runtimes the API returns
    pub runtimes: WorkloadRuntimes,
    /// How long the API waits before answering an entity read
    pub entity_delay: Duration,
    /// The token value or error status the run token read returns, by run; every other run is
    /// a `404`
    pub run_tokens: HashMap<Uuid, Result<String, u16>>,
    /// The models or error status the run models read returns, by run; every other run is a
    /// `404`
    pub run_models: HashMap<Uuid, Result<RunModels, u16>>,
    /// The reactions the API returns by id; every other id is a `404`
    pub reactions: HashMap<Uuid, Reaction>,
    /// How long the API waits before answering a run token or run models read
    pub run_env_delay: Duration,
    /// The network policies the API lists
    pub network_policies: Vec<NetworkPolicy>,
}

/// A request the mock API received
#[derive(Debug, Clone)]
pub struct Recorded {
    /// The request's method
    pub method: String,
    /// The request's path without its query
    pub path: String,
    /// The request's query without its leading `?`
    pub query: String,
    /// The request's body
    pub body: Vec<u8>,
}

/// A mock Thorium API serving a [`World`]
pub struct MockApi {
    /// The url of the mock API
    pub host: String,
    /// The state the API answers from
    pub world: Arc<Mutex<World>>,
    /// Every request the API received in order
    pub requests: Arc<Mutex<Vec<Recorded>>>,
    /// The most entity reads the API answered at once
    pub max_entity_reads: Arc<AtomicUsize>,
    /// The most run token and run models reads the API answered at once
    pub max_run_env_reads: Arc<AtomicUsize>,
}

/// Get the run a run token or run models path names
///
/// # Arguments
///
/// * `path` - The request's path
/// * `suffix` - The route's last path segment
fn run_of(path: &str, suffix: &str) -> Option<Uuid> {
    // the run is the segment before the suffix
    path.strip_suffix(suffix)
        .and_then(|rest| rest.rsplit('/').next())
        .and_then(|id| id.parse::<Uuid>().ok())
}

/// Get the status line of an error status code
///
/// # Arguments
///
/// * `code` - The status code
fn status_line(code: u16) -> &'static str {
    // the codes the tests use
    match code {
        404 => "404 Not Found",
        409 => "409 Conflict",
        _ => "500 Internal Server Error",
    }
}

/// Answer a single request from the world
///
/// # Arguments
///
/// * `world` - The state to answer from
/// * `method` - The request's method
/// * `path` - The request's path without its query
/// * `query` - The request's query
fn answer(world: &World, method: &str, path: &str, query: &str) -> (&'static str, Vec<u8>) {
    // build the response for each route the scaler calls
    match (method, path) {
        ("GET", "/api/system/settings") => ("200 OK", json(&SystemSettings::default())),
        ("GET", "/api/system/stats") => (
            "200 OK",
            json(&world.stats.clone().unwrap_or_else(empty_stats)),
        ),
        ("GET", "/api/users/details/") => ("200 OK", json(&world.users)),
        ("GET", "/api/network-policies/details/") => (
            "200 OK",
            json(&serde_json::json!({"data": world.network_policies})),
        ),
        ("GET", "/api/entities/") => {
            // list the tool entities in the queried groups in a single page
            let groups = query_list(query, "groups[]");
            let lines = world
                .entities
                .values()
                .filter(|entity| entity.kind == EntityKinds::Tool)
                .filter(|entity| entity.groups.iter().any(|group| groups.contains(group)))
                .map(|entity| {
                    serde_json::json!({
                        "groups": entity.groups,
                        "id": entity.id,
                        "name": entity.name,
                        "kind": entity.kind,
                        "created": entity.created,
                    })
                })
                .collect::<Vec<_>>();
            ("200 OK", json(&serde_json::json!({"data": lines})))
        }
        ("GET", "/api/jobs/workloads/runtimes") => ("200 OK", json(&world.runtimes)),
        ("GET", path) if path.starts_with("/api/jobs/deadlines/") => {
            ("200 OK", json(&world.deadlines))
        }
        ("GET", path) if path.ends_with("/run-token") => {
            // answer with the run's token or its error
            match run_of(path, "/run-token")
                .and_then(|run| world.run_tokens.get(&run).map(|token| (run, token)))
            {
                Some((run, Ok(token))) => {
                    let token = ScopedToken {
                        name: format!("run-{run}"),
                        owner: "thorium".to_owned(),
                        role: ScopedTokenRole::User,
                        groups: vec![GROUP.to_owned()],
                        token: token.clone(),
                        token_expiration: Utc::now(),
                        expires: None,
                        run: None,
                    };
                    ("200 OK", json(&token))
                }
                Some((_, Err(code))) => (status_line(*code), br#"{"error":"failed"}"#.to_vec()),
                None => ("404 Not Found", br#"{"error":"not found"}"#.to_vec()),
            }
        }
        ("GET", path) if path.ends_with("/run-models") => {
            // answer with the run's models or their error
            match run_of(path, "/run-models").and_then(|run| world.run_models.get(&run)) {
                Some(Ok(models)) => ("200 OK", json(models)),
                Some(Err(code)) => (status_line(*code), br#"{"error":"failed"}"#.to_vec()),
                None => ("404 Not Found", br#"{"error":"not found"}"#.to_vec()),
            }
        }
        ("POST", path) if path.starts_with("/api/reactions/handle/") => {
            ("202 Accepted", br#"{"status":"Failed"}"#.to_vec())
        }
        ("GET", path) if path.starts_with("/api/reactions/") => {
            // get the reaction this path names
            let id = path
                .rsplit('/')
                .next()
                .and_then(|id| id.parse::<Uuid>().ok());
            match id.and_then(|id| world.reactions.get(&id)) {
                Some(reaction) => ("200 OK", json(reaction)),
                None => ("404 Not Found", br#"{"error":"not found"}"#.to_vec()),
            }
        }
        ("GET", path) if path.starts_with("/api/entities/") => {
            // get the entity this path names
            let id = path
                .trim_start_matches("/api/entities/")
                .parse::<Uuid>()
                .ok();
            match id.and_then(|id| world.entities.get(&id)) {
                Some(entity) => ("200 OK", json(entity)),
                None => ("404 Not Found", br#"{"error":"not found"}"#.to_vec()),
            }
        }
        _ => ("204 No Content", Vec::new()),
    }
}

/// Get the values of a list parameter in a query
///
/// # Arguments
///
/// * `query` - The query without its leading `?`
/// * `key` - The list parameter's key, like `groups[]`
fn query_list(query: &str, key: &str) -> Vec<String> {
    // the client percent-encodes the brackets of list keys
    let encoded = key.replace('[', "%5B").replace(']', "%5D");
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter(|(name, _)| *name == key || *name == encoded)
        .map(|(_, value)| value.to_owned())
        .collect()
}

/// Serialize a value to JSON bytes
///
/// # Arguments
///
/// * `value` - The value to serialize
fn json<T: serde::Serialize>(value: &T) -> Vec<u8> {
    serde_json::to_vec(value).unwrap()
}

impl MockApi {
    /// Start a mock API on a random port
    ///
    /// # Arguments
    ///
    /// * `world` - The state the API answers from
    #[allow(clippy::too_many_lines)]
    pub async fn start(world: World) -> Self {
        // bind to a random local port
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let world = Arc::new(Mutex::new(world));
        let requests: Arc<Mutex<Vec<Recorded>>> = Arc::default();
        let max_entity_reads = Arc::new(AtomicUsize::new(0));
        let max_run_env_reads = Arc::new(AtomicUsize::new(0));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let run_env_in_flight = Arc::new(AtomicUsize::new(0));
        let api = MockApi {
            host: format!("http://{addr}"),
            world: world.clone(),
            requests: requests.clone(),
            max_entity_reads: max_entity_reads.clone(),
            max_run_env_reads: max_run_env_reads.clone(),
        };
        // answer every connection in its own task
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                stream.set_nodelay(true).unwrap();
                let world = world.clone();
                let requests = requests.clone();
                let max_entity_reads = max_entity_reads.clone();
                let in_flight = in_flight.clone();
                let max_run_env_reads = max_run_env_reads.clone();
                let run_env_in_flight = run_env_in_flight.clone();
                tokio::spawn(async move {
                    let mut buf: Vec<u8> = Vec::with_capacity(8192);
                    let mut chunk = vec![0_u8; 65536];
                    loop {
                        // read one request head and body
                        let (head_end, length) = loop {
                            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
                                let length = head
                                    .lines()
                                    .find_map(|l| {
                                        l.strip_prefix("content-length:")
                                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                                    })
                                    .unwrap_or(0);
                                if buf.len() >= end + 4 + length {
                                    break (end, length);
                                }
                            }
                            let Ok(read) = stream.read(&mut chunk).await else {
                                return;
                            };
                            if read == 0 {
                                return;
                            }
                            buf.extend_from_slice(&chunk[..read]);
                        };
                        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                        let body = buf[head_end + 4..head_end + 4 + length].to_vec();
                        buf.drain(..head_end + 4 + length);
                        // get the method and path of this request
                        let mut parts = head.lines().next().unwrap_or_default().split(' ');
                        let method = parts.next().unwrap_or_default().to_owned();
                        let target = parts.next().unwrap_or_default();
                        let (path, query) = target.split_once('?').unwrap_or((target, ""));
                        let (path, query) = (path.to_owned(), query.to_owned());
                        requests.lock().unwrap().push(Recorded {
                            method: method.clone(),
                            path: path.clone(),
                            query: query.clone(),
                            body,
                        });
                        // track how many entity reads are answered at once
                        let entity = path.starts_with("/api/entities/");
                        if entity {
                            let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                            max_entity_reads.fetch_max(now, Ordering::SeqCst);
                            let delay = world.lock().unwrap().entity_delay;
                            tokio::time::sleep(delay).await;
                        }
                        // track how many run env reads are answered at once
                        let run_env = path.ends_with("/run-token") || path.ends_with("/run-models");
                        if run_env {
                            let now = run_env_in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                            max_run_env_reads.fetch_max(now, Ordering::SeqCst);
                            let delay = world.lock().unwrap().run_env_delay;
                            tokio::time::sleep(delay).await;
                        }
                        // build our response
                        let (status, body) = answer(&world.lock().unwrap(), &method, &path, &query);
                        if entity {
                            in_flight.fetch_sub(1, Ordering::SeqCst);
                        }
                        if run_env {
                            run_env_in_flight.fetch_sub(1, Ordering::SeqCst);
                        }
                        let mut out = format!(
                            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        out.extend_from_slice(&body);
                        if stream.write_all(&out).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        api
    }

    /// Count the requests with a method whose path starts with a prefix
    ///
    /// # Arguments
    ///
    /// * `method` - The method to count
    /// * `prefix` - The path prefix to count
    pub fn count(&self, method: &str, prefix: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|req| req.method == method && req.path.starts_with(prefix))
            .count()
    }

    /// Get the requests with a method whose path starts with a prefix
    ///
    /// # Arguments
    ///
    /// * `method` - The method to get
    /// * `prefix` - The path prefix to get
    pub fn matching(&self, method: &str, prefix: &str) -> Vec<Recorded> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|req| req.method == method && req.path.starts_with(prefix))
            .cloned()
            .collect()
    }

    /// Forget every request received so far
    pub fn clear(&self) {
        self.requests.lock().unwrap().clear();
    }

    /// Build a client for this API whose token never needs a refresh
    pub async fn client(&self) -> Thorium {
        // build a client with a placeholder token
        let mut thorium = Thorium::build(&self.host).token("t").build().await.unwrap();
        // push the expiry far out so the client never tries to refresh its token
        thorium.expires = Some(Utc::now() + chrono::Duration::weeks(52));
        thorium
    }
}

/// Build system stats with no jobs
#[must_use]
pub fn empty_stats() -> SystemStats {
    stats(HashMap::new())
}

/// Build system stats from per group workload stats
///
/// # Arguments
///
/// * `groups` - Each group's stats
#[must_use]
pub fn stats(groups: HashMap<String, GroupStats>) -> SystemStats {
    SystemStats {
        deadlines: 0,
        running: 0,
        users: 2,
        k8s: ScalerStats::default(),
        baremetal: ScalerStats::default(),
        external: ScalerStats::default(),
        windows: ScalerStats::default(),
        kvm: ScalerStats::default(),
        groups,
    }
}

/// Build system stats with created jobs for each user and tool instance in one group
///
/// # Arguments
///
/// * `jobs` - The tool instance, user, created jobs, and running jobs of each entry
#[must_use]
pub fn workload_stats(jobs: &[(Uuid, &str, u64, u64)]) -> SystemStats {
    // build the per tool instance user stats
    let mut workloads: HashMap<Uuid, HashMap<String, StageStats>> = HashMap::new();
    for (tool_instance, user, created, running) in jobs {
        let entry = StageStats {
            created: *created,
            running: *running,
            completed: 0,
            failed: 0,
            sleeping: 0,
            total: created + running,
        };
        workloads
            .entry(*tool_instance)
            .or_default()
            .insert((*user).to_owned(), entry);
    }
    let mut groups = HashMap::new();
    groups.insert(GROUP.to_owned(), GroupStats { workloads });
    stats(groups)
}

/// Build a user in some groups
///
/// # Arguments
///
/// * `name` - The username
/// * `groups` - The user's groups
#[must_use]
pub fn user(name: &str, groups: &[&str]) -> ScrubbedUser {
    let value = serde_json::json!({
        "username": name,
        "role": UserRole::User,
        "email": "fake@fake.gov",
        "groups": groups,
        "token": "t",
        "token_expiration": Utc::now() + chrono::Duration::weeks(52),
        "settings": thorium::models::UserSettings::default(),
        "local": true,
        "verified": true,
        "has_image": false,
    });
    serde_json::from_value(value).unwrap()
}

/// Build scheduling settings
///
/// # Arguments
///
/// * `spawn_limit` - The workload's spawn limit
#[must_use]
pub fn scheduling(spawn_limit: SpawnLimits) -> SchedulingSettings {
    SchedulingSettings {
        resources: Resources::new(1000, 1024, 0, 1),
        spawn_limit,
        lifetime: None,
    }
}

/// Build a K8s workload spec
///
/// # Arguments
///
/// * `image` - The container image
/// * `spawn_limit` - The workload's spawn limit
#[must_use]
pub fn k8s_spec(image: &str, spawn_limit: SpawnLimits) -> WorkloadSpec {
    WorkloadSpec {
        environment: WorkloadEnvironment::K8s(K8sEnvironment {
            image: image.to_owned(),
            scheduling: scheduling(spawn_limit),
            ..K8sEnvironment::default()
        }),
        ..WorkloadSpec::default()
    }
}

/// Build a `BareMetal` workload spec
#[must_use]
pub fn bare_metal_spec() -> WorkloadSpec {
    WorkloadSpec {
        environment: WorkloadEnvironment::BareMetal(BareMetalEnvironment {
            scheduling: scheduling(SpawnLimits::Unlimited),
            ..BareMetalEnvironment::default()
        }),
        ..WorkloadSpec::default()
    }
}

/// Build a tool instance entity running a workload
///
/// # Arguments
///
/// * `id` - The tool instance's id
/// * `tool` - The tool's name
/// * `version` - The version label
/// * `groups` - The tool instance's groups
/// * `spec` - The workload the tool instance runs
#[must_use]
pub fn tool_instance(
    id: Uuid,
    tool: &str,
    version: &str,
    groups: &[&str],
    spec: WorkloadSpec,
) -> Entity {
    Entity {
        id,
        name: format!("{tool}@{version}"),
        kind: EntityKinds::ToolInstance,
        metadata: EntityMetadata::ToolInstance(ToolInstanceEntity {
            tool: Uuid::new_v4(),
            version: ToolVersion::from(version),
            previous: None,
            spec_hash: String::new(),
            deprecated: false,
            spec: ToolSpec::Image(spec),
            portability: None,
            provides: Vec::new(),
        }),
        description: None,
        submitter: "thorium".to_owned(),
        groups: groups.iter().map(|group| (*group).to_owned()).collect(),
        tags: HashMap::new(),
        image: None,
        created: Utc::now(),
    }
}

/// Build a tool entity as the mock API lists it
///
/// Listings only read a tool's id, name, kind, groups, and creation time, so its metadata is
/// a tool instance's.
///
/// # Arguments
///
/// * `id` - The tool's id
/// * `name` - The tool's name
/// * `groups` - The tool's groups
#[must_use]
pub fn tool(id: Uuid, name: &str, groups: &[&str]) -> Entity {
    // a tool instance entity renamed and re-kinded as a tool
    let spec = k8s_spec("registry.example/tool:1", SpawnLimits::Unlimited);
    let mut entity = tool_instance(id, name, "1.0", groups, spec);
    entity.name = name.to_owned();
    entity.kind = EntityKinds::Tool;
    entity
}

/// Build a Harness tool instance entity running a workload
///
/// # Arguments
///
/// * `id` - The tool instance's id
/// * `tool` - The tool's name
/// * `groups` - The tool instance's groups
/// * `spec` - The workload the harness runs
#[must_use]
pub fn harness_instance(id: Uuid, tool: &str, groups: &[&str], spec: WorkloadSpec) -> Entity {
    // a tool instance whose spec is a harness around the workload
    let mut entity = tool_instance(id, tool, "1.0.0", groups, spec.clone());
    if let EntityMetadata::ToolInstance(instance) = &mut entity.metadata {
        instance.spec = ToolSpec::Harness(thorium::models::HarnessSpec {
            workload: spec,
            ..thorium::models::HarnessSpec::default()
        });
    }
    entity
}

/// Build a deadline for a job
///
/// # Arguments
///
/// * `user` - The job's creator
/// * `group` - The job's group
/// * `tool_instance` - The tool instance the job runs
/// * `run` - The run the job belongs to, if any
/// * `age` - How many seconds ago the deadline was
#[must_use]
pub fn deadline(
    user: &str,
    group: &str,
    tool_instance: Uuid,
    run: Option<Uuid>,
    age: i64,
) -> Deadline {
    Deadline {
        group: group.to_owned(),
        pipeline: "harvest".to_owned(),
        stage: "husk".to_owned(),
        creator: user.to_owned(),
        job_id: Uuid::new_v4(),
        reaction: run.unwrap_or_else(Uuid::new_v4),
        deadline: Utc::now() - chrono::Duration::seconds(age),
        tool_instance,
        run,
    }
}

/// A mock crane that records every image it inspects
pub struct MockCrane {
    /// The path to the mock crane script
    pub path: PathBuf,
    /// The file each inspected image is appended to
    pub log: PathBuf,
}

impl MockCrane {
    /// Write a mock crane script that fails to inspect any image containing `broken`
    #[must_use]
    pub fn new() -> Self {
        // build a unique directory for this crane
        let dir = std::env::temp_dir().join(format!("scaler-crane-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("crane");
        let log = dir.join("inspected.log");
        let script = format!(
            "#!/bin/sh\nimage=\"$2\"\nif [ \"$1\" = \"--insecure\" ]; then image=\"$3\"; fi\necho \"$image\" >> \"{}\"\ncase \"$image\" in *broken*) echo \"cannot inspect $image\" >&2; exit 1;; esac\necho '{{\"architecture\":\"amd64\",\"os\":\"linux\",\"config\":{{\"Entrypoint\":[\"/bin/run\"],\"Cmd\":[\"--fast\"]}}}}'\n",
            log.display()
        );
        std::fs::write(&path, script).unwrap();
        // make the script executable
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        MockCrane { path, log }
    }

    /// Get every image inspected so far in order
    #[must_use]
    pub fn inspected(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

/// Load the test config with one K8s cluster and a mock crane
///
/// # Arguments
///
/// * `crane` - The mock crane to inspect images with
#[must_use]
pub fn conf(crane: &MockCrane) -> Conf {
    // the test config with one K8s cluster (the dry-run scheduler gives it 3 nodes)
    let mut conf = Conf::new("../api/tests/thorium-testing.yml").unwrap();
    let cluster = serde_json::from_value(serde_json::json!({"alias": null})).unwrap();
    conf.thorium.scaler.k8s.clusters.clear();
    conf.thorium
        .scaler
        .k8s
        .clusters
        .insert("cluster0".to_owned(), cluster);
    conf.thorium.scaler.crane.path.clone_from(&crane.path);
    conf.thorium.scaler.crane.insecure = false;
    conf
}

/// Build a dry-run scaler against a mock API
///
/// # Arguments
///
/// * `api` - The mock API
/// * `conf` - The config to use
/// * `scaler` - The type of scaler to build
pub async fn scaler(api: &MockApi, conf: Conf, scaler: ImageScaler) -> Scaler {
    // build a dry-run scaler that loads its cache from the mock API
    let thorium = Arc::new(api.client().await);
    let mut built = Scaler::build(
        conf,
        "unused-keys.yml".to_owned(),
        thorium,
        scaler,
        true,
        &"ctx".to_owned(),
    )
    .await
    .unwrap();
    // rebuild the allocatable so fair share knows the cached users, then load cluster resources
    let settings = built.cache.settings.clone();
    built.allocatable = Allocatable::new(scaler, &built.conf, &settings, &built.cache);
    built.init().await.unwrap();
    built
}

/// Build a cache for a scaler type without loading it from an API
///
/// # Arguments
///
/// * `api` - The mock API the cache's client talks to
/// * `conf` - The config to use
/// * `scaler` - The scaler the cache is for
/// * `users` - The users and groups to cache
pub async fn cache(api: &MockApi, conf: Conf, scaler: ImageScaler, users: &[&str]) -> Cache {
    // build an empty cache
    let mut cache = Cache::blank(
        Arc::new(api.client().await),
        conf,
        "unused".to_owned(),
        scaler,
    );
    // cache our users in our group
    for name in users {
        cache.users.insert((*name).to_owned(), user(name, &[GROUP]));
    }
    cache.groups.insert(GROUP.to_owned());
    cache
}

/// Get the requisitions spawned in a requisition map by their order of deadline
///
/// # Arguments
///
/// * `spawns` - The spawns by cluster and deadline
#[must_use]
pub fn spawned_reqs(
    spawns: &hashbrown::HashMap<String, BTreeMap<DateTime<Utc>, Vec<crate::libs::Spawned>>>,
) -> Vec<thorium::models::Requisition> {
    // walk the spawns of every cluster by deadline
    let mut all = spawns
        .values()
        .flat_map(|by_deadline| by_deadline.iter())
        .flat_map(|(deadline, spawned)| {
            spawned
                .iter()
                .map(move |spawn| (*deadline, spawn.req.clone()))
        })
        .collect::<Vec<_>>();
    all.sort_by_key(|(deadline, _)| *deadline);
    all.into_iter().map(|(_, req)| req).collect()
}
