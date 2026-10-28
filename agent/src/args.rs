use clap::Parser;
use serde::Deserialize;
use thorium::models::ImageScaler;
use thorium::{Error, Thorium};
use tracing::{Level, event, instrument};
use uuid::Uuid;

use crate::libs::{Target, Workload};

/// Command line args
#[derive(Parser, Debug, Deserialize, Clone)]
#[clap(version, author)]
pub struct Args {
    /// The environment specific args for this agent
    #[clap(subcommand)]
    pub env: Envs,
    /// The cluster this worker is in
    #[clap(short, long)]
    pub cluster: String,
    /// The node this worker is on
    #[clap(long)]
    pub node: Option<String>,
    /// The group to claim jobs from
    #[clap(short, long)]
    pub group: String,
    /// The tool instance to claim jobs for
    #[clap(long)]
    pub tool_instance: Uuid,
    /// The name of this worker
    #[clap(short, long)]
    pub name: String,
    /// The path to the tracing config to load
    #[clap(short, long, default_value = "/opt/thorium/tracing.yml")]
    pub trace: String,
    /// The keys to use when authenticating to Thorium (`keys.yml` if not set); a worker launched
    /// without them builds its client from `THORIUM_URL` and `THORIUM_TOKEN` when both are set
    #[clap(short, long)]
    pub keys: Option<String>,
    /// How long should this agent sit limbo before exiting without a job to work on
    #[clap(short, long, default_value = "5")]
    pub limbo: usize,
}

impl Args {
    /// Get the path to the keys this worker authenticates with when it has no run env
    #[must_use]
    pub fn keys_path(&self) -> &str {
        // workers launched without keys read them from the working directory
        self.keys.as_deref().unwrap_or("keys.yml")
    }

    /// Get the tool instance this worker runs and the user and worker it runs as
    ///
    /// # Arguments
    ///
    /// * `thorium` - The client to fetch the tool instance, user, and worker with
    #[instrument(name = "Args::target", skip_all, fields(tool_instance = self.tool_instance.to_string()))]
    pub async fn target(&self, thorium: &Thorium) -> Result<Target, Error> {
        // get the tool instance this worker runs and project its workload into an image
        let workload = Workload::fetch(thorium, self.tool_instance).await?;
        let image = workload.project(&self.group);
        // get our current users info
        let user = thorium.users.info().await?;
        // get the name of this worker
        let name = self.name.clone();
        // get our workers info
        let worker = thorium.system.get_worker(&name).await?;
        // build the target object
        let target = Target {
            name,
            group: self.group.clone(),
            tool_instance: self.tool_instance,
            workload,
            image,
            user,
            thorium: thorium.clone(),
            pool: worker.pool,
            run: worker.run,
        };
        // log this new target
        event!(
            Level::INFO,
            name = target.name,
            user = target.user.username,
            group = target.group,
            tool_instance = target.tool_instance.to_string(),
            tool = target.image.name,
            pool = target.pool.as_str(),
        );
        Ok(target)
    }

    /// Get the current scaler we are running under
    pub fn scaler(&self) -> ImageScaler {
        match self.env {
            Envs::K8s(_) => ImageScaler::K8s,
            Envs::BareMetal(_) => ImageScaler::BareMetal,
            Envs::Windows(_) => ImageScaler::Windows,
            Envs::Kvm(_) => ImageScaler::Kvm,
        }
    }

    /// Get our nodes hostname
    pub fn node(&self) -> Result<String, Error> {
        // if we have a node specified in our args then use that
        match &self.node {
            Some(node) => Ok(node.to_owned()),
            None => {
                // get our hostname since our args don't specify it
                match gethostname::gethostname().into_string() {
                    Ok(hostname) => Ok(hostname),
                    Err(err) => Err(Error::new(format!(
                        "Failed to get hostname with {:#?}",
                        err
                    ))),
                }
            }
        }
    }
}

/// The different environments this agent is executing in
#[derive(Parser, Debug, Deserialize, Clone)]
pub enum Envs {
    /// This agent is running in k8s
    #[clap(version, author)]
    K8s(K8s),
    /// This agent is running on bare metal
    #[clap(version, author)]
    BareMetal(BareMetal),
    /// This agent is running in k8s
    #[clap(version, author)]
    Windows(Windows),
    /// This agent is running in a kvm vm
    #[clap(version, author)]
    Kvm(Kvm),
}

impl Envs {
    /// Get our env name as a str
    pub fn kind(&self) -> &str {
        match self {
            Envs::K8s(_) => "K8s",
            Envs::BareMetal(_) => "BareMetal",
            Envs::Windows(_) => "Windows",
            Envs::Kvm(_) => "Kvm",
        }
    }
}

/// The args for running the agent in K8s
#[derive(Parser, Debug, Deserialize, Clone)]
#[clap(version, author)]
pub struct K8s {
    /// The original entrypoint for this container
    #[clap(short, long)]
    pub entrypoint: String,
    /// The original command for this container
    #[clap(short = 'm', long)]
    pub cmd: String,
}

/// The args for running the agent in K8s
#[derive(Parser, Debug, Deserialize, Clone)]
#[clap(version, author)]
pub struct BareMetal {}

/// The args for running the agent in Windows
#[derive(Parser, Debug, Deserialize, Clone)]
#[clap(version, author)]
pub struct Windows {
    /// The original entrypoint for this container
    #[clap(short, long)]
    pub entrypoint: String,
    /// The original command for this container
    #[clap(short = 'm', long)]
    pub cmd: String,
    /// The name of this worker
    #[clap(short, long)]
    pub name: String,
}

/// The args for running the agent in an KVM based vm
#[derive(Parser, Debug, Deserialize, Clone)]
#[clap(version, author)]
pub struct Kvm {}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use thorium::Thorium;
    use thorium::models::ToolSpec;
    use thorium::models::runtime::DEFAULT_PROJECTED_RUNTIME;
    use uuid::Uuid;

    use super::Args;
    use crate::libs::test_server::{self, MockThorium};

    /// Build the command line of a bare metal worker with some extra flags
    ///
    /// # Arguments
    ///
    /// * `extra` - The flags to add before the environment subcommand
    fn command_line(extra: &[&str]) -> Vec<String> {
        // the flags every worker takes
        let mut line = vec!["thorium-agent", "--cluster", "cluster0", "--node", "node0"];
        line.extend(["--group", "corn", "--name", "worker0", "--keys", "keys.yml"]);
        line.extend(extra);
        line.push("bare-metal");
        line.into_iter().map(str::to_owned).collect()
    }

    /// A worker takes a required tool instance and no pipeline or stage
    #[test]
    fn tool_instance_required() {
        // a tool instance id parses
        let id = Uuid::new_v4().to_string();
        let args = Args::try_parse_from(command_line(&["--tool-instance", &id])).unwrap();
        assert_eq!(args.tool_instance.to_string(), id);
        // a worker without a tool instance is refused
        let err = Args::try_parse_from(command_line(&[])).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        assert!(err.to_string().contains("--tool-instance"), "{err}");
        // a tool instance that isn't a UUID is refused
        let err =
            Args::try_parse_from(command_line(&["--tool-instance", "harvester"])).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
        // the pipeline and stage flags are refused, with or without a tool instance
        for flag in ["--pipeline", "--stage"] {
            let err = Args::try_parse_from(command_line(&["--tool-instance", &id, flag, "x"]))
                .unwrap_err();
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::UnknownArgument,
                "{flag}"
            );
            let err = Args::try_parse_from(command_line(&[flag, "x"])).unwrap_err();
            assert_ne!(err.kind(), clap::error::ErrorKind::DisplayHelp, "{flag}");
        }
    }

    /// The target of a worker is its tool instance, read once and projected into an image
    #[tokio::test]
    async fn target_reads_the_tool_instance_once() {
        // a mock API serving one tool instance, the user, and the worker
        let id = Uuid::new_v4();
        let entity = test_server::tool_instance_entity(
            id,
            "harvester",
            "1.2.0",
            ToolSpec::Image(test_server::k8s_spec()),
        );
        let entity_json = serde_json::to_string(&entity).unwrap();
        let worker_json = test_server::worker_json("worker0", id);
        let entity_path = format!("GET /api/entities/{id} ");
        let mock = MockThorium::start(move |line| {
            if line.starts_with(&entity_path) {
                (200, entity_json.clone())
            } else if line.starts_with("GET /api/users/whoami ") {
                (200, test_server::user_json())
            } else if line.starts_with("GET /api/system/worker/worker0 ") {
                (200, worker_json.clone())
            } else {
                (500, "{}".to_owned())
            }
        })
        .await;
        let thorium = Thorium::build(&mock.host)
            .token("token")
            .build()
            .await
            .unwrap();
        // build the worker's target
        let args =
            Args::try_parse_from(command_line(&["--tool-instance", &id.to_string()])).unwrap();
        let target = args.target(&thorium).await.unwrap();
        // the tool instance was read once, the user and worker were read, and no image was read
        let lines = mock.lines();
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains("/api/entities/"))
                .count(),
            1,
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("/api/images/")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("/api/users/whoami")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("/api/system/worker/worker0")),
            "{lines:?}"
        );
        // the image is the workload's projection under the group and the tool's name
        let expected =
            test_server::k8s_spec().project_image("corn", "harvester", DEFAULT_PROJECTED_RUNTIME);
        assert_eq!(
            serde_json::to_value(&target.image).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        assert_eq!(target.tool_instance, id);
        assert_eq!(target.workload.version.to_string(), "1.2.0");
        assert!(target.workload.harness.is_none());
        // the target prints its group, tool instance, and worker name
        assert_eq!(target.to_string(), format!("corn:{id} - worker0"));
    }

    /// A worker whose tool instance can't be read exits with an error naming it and claims nothing
    #[tokio::test]
    async fn missing_tool_instance_ends_the_agent() {
        // a mock API that has no tool instances
        let mock = MockThorium::start(|line| {
            if line.starts_with("GET /api/entities/") {
                (404, r#"{"error":"entity not found"}"#.to_owned())
            } else if line.starts_with("GET /api/users/whoami ") {
                (200, test_server::user_json())
            } else {
                (200, "[]".to_owned())
            }
        })
        .await;
        // a keys file pointing at the mock API
        let dir = std::env::temp_dir().join(format!("thorium-agent-{}", Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let keys = dir.join("keys.yml");
        tokio::fs::write(&keys, format!("api: {}\ntoken: token\n", mock.host))
            .await
            .unwrap();
        // launch the agent for a tool instance that doesn't exist
        let id = Uuid::new_v4();
        let mut args =
            Args::try_parse_from(command_line(&["--tool-instance", &id.to_string()])).unwrap();
        args.keys = Some(keys.to_string_lossy().into_owned());
        let err = crate::launch(args).await.unwrap_err();
        tokio::fs::remove_dir_all(&dir).await.unwrap();
        // the error names the tool instance and nothing was claimed
        let msg = err.to_string();
        assert!(msg.contains(&id.to_string()), "{msg}");
        let lines = mock.lines();
        assert!(
            !lines.iter().any(|line| line.contains("claim")),
            "{lines:?}"
        );
    }
}
