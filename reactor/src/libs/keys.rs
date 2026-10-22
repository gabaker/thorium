//! Handles user and run keys operations

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use thorium::Keys;
use thorium::models::Worker;
use uuid::Uuid;

use crate::Error;

/// The directory under the keys base that holds each run's keys
const RUNS_DIR: &str = "_runs";

/// The name of every keys file
const KEYS_FILE: &str = "keys.yml";

/// Get the base path to store keys at
#[cfg(target_os = "linux")]
pub fn base_path() -> PathBuf {
    PathBuf::from("/opt/thorium-keys")
}

/// Get the base path to store keys at
#[cfg(target_os = "windows")]
pub fn base_path() -> PathBuf {
    PathBuf::from("C:\\thorium\\keys")
}

/// Build the path a users keys should be written too/at
///
/// # Arguments
///
/// * `username` - The user whose keys we want to write/get
pub fn path(username: &str) -> PathBuf {
    // build the path under our base path
    path_in(&base_path(), username)
}

/// Build the path a users keys should be written to under a base directory
///
/// # Arguments
///
/// * `base` - The base directory keys are stored in
/// * `username` - The user whose keys we want to write/get
pub fn path_in(base: &Path, username: &str) -> PathBuf {
    // start with the base path for our keys
    let mut path = base.to_path_buf();
    // append our username
    path.push(username);
    path.push(KEYS_FILE);
    path
}

/// Build the path a run's keys should be written to
///
/// # Arguments
///
/// * `run` - The run whose keys we want to write/get
pub fn run_path(run: &Uuid) -> PathBuf {
    // build the path under our base path
    run_path_in(&base_path(), run)
}

/// Build the directory every run's keys are written under in a base directory
///
/// Usernames are only lowercase letters, digits, and `-`, so this never collides with a user.
///
/// # Arguments
///
/// * `base` - The base directory keys are stored in
pub fn runs_dir_in(base: &Path) -> PathBuf {
    base.join(RUNS_DIR)
}

/// Build the path a run's keys should be written to under a base directory
///
/// # Arguments
///
/// * `base` - The base directory keys are stored in
/// * `run` - The run whose keys we want to write/get
pub fn run_path_in(base: &Path, run: &Uuid) -> PathBuf {
    // runs keep their keys in their own directory
    runs_dir_in(base).join(run.to_string()).join(KEYS_FILE)
}

/// Build the path of the keys a worker's agent authenticates with
///
/// A per-run worker uses its run's keys and any other worker its user's keys.
///
/// # Arguments
///
/// * `worker` - The worker to get the keys path for
pub fn for_worker(worker: &Worker) -> PathBuf {
    // per-run workers use their run's keys
    match &worker.run {
        Some(run) => run_path(run),
        None => path(&worker.user),
    }
}

/// Build the path of the keys a worker's agent authenticates with under a base directory
///
/// # Arguments
///
/// * `base` - The base directory keys are stored in
/// * `worker` - The worker to get the keys path for
#[cfg(test)]
pub fn for_worker_in(base: &Path, worker: &Worker) -> PathBuf {
    // per-run workers use their run's keys
    match &worker.run {
        Some(run) => run_path_in(base, run),
        None => path_in(base, &worker.user),
    }
}

/// Build the path of the copy of a keys file that an agent running as a uid reads
///
/// # Arguments
///
/// * `path` - The keys file being copied
/// * `uid` - The uid the agent runs as
pub fn uid_copy(path: &Path, uid: u32) -> PathBuf {
    // the copy sits next to the keys file in a directory for the uid
    let parent = path.parent().unwrap_or(path);
    parent.join(format!("uid-{uid}")).join(KEYS_FILE)
}

/// Check if a target users key file exists and if it is correct
///
/// # Arguments
///
/// * `path` - The path to check against
/// * `token` - The token to look for
pub async fn exists(path: &PathBuf, token: &str) -> Result<bool, thorium::Error> {
    // determine if this users keys exist on disk or not
    if path.exists() {
        // this users key exists so make sure its correct
        // read in this users current key file
        let data = tokio::fs::read_to_string(path).await?;
        // check this users token is in our key file
        if data.contains(token) {
            return Ok(true);
        } else {
            // a keys file exists but its wrong so delete it
            tokio::fs::remove_file(path).await?;
        }
    }
    // this file doesn't currently exist
    Ok(false)
}

/// Only let the owner of a keys file read or write it
///
/// # Arguments
///
/// * `path` - The keys file to restrict
pub async fn restrict(path: &Path) -> Result<(), Error> {
    // keys are only readable by their owner on unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    }
    // windows keys files keep the default ACLs of the keys directory
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Write a keys file holding a token that only its owner can read
///
/// # Arguments
///
/// * `path` - The path to write the keys file to
/// * `host` - The Thorium API the token is for
/// * `token` - The token to write
pub async fn write(path: &Path, host: &str, token: &str) -> Result<(), Error> {
    // make sure all of our parent paths exists
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    // serialize our keys in the keys.yml format
    let keys = Keys::new_token(host, token);
    let serialized = serde_norway::to_string(&keys)
        .map_err(|err| Error::new(format!("Failed to serialize keys: {err}")))?;
    // create the file empty and restrict it before writing the token into it
    tokio::fs::write(path, b"").await?;
    restrict(path).await?;
    tokio::fs::write(path, serialized.as_bytes()).await?;
    Ok(())
}

/// Make sure a user's keys file holds their current token and only they can read it
///
/// # Arguments
///
/// * `base` - The base directory keys are stored in
/// * `host` - The Thorium API the token is for
/// * `username` - The user whose keys to write
/// * `token` - The user's current token
pub async fn ensure_user(
    base: &Path,
    host: &str,
    username: &str,
    token: &str,
) -> Result<(), Error> {
    // build the path to store this users keys at
    let path = path_in(base, username);
    // keep keys that already hold this token, restricting them if an older reactor didn't
    if exists(&path, token).await? {
        return restrict(&path).await;
    }
    // write this users keys
    write(&path, host, token).await
}

/// Copy a keys file for an agent that runs as another uid and gid
///
/// The copy can only be read by the agent's uid.
///
/// # Arguments
///
/// * `src` - The keys file to copy
/// * `dest` - Where to write the copy
/// * `uid` - The uid that owns the copy
/// * `gid` - The gid that owns the copy
#[cfg(unix)]
pub async fn copy_for_agent(src: &Path, dest: &Path, uid: u32, gid: u32) -> Result<(), Error> {
    // read the keys to copy
    let data = tokio::fs::read(src).await?;
    // make sure the copy's directory exists
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    // create the copy empty, restrict it, and hand it to the agent's ids before filling it
    tokio::fs::write(dest, b"").await?;
    restrict(dest).await?;
    std::os::unix::fs::chown(dest, Some(uid), Some(gid))?;
    tokio::fs::write(dest, data).await?;
    Ok(())
}

/// Delete the keys of every run that no worker on this node serves anymore
///
/// # Arguments
///
/// * `base` - The base directory keys are stored in
/// * `live` - The runs that workers on this node still serve
pub async fn sweep_runs(base: &Path, live: &HashSet<Uuid>) -> Result<(), Error> {
    // list the runs we have keys for
    let mut entries = match tokio::fs::read_dir(runs_dir_in(base)).await {
        Ok(entries) => entries,
        // no run has written keys yet
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(Error::from(err)),
    };
    // remove the directory of each run nothing serves
    while let Some(entry) = entries.next_entry().await? {
        // only directories named after a run are ours to remove
        let name = entry.file_name();
        let Some(run) = name.to_str().and_then(|name| Uuid::parse_str(name).ok()) else {
            continue;
        };
        if !live.contains(&run) {
            tokio::fs::remove_dir_all(entry.path()).await?;
        }
    }
    Ok(())
}
