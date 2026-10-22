//! Test support: a mock Thorium API that records every request it answers, and fixtures

use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// The answer to one request: a status code and a json body
pub type Answer = (u16, String);

/// Picks the answer to a request from its request line (`METHOD /path HTTP/1.1`)
pub type Router = Arc<dyn Fn(&str) -> Answer + Send + Sync>;

/// A running mock Thorium API
pub struct MockThorium {
    /// The url the mock API is listening at
    pub host: String,
    /// Every request the mock API received, in order, head and body
    pub requests: Arc<Mutex<Vec<String>>>,
}

impl MockThorium {
    /// Start a mock Thorium API that answers each request with its router
    ///
    /// # Arguments
    ///
    /// * `router` - Picks the answer to each request
    pub async fn start<F>(router: F) -> Self
    where
        F: Fn(&str) -> Answer + Send + Sync + 'static,
    {
        // bind to a random local port
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // track the requests we receive
        let requests = Arc::new(Mutex::new(Vec::new()));
        let inner = requests.clone();
        let router: Router = Arc::new(router);
        // answer requests in the background
        tokio::spawn(async move {
            loop {
                // accept the next connection
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                // read until the request head and its whole body have arrived
                let text = read_request(&mut stream).await;
                // record this request and pick its answer from the request line
                let line = text.lines().next().unwrap_or_default().to_owned();
                inner.lock().unwrap().push(text);
                let (status, body) = router(&line);
                // send the answer and close the connection
                let resp = format!(
                    "HTTP/1.1 {status} Mock\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        MockThorium {
            host: format!("http://{addr}"),
            requests,
        }
    }

    /// Get the request lines this mock API has received, in order
    pub fn lines(&self) -> Vec<String> {
        // keep only the first line of each request
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|req| req.lines().next().unwrap_or_default().to_owned())
            .collect()
    }

    /// Get every whole request whose request line contains a pattern
    ///
    /// # Arguments
    ///
    /// * `pattern` - The text the request line must contain
    pub fn matching(&self, pattern: &str) -> Vec<String> {
        // keep requests whose first line holds the pattern
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|req| req.lines().next().unwrap_or_default().contains(pattern))
            .cloned()
            .collect()
    }
}

/// Read one whole request, head and body, from a connection
///
/// # Arguments
///
/// * `stream` - The connection to read from
async fn read_request(stream: &mut tokio::net::TcpStream) -> String {
    // read chunks until the head is complete and the body has arrived
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        // read the next chunk of this request
        let read = stream.read(&mut chunk).await.unwrap_or(0);
        buf.extend_from_slice(&chunk[..read]);
        // check whether the head is complete and the body has arrived
        let text = String::from_utf8_lossy(&buf).to_string();
        let done = text.find("\r\n\r\n").is_some_and(|end| {
            // get the headers of this request
            let head = text[..end].to_lowercase();
            // a chunked body ends with an empty chunk
            if head.contains("transfer-encoding: chunked") {
                return text.ends_with("\r\n0\r\n\r\n");
            }
            // otherwise the body is as long as its content length
            let length = head
                .lines()
                .find_map(|line| {
                    line.strip_prefix("content-length:")
                        .map(|len| len.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            buf.len() >= end + 4 + length
        });
        // stop once the request is complete or the client closed the connection
        if done || read == 0 {
            return text;
        }
    }
}

/// Build a K8s workload spec that runs a fixed image
pub fn k8s_spec() -> thorium::models::WorkloadSpec {
    // a K8s workload with a fixed image and every other setting at its default
    thorium::models::WorkloadSpec {
        environment: thorium::models::WorkloadEnvironment::K8s(thorium::models::K8sEnvironment {
            image: "registry.local/harvester:1".to_owned(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Build a tool instance entity named after its tool and version
///
/// # Arguments
///
/// * `id` - The id of the tool instance
/// * `tool` - The name of the tool
/// * `version` - The version label of the tool instance
/// * `spec` - The spec of the tool instance
pub fn tool_instance_entity(
    id: uuid::Uuid,
    tool: &str,
    version: &str,
    spec: thorium::models::ToolSpec,
) -> thorium::models::Entity {
    // instances are named `{tool}@{version}`
    let version = thorium::models::ToolVersion::from(version);
    thorium::models::Entity {
        id,
        name: format!("{tool}@{version}"),
        kind: thorium::models::EntityKinds::ToolInstance,
        metadata: thorium::models::EntityMetadata::ToolInstance(
            thorium::models::ToolInstanceEntity {
                tool: uuid::Uuid::new_v4(),
                version,
                previous: None,
                spec_hash: "hash".to_owned(),
                deprecated: false,
                spec,
                portability: None,
                provides: Vec::new(),
            },
        ),
        description: None,
        submitter: "mcarson".to_owned(),
        groups: vec!["corn".to_owned()],
        tags: std::collections::HashMap::default(),
        image: None,
        created: chrono::Utc::now(),
    }
}

/// Build the workload of a tool instance the way a worker reads it
///
/// # Arguments
///
/// * `harness` - Where the harness writes its outputs, for a Harness tool instance
pub fn workload(harness: Option<thorium::models::HarnessOutputs>) -> super::Workload {
    // a K8s tool named harvester at version 1.2.0
    super::Workload {
        tool: "harvester".to_owned(),
        version: thorium::models::ToolVersion::from("1.2.0"),
        spec: k8s_spec(),
        harness: harness.map(|outputs| super::target::Harness {
            interface: thorium::models::HarnessInterface {
                outputs,
                ..thorium::models::HarnessInterface::default()
            },
            orphaned_runs: thorium::models::OrphanedRunPolicy::Delete,
        }),
        dependency_names: std::collections::BTreeMap::new(),
    }
}

/// Build the json body of a `whoami` response for a plain user
pub fn user_json() -> String {
    // a plain user in the corn group
    let user = thorium::models::ScrubbedUser {
        username: "mcarson".to_owned(),
        role: thorium::models::UserRole::User,
        email: "mcarson@example.com".to_owned(),
        groups: vec!["corn".to_owned()],
        actual_groups: Vec::new(),
        token: "token".to_owned(),
        token_expiration: chrono::Utc::now(),
        unix: None,
        settings: thorium::models::UserSettings::default(),
        local: true,
        verified: true,
        has_image: false,
    };
    serde_json::to_string(&user).unwrap()
}

/// Build the json body of a worker read for a registered worker
///
/// # Arguments
///
/// * `name` - The name of the worker
/// * `tool_instance` - The tool instance the worker runs
pub fn worker_json(name: &str, tool_instance: uuid::Uuid) -> String {
    // the worker as the API stores it
    serde_json::json!({
        "cluster": "cluster0",
        "node": "node0",
        "scaler": "BareMetal",
        "name": name,
        "user": "mcarson",
        "group": "corn",
        "pipeline": "harvest",
        "stage": "harvester",
        "tool_instance": tool_instance,
        "status": "Spawning",
        "spawned": chrono::Utc::now(),
        "heart_beat": null,
        "resources": thorium::models::Resources::default(),
        "pool": "Deadline",
        "active": null,
    })
    .to_string()
}

/// Build a reaction as the API returns it
///
/// # Arguments
///
/// * `id` - The id of the reaction
/// * `status` - The status of the reaction
/// * `parent` - The reaction this is a sub reaction of
pub fn reaction(
    id: uuid::Uuid,
    status: thorium::models::ReactionStatus,
    parent: Option<uuid::Uuid>,
) -> thorium::models::Reaction {
    // a direct tool run of the harvester tool on one file in the corn group
    let tool_instance = uuid::Uuid::new_v4();
    thorium::models::Reaction {
        id,
        group: "corn".to_owned(),
        creator: "mcarson".to_owned(),
        pipeline: "harvester".to_owned(),
        status,
        current_stage: 0,
        current_stage_progress: 0,
        current_stage_length: 1,
        args: std::collections::HashMap::default(),
        sla: chrono::Utc::now(),
        jobs: Vec::new(),
        tags: Vec::new(),
        parent,
        sub_reactions: 0,
        completed_sub_reactions: 0,
        generators: Vec::new(),
        samples: vec![SHA256.to_owned()],
        ephemeral: Vec::new(),
        parent_ephemeral: std::collections::HashMap::default(),
        repos: Vec::new(),
        trigger_depth: None,
        has_cache: false,
        source: thorium::models::ReactionSource::Tool {
            tool_instance,
            provenance: thorium::models::RunProvenance::default(),
        },
        plan: thorium::models::ReactionPlan {
            stages: vec![vec![thorium::models::PlannedStage {
                name: "harvester".to_owned(),
                tool_instance,
            }]],
        },
        run_token: None,
        run: parent,
    }
}

/// The SHA256 of the file test tool runs are given
pub const SHA256: &str = "63b9c2cbd4f4f1f6c2a0c2a1d7e0d2e6c2b6a8a3b1c4d5e6f7a8b9c0d1e2f3a4";

/// Build a fresh, empty scratch directory
pub fn scratch() -> std::path::PathBuf {
    // a uniquely named directory under the temp dir
    let dir = std::env::temp_dir().join(format!("thorium-agent-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Build a running job of a tool instance in the corn group with one input file
///
/// # Arguments
///
/// * `tool_instance` - The tool instance the job runs
pub fn job(tool_instance: uuid::Uuid) -> thorium::models::GenericJob {
    // a job with one file and no repos or ephemeral files
    thorium::models::GenericJob {
        reaction: uuid::Uuid::new_v4(),
        id: uuid::Uuid::new_v4(),
        group: "corn".into(),
        pipeline: "harvest".into(),
        stage: "harvester".into(),
        creator: "mcarson".into(),
        args: thorium::models::GenericJobArgs::default(),
        status: thorium::models::JobStatus::Running,
        deadline: chrono::Utc::now(),
        parent: None,
        generator: false,
        samples: vec![SHA256.to_owned()],
        ephemeral: Vec::new(),
        parent_ephemeral: std::collections::HashMap::default(),
        repos: Vec::new(),
        trigger_depth: None,
        tool_instance,
        run: None,
    }
}
