//! A Thorium cluster thoradm runs against: one named in the environment, or the harness's own

use super::harness::Harness;
use std::path::PathBuf;
use std::process::Command;
use thorium::{Conf, Thorium};
use uuid::Uuid;

/// The variable naming the cluster config of the cluster to test
pub const CLUSTER_CONF_VAR: &str = "THORADM_E2E_CLUSTER_CONF";

/// The variable naming the Thorctl config that holds an admin's token for the cluster's API
pub const CTL_CONF_VAR: &str = "THORADM_E2E_CTL_CONF";

/// The variable naming the directory the backup goes in
pub const DIR_VAR: &str = "THORADM_E2E_DIR";

/// The variable that, set to `1`, runs the test on the test harness's namespace instead
pub const HARNESS_VAR: &str = "THORADM_E2E_HARNESS";

/// A cluster and the configs thoradm reads for it
pub struct Cluster {
    /// The cluster config thoradm reads
    pub conf_path: PathBuf,
    /// The Thorctl config thoradm and the test's client read
    pub ctl_conf: PathBuf,
    /// The namespace the cluster config names
    pub namespace: String,
    /// The directory the backup goes in, which doesn't exist yet
    backup: PathBuf,
}

impl Cluster {
    /// Reads the cluster the environment names
    pub fn from_env() -> Result<Cluster, String> {
        // both configs are required
        let path = |var: &str| {
            std::env::var_os(var)
                .map(PathBuf::from)
                .ok_or_else(|| format!("{var} isn't set"))
        };
        let cluster_conf = path(CLUSTER_CONF_VAR)?;
        let ctl_conf = path(CTL_CONF_VAR)?;
        // the namespace comes from the cluster config, loaded the way thoradm loads it
        let conf = Conf::new(&cluster_conf)
            .map_err(|err| format!("failed to load {}: {err}", cluster_conf.display()))?;
        // the backup goes in the named directory, or in a new one under the temp directory
        let backup = std::env::var_os(DIR_VAR).map_or_else(
            || std::env::temp_dir().join(format!("thoradm-end-to-end-{}", Uuid::new_v4())),
            PathBuf::from,
        );
        if backup.exists() {
            return Err(format!("{} already exists", backup.display()));
        }
        Ok(Cluster {
            conf_path: cluster_conf,
            ctl_conf,
            namespace: conf.thorium.namespace,
            backup,
        })
    }

    /// Returns the harness's namespace as a cluster
    ///
    /// # Arguments
    ///
    /// * `harness` - The harness whose namespace to use
    pub fn of_harness(harness: &Harness) -> Cluster {
        // the backup goes in a fresh scratch directory, which thoradm creates
        let backup = harness.scratch("end-to-end").join("backup");
        Cluster {
            conf_path: harness.cluster_conf.clone(),
            ctl_conf: harness.ctl_conf.clone(),
            namespace: harness.conf.thorium.namespace.clone(),
            backup,
        }
    }

    /// Returns the directory the backup goes in
    #[must_use]
    pub fn backup_dir(&self) -> PathBuf {
        // the directory was chosen when the cluster was named
        self.backup.clone()
    }

    /// Builds a command that runs thoradm against this cluster
    ///
    /// The command carries no `THORIUM__` or `THORCTL__` variables from the test environment, so
    /// the config files alone decide which cluster thoradm talks to.
    #[must_use]
    pub fn thoradm(&self) -> Command {
        // start from the built binary without the inherited config overrides
        let mut command = Command::new(env!("CARGO_BIN_EXE_thoradm"));
        for (key, _) in std::env::vars() {
            if key.starts_with("THORIUM__") || key.starts_with("THORCTL__") {
                command.env_remove(key);
            }
        }
        // point thoradm at the cluster's configs
        command
            .arg("--cluster-conf")
            .arg(&self.conf_path)
            .arg("--ctl-conf")
            .arg(&self.ctl_conf);
        command
    }

    /// Builds an admin client from the Thorctl config
    pub async fn client(&self) -> Thorium {
        // the Thorctl config holds the API's URL and the admin's token
        Thorium::from_ctl_conf_file(&self.ctl_conf)
            .await
            .unwrap_or_else(|err| {
                panic!(
                    "failed to build a client from {}: {err}",
                    self.ctl_conf.display()
                )
            })
    }
}
