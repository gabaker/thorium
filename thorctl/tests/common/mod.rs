//! Shared helpers for running the thorctl binary against the in-process test API

#![allow(dead_code)]

use std::path::PathBuf;
use std::process::{Output, Stdio};
use std::time::Duration;

use thorium::client::{ClientSettings, Users};
use thorium::models::{
    GroupRequest, GroupUsersRequest, ImageScaler, Pipeline, Pools, Reaction, Resources, UserCreate,
    UserRole, WorkerRegistrationList,
};
use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium};
use tokio::process::{Child, Command};
use uuid::Uuid;

/// A user of the test API with a keys file thorctl can authenticate with
pub struct TestUser {
    /// The user's name
    pub username: String,
    /// The user's password
    pub password: String,
    /// A client authenticated as this user
    pub client: Thorium,
    /// An admin client for the same API
    pub admin: Thorium,
    /// The keys file thorctl authenticates with
    pub keys: PathBuf,
    /// A group this user owns
    pub group: String,
}

/// Build a unique lowercase name with a prefix
///
/// # Arguments
///
/// * `prefix` - The prefix of the name
pub fn unique(prefix: &str) -> String {
    format!("{prefix}{}", Uuid::new_v4().simple())
}

impl TestUser {
    /// Create a developer, a group it owns, and a keys file for it
    ///
    /// # Arguments
    ///
    /// * `claim` - A label for the test, which names what it checks
    pub async fn new(claim: &str) -> Result<Self, Error> {
        // create the user and group under random names
        Self::named(claim, &unique("ctl"), &unique("ctlgroup")).await
    }

    /// Create a developer and a group it owns under readable names, and a keys file for it
    ///
    /// # Arguments
    ///
    /// * `_claim` - A label for the test, which names what it checks
    /// * `username` - The name of the developer
    /// * `group` - The name of the group the developer owns
    pub async fn named(_claim: &str, username: &str, group: &str) -> Result<Self, Error> {
        // start the test API if needed and get an admin client
        let admin = test_utilities::admin_client().await?;
        // create a developer with a known password
        let username = username.to_owned();
        let password = unique("pw");
        let create = UserCreate::new(&username, &password, "fake@fake.gov")
            .skip_verification()
            .role(UserRole::Developer {
                k8s: true,
                bare_metal: true,
                windows: true,
                kvm: false,
                external: true,
            });
        let secret_key = Some(&test_utilities::CONF.thorium.secret_key);
        Users::create(&admin.host, create, secret_key, &ClientSettings::default()).await?;
        // build a client for our user
        let client = Thorium::build(&admin.host)
            .basic_auth(&username, &password)
            .build()
            .await?;
        // create a group owned by the admin and our user
        let group = group.to_owned();
        let owners = GroupUsersRequest::default()
            .direct("thorium")
            .direct(&username);
        admin
            .groups
            .create(&GroupRequest::new(&group).owners(owners))
            .await?;
        // write a keys file for our user
        let dir = std::env::temp_dir().join(unique("thorctl-test-"));
        std::fs::create_dir_all(&dir)?;
        let keys = dir.join("keys.yml");
        let contents = format!(
            "api: {}\nusername: {username}\npassword: {password}\n",
            admin.host
        );
        std::fs::write(&keys, contents)?;
        Ok(TestUser {
            username,
            password,
            client,
            admin,
            keys,
            group,
        })
    }

    /// Build a thorctl command authenticated as this user
    ///
    /// # Arguments
    ///
    /// * `args` - The thorctl arguments after the global flags
    pub fn command(&self, args: &[&str]) -> Command {
        // build a command running the thorctl binary with our keys file
        let mut command = Command::new(env!("CARGO_BIN_EXE_thorctl"));
        command
            .arg("--keys")
            .arg(&self.keys)
            .arg("--skip-update")
            .args(args)
            .env("NO_PROXY", "127.0.0.1,localhost,::1")
            .env("no_proxy", "127.0.0.1,localhost,::1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    /// Run thorctl to completion and return its output
    ///
    /// # Arguments
    ///
    /// * `_name` - A label for the run, which names what it checks
    /// * `args` - The thorctl arguments after the global flags
    pub async fn run(&self, _name: &str, args: &[&str]) -> Output {
        // run thorctl, giving up if it hangs
        tokio::time::timeout(Duration::from_secs(120), self.command(args).output())
            .await
            .expect("thorctl timed out")
            .expect("failed to run thorctl")
    }

    /// Start thorctl without waiting for it to finish
    ///
    /// # Arguments
    ///
    /// * `args` - The thorctl arguments after the global flags
    pub fn spawn(&self, args: &[&str]) -> Child {
        self.command(args).spawn().expect("failed to start thorctl")
    }

    /// Get a raw API response body as this user
    ///
    /// # Arguments
    ///
    /// * `path` - The API path to get, starting with `/api/`
    pub async fn raw_get(&self, path: &str) -> serde_json::Value {
        // send our request with basic auth
        let body = reqwest::Client::new()
            .get(format!("{}{path}", self.admin.host))
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .await
            .expect("raw request failed")
            .error_for_status()
            .expect("raw request returned an error")
            .text()
            .await
            .expect("raw response had no body");
        serde_json::from_str(&body).expect("raw response was not JSON")
    }

    /// Create a pipeline and one reaction for it in our group
    pub async fn reaction(&self) -> Result<(Pipeline, Reaction), Error> {
        // create a random pipeline as our user
        let pipe_req = generators::pipelines(&self.group, 1, false, &self.client)
            .await?
            .remove(0);
        let pipe = self
            .client
            .pipelines
            .get(&self.group, &pipe_req.name)
            .await?;
        // create a reaction for it as our user
        let req = generators::gen_reaction(&self.group, &pipe, None);
        let created = self.client.reactions.create(&req).await?;
        let reaction = self.client.reactions.get(&self.group, created.id).await?;
        Ok((pipe, reaction))
    }

    /// Complete every job of a reaction as a worker for our user, adding stage logs
    ///
    /// # Arguments
    ///
    /// * `pipe` - The reaction's pipeline
    pub async fn complete(&self, pipe: &Pipeline) -> Result<(), Error> {
        // register a node for our worker
        let cluster = unique("cluster");
        let node = unique("node");
        generators::node(&cluster, &node, Resources::default(), &self.admin).await?;
        // claim and finish a job for every stage
        for stage in pipe.order.iter().flatten() {
            // register a worker for this stage that runs our user's jobs
            let worker = unique("worker");
            let workers = WorkerRegistrationList::default().add(
                &cluster,
                &node,
                &worker,
                &self.username,
                &self.group,
                generators::stage_instance(pipe, stage),
                None,
                Resources::default(),
                Pools::Deadline,
            );
            self.admin
                .system
                .register_workers(ImageScaler::K8s, &workers)
                .await?;
            // claim this stage's job as our user
            let tool_instance = generators::stage_instance(pipe, stage);
            let jobs = self
                .client
                .jobs
                .claim_workload(&self.group, tool_instance, &cluster, &node, &worker, 1)
                .await?;
            // finish it with random logs as the worker that claimed it
            let logs = generators::stage_logs();
            self.client
                .jobs
                .proceed(&jobs[0], &logs, 2, &worker)
                .await?;
            // remove our worker
            generators::delete_worker(&worker, &self.admin).await?;
        }
        Ok(())
    }
}

/// Get a command's stdout as a string
///
/// # Arguments
///
/// * `output` - The command's output
pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Assert that a command exited 0, printing its stderr when it didn't
///
/// # Arguments
///
/// * `output` - The command's output
pub fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "thorctl failed with {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Parse a command's stdout as JSON
///
/// # Arguments
///
/// * `output` - The command's output
pub fn json(output: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(output)).expect("stdout was not JSON")
}
