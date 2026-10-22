//! The files a per-run worker writes before it runs a job: its run's keys file, and for a
//! harness the models file and the credential files SDKs read
//!
//! A per-run worker in K8s gets its run's url and token, and a harness's worker its run's models,
//! only as literal pod env values. The worker builds its client from them and writes them into
//! the files tools and harnesses read, each readable only by the runner.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use thorium::client::Keys;
use thorium::models::{CREDENTIALS_DIR, HarnessInputs, HarnessInterface, HarnessModels};
use thorium::{Error, Thorium};

/// The pod env value holding the API url a per-run worker talks to
pub const URL_ENV: &str = "THORIUM_URL";

/// The pod env value holding a per-run worker's run-scoped token
pub const TOKEN_ENV: &str = "THORIUM_TOKEN";

/// The pod env value holding every agent's resolved model and credential
pub const MODELS_ENV: &str = "THORIUM_MODELS";

/// The keys file a per-run worker writes for tools and the CLI wrapper
pub const RUN_KEYS: &str = "/tmp/thorium/keys.yml";

/// The keys file at the standard path in a per-run pod's empty keys dir
pub const STANDARD_KEYS: &str = "/opt/thorium-keys/keys.yml";

/// A per-run worker's url, run-scoped token, and models, read from its pod env
#[derive(Clone)]
pub struct RunEnv {
    /// The API url to talk to
    pub url: String,
    /// The run-scoped token
    pub token: String,
    /// Every agent's resolved model and credential as JSON, for a harness's worker
    pub models: Option<String>,
}

impl fmt::Debug for RunEnv {
    /// Write this env without its token or models
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // the token and models are secret
        f.debug_struct("RunEnv")
            .field("url", &self.url)
            .field("token", &"<redacted>")
            .field("models", &self.models.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl RunEnv {
    /// Build a run env from its values, if both the url and token are set
    ///
    /// # Arguments
    ///
    /// * `url` - The `THORIUM_URL` value
    /// * `token` - The `THORIUM_TOKEN` value
    /// * `models` - The `THORIUM_MODELS` value
    #[must_use]
    pub fn from_values(
        url: Option<String>,
        token: Option<String>,
        models: Option<String>,
    ) -> Option<Self> {
        // both the url and token must be set and not empty
        match (url, token) {
            (Some(url), Some(token)) if !url.is_empty() && !token.is_empty() => Some(RunEnv {
                url,
                token,
                models: models.filter(|models| !models.is_empty()),
            }),
            _ => None,
        }
    }

    /// Read a run env from this process's env, if both the url and token are set
    #[must_use]
    pub fn from_env() -> Option<Self> {
        // read each value, treating anything unreadable as unset
        Self::from_values(
            std::env::var(URL_ENV).ok(),
            std::env::var(TOKEN_ENV).ok(),
            std::env::var(MODELS_ENV).ok(),
        )
    }

    /// Build a client that authenticates with this run's token
    pub async fn client(&self) -> Result<Thorium, Error> {
        // the token is used as is, with no token request
        Thorium::build(&self.url).token(&self.token).build().await
    }
}

/// Where a per-run worker writes its run's files
#[derive(Debug, Clone)]
pub struct HarnessPaths {
    /// Every path the keys file is written to
    pub keys: Vec<PathBuf>,
    /// The models file
    pub models: PathBuf,
    /// The directory credential files are written to
    pub credentials: PathBuf,
}

impl HarnessPaths {
    /// Get the standard paths, with a harness's own keys and models paths
    ///
    /// # Arguments
    ///
    /// * `harness` - The harness's interface, when the job runs a harness
    #[must_use]
    pub fn standard(harness: Option<&HarnessInterface>) -> Self {
        // a harness names where it reads its keys and models
        let inputs = harness.map_or_else(HarnessInputs::default, |harness| harness.inputs.clone());
        // the keys file goes where the harness reads it and at both standard paths
        let mut keys = Vec::with_capacity(3);
        for path in [inputs.thorium_keys.as_str(), RUN_KEYS, STANDARD_KEYS] {
            let path = PathBuf::from(path);
            if !keys.contains(&path) {
                keys.push(path);
            }
        }
        HarnessPaths {
            keys,
            models: PathBuf::from(inputs.models),
            credentials: PathBuf::from(CREDENTIALS_DIR),
        }
    }
}

/// Write a file only its owner can read or write, creating its directory
///
/// # Arguments
///
/// * `path` - The file to write
/// * `contents` - What to write
fn write_private(path: &Path, contents: &[u8]) -> Result<(), Error> {
    use std::io::Write;
    // make sure the directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // open the file for its owner only, replacing anything already there
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    // a file that already existed keeps its mode, so set it again
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(contents)?;
    Ok(())
}

/// Write a run's keys file to every keys path
///
/// # Arguments
///
/// * `env` - The run's url and token
/// * `paths` - Where to write the files
pub fn write_keys(env: &RunEnv, paths: &HarnessPaths) -> Result<(), Error> {
    // the keys file holds the url and the run-scoped token
    let keys = Keys::new_token(&env.url, &env.token);
    let raw = serde_norway::to_string(&keys)
        .map_err(|error| Error::new(format!("Failed to build the run's keys file: {error}")))?;
    for path in &paths.keys {
        write_private(path, raw.as_bytes())?;
    }
    Ok(())
}

/// Write a run's models file and the file of each credential SDKs read from a file
///
/// # Arguments
///
/// * `models` - Every agent's model and credential as JSON
/// * `paths` - Where to write the files
pub fn write_models(models: &str, paths: &HarnessPaths) -> Result<HarnessModels, Error> {
    // the models must be valid, without echoing them in the error
    let parsed: HarnessModels = serde_json::from_str(models).map_err(|error| {
        Error::new(format!(
            "{MODELS_ENV} isn't valid at line {} column {}",
            error.line(),
            error.column()
        ))
    })?;
    // write the models file
    let raw = serde_json::to_vec_pretty(&parsed)
        .map_err(|error| Error::new(format!("Failed to build the models file: {error}")))?;
    write_private(&paths.models, &raw)?;
    // write each credential file once
    let mut written = BTreeSet::new();
    for entry in parsed.agents.values() {
        let Some(credential) = &entry.credential else {
            continue;
        };
        let (Some(file), Some(contents)) = (&credential.file, credential.file_contents()) else {
            continue;
        };
        // a file name never leaves the credentials directory
        if file.contains('/') || file.contains('\\') || file == ".." || file == "." {
            return Err(Error::new(format!(
                "Credential file name `{file}` isn't a plain file name"
            )));
        }
        if written.insert(file.clone()) {
            write_private(&paths.credentials.join(file), contents.as_bytes())?;
        }
    }
    Ok(parsed)
}

/// Write a per-run worker's files for a job: the keys file, and for a harness the models file and
/// credential files
///
/// # Arguments
///
/// * `env` - The run's url, token, and models
/// * `harness` - The harness's interface, when the job runs a harness
/// * `paths` - Where to write the files
pub fn write_run_files(
    env: &RunEnv,
    harness: Option<&HarnessInterface>,
    paths: &HarnessPaths,
) -> Result<(), Error> {
    // every per-run job gets the keys file
    write_keys(env, paths)?;
    // a harness also gets its run's models and credentials
    if harness.is_some() {
        let Some(models) = &env.models else {
            return Err(Error::new(format!(
                "A harness's per-run worker has no {MODELS_ENV}"
            )));
        };
        write_models(models, paths)?;
    }
    Ok(())
}

/// Write the files a job's worker needs before it runs: none for a shared worker, and its run's
/// files for a per-run worker built from its pod env
///
/// # Arguments
///
/// * `env` - The run's url, token, and models, for a per-run worker built from its env
/// * `harness` - The harness's interface, when the job runs a harness
/// * `paths` - Where to write the files
pub fn write_for_job(
    env: Option<&RunEnv>,
    harness: Option<&HarnessInterface>,
    paths: &HarnessPaths,
) -> Result<(), Error> {
    // a shared worker reads its user's mounted keys and writes nothing
    match env {
        Some(env) => write_run_files(env, harness, paths),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests;
