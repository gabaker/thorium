//! What a backup directory holds: the marker of a backup in progress, and the manifest a
//! finished backup ends with
//!
//! A backup run writes `backup-in-progress.json` when it starts and, once every component has
//! run, writes `manifest.json` and removes the marker. Both are written to a temporary file,
//! synced, and renamed into place, so each is either whole or absent. A directory therefore
//! holds a finished backup (a manifest), an interrupted one (a marker and no manifest), or,
//! with neither file, a backup taken before manifests existed or files of no backup at all.

use chrono::{DateTime, Utc};
use data_encoding::HEXLOWER;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncReadExt};
use uuid::Uuid;

use super::sink::SinkFactory;
use super::version::VersionMismatch;
use crate::Error;
use crate::args::BackupComponents;

/// The name of the manifest a finished backup holds
pub const MANIFEST_FILE: &str = "manifest.json";

/// The name of the marker a backup in progress holds
pub const MARKER_FILE: &str = "backup-in-progress.json";

/// The name of the file in a backup root that the run writing into it holds an exclusive lock on
pub const LOCK_FILE: &str = "backup.lock";

/// The archive format version this thoradm writes; it reads every version up to this one
pub const FORMAT_VERSION: u32 = 2;

/// The name of the object index each S3 component writes in its directory
pub const INDEX_FILE: &str = "objects.index.jsonl";

/// The name of the file listing what `--prune-dangling` changed in the Redis state, written in the
/// backup root beside `redis.json`
pub const PRUNED_FILE: &str = "pruned.jsonl";

/// Files other features write into a backup root, which the manifest lists when present
pub const SIBLINGS: &[&str] = &["schema.json"];

/// The mode the manifest and the marker are created with
const MODE: u32 = 0o600;

/// The size of the buffer a file is read through to hash it
const HASH_BUFFER: usize = 1_048_576;

/// `backup-in-progress.json`: marks a backup run that hasn't finished
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InProgressMarker {
    /// The thoradm version running the backup
    pub thoradm_version: String,
    /// When the run started
    pub started: DateTime<Utc>,
    /// The components the run backs up, after expanding `all` and `no-s3`
    pub components: Vec<BackupComponents>,
    /// Whether the run accepts broken links (`--accept-broken-links`), which a run that resumes
    /// this backup keeps accepting
    #[serde(default)]
    pub accept_broken_links: bool,
}

impl InProgressMarker {
    /// Builds the marker of a run starting now
    ///
    /// # Arguments
    ///
    /// * `started` - When the run started
    /// * `components` - The components the run backs up
    #[must_use]
    pub fn new(started: DateTime<Utc>, components: Vec<BackupComponents>) -> Self {
        // name the running thoradm
        InProgressMarker {
            thoradm_version: env!("CARGO_PKG_VERSION").to_owned(),
            started,
            components,
            accept_broken_links: false,
        }
    }

    /// Writes the marker atomically into a backup root
    ///
    /// # Arguments
    ///
    /// * `root` - The backup root
    /// * `sinks` - The file-system seam to write through
    pub async fn write(&self, root: &Path, sinks: &dyn SinkFactory) -> Result<(), Error> {
        // the marker goes in place whole or not at all
        write_json_atomic(root, MARKER_FILE, self, sinks).await
    }
}

/// `manifest.json`: what a finished backup holds, written last and atomically
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// The archive format version of every table in this backup
    pub format_version: u32,
    /// The thoradm version that wrote the backup
    pub thoradm_version: String,
    /// The commit thoradm was built from, when the build recorded one
    #[serde(default)]
    pub thoradm_commit: Option<String>,
    /// The Thorium namespace that was backed up
    pub namespace: String,
    /// The partition size of each partitioned data type when the backup ran, by config name
    pub partition_sizes: BTreeMap<String, u32>,
    /// The S3 bucket name of each bucket role when the backup ran
    pub buckets: BTreeMap<String, String>,
    /// When the backup run started
    pub started: DateTime<Utc>,
    /// When the manifest was written
    pub finished: DateTime<Utc>,
    /// The components the run backed up, after expanding `all` and `no-s3`
    pub components: Vec<BackupComponents>,
    /// How many objects failed across the run
    pub failures: u64,
    /// How many rows were skipped across the run
    pub skipped: u64,
    /// How many broken links the run accepted (`--accept-broken-links`): references to objects the
    /// cluster itself lacked, recorded in `failures.jsonl` and kept as they were
    #[serde(default)]
    pub broken_links: u64,
    /// The sha256 of `failures.jsonl`, when the run recorded anything
    #[serde(default)]
    pub failures_sha256: Option<String>,
    /// One entry per backed-up table, by table name
    pub tables: BTreeMap<String, TableEntry>,
    /// One entry per S3 component, by component name
    pub s3: BTreeMap<String, S3Entry>,
    /// The Redis state, when the `redis` component wrote it
    #[serde(default)]
    pub redis: Option<RedisEntry>,
    /// Other files in the backup root that the backup carries (`schema.json`)
    #[serde(default)]
    pub siblings: Vec<SiblingEntry>,
    /// The API's version and thoradm's, when they differed and `--allow-version-mismatch` let
    /// the backup of the Redis state go ahead; left out when they matched
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_version_mismatch: Option<VersionMismatch>,
    /// `pruned.jsonl`, when `--prune-dangling` dropped references from the Redis state; left out
    /// when nothing was pruned
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pruned: Option<PrunedEntry>,
}

/// `pruned.jsonl`: what `--prune-dangling` changed in the Redis state, one JSON line per change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrunedEntry {
    /// How many changes the file lists
    pub count: u64,
    /// The file's sha256
    pub sha256: String,
}

/// One backed-up table
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableEntry {
    /// Every archive the table's directory holds
    pub archives: Vec<ArchiveEntry>,
    /// The rows read from each token range, in range order
    pub ranges: Vec<RangeCount>,
    /// The table's total rows across its archives
    pub rows: u64,
    /// The rows skipped because they couldn't be deserialized
    pub skipped: u64,
    /// The columns its archives carry, as in the registry for this format version
    pub columns: Vec<String>,
}

/// One archive (a data file and its map) of a table
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveEntry {
    /// The uuid naming `data/<uuid>` and `maps/<uuid>.thoriummap`
    pub uuid: Uuid,
    /// The data file's length in bytes, equal to the last map entry's `end`
    pub data_len: u64,
    /// The number of 96-byte entries in the map
    pub entries: u64,
    /// The rows stored in this archive
    pub rows: u64,
    /// The sha256 of the map file
    pub map_sha256: String,
}

/// The rows read from one token range
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RangeCount {
    /// The first token of the range
    pub start: i64,
    /// The last token of the range
    pub end: i64,
    /// How many rows of the range were read and handed to the archive writer
    pub rows: u64,
}

/// One S3 component
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct S3Entry {
    /// The bucket roles the objects came from
    pub bucket_roles: Vec<String>,
    /// How many objects were copied
    pub objects: u64,
    /// Their total size in bytes
    pub bytes: u64,
    /// The object index file, relative to the backup root (`<table>/objects.index.jsonl`)
    pub index: String,
    /// The sha256 of the object index file
    pub index_sha256: String,
}

/// One line of a component's object index
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectIndexLine {
    /// The bucket the object came from
    pub bucket: String,
    /// The object's key in its bucket
    pub key: String,
    /// The object's size in bytes
    pub size: u64,
    /// The `ETag` S3 returned for it
    pub etag: String,
    /// The sha256 of the bytes written to the backup
    pub sha256: String,
}

/// The Redis state in `redis.json`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedisEntry {
    /// The file's length in bytes
    pub len: u64,
    /// The file's sha256
    pub sha256: String,
    /// Records per type: `users`, `groups`, `images`, `pipelines`
    pub counts: BTreeMap<String, u64>,
}

/// A file in the backup root that another feature writes and this backup carries unchanged
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiblingEntry {
    /// The file name (`schema.json`)
    pub name: String,
    /// Its length in bytes
    pub len: u64,
    /// Its sha256
    pub sha256: String,
}

impl Manifest {
    /// Reads a backup's manifest, or returns `None` when the backup has none
    ///
    /// A manifest written by a newer thoradm, with a format version this thoradm can't read, is
    /// refused before anything else is read.
    ///
    /// # Arguments
    ///
    /// * `root` - The backup root
    pub async fn read(root: &Path) -> Result<Option<Manifest>, Error> {
        // a backup without a manifest is interrupted or legacy, which callers judge themselves
        let path = root.join(MANIFEST_FILE);
        if !tokio::fs::try_exists(&path).await? {
            return Ok(None);
        }
        let bytes = tokio::fs::read(&path).await?;
        // check the version before the rest, so a newer layout is named rather than misread
        let probe: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|error| Error::new(format!("{} can't be read: {error}", path.display())))?;
        if let Some(newer) = newer_format(&probe, &path) {
            return Err(newer);
        }
        let manifest = serde_json::from_slice(&bytes)
            .map_err(|error| Error::new(format!("{} can't be read: {error}", path.display())))?;
        Ok(Some(manifest))
    }

    /// Writes the manifest atomically and then removes the in-progress marker
    ///
    /// # Arguments
    ///
    /// * `root` - The backup root
    /// * `sinks` - The file-system seam to write through
    pub async fn write(&self, root: &Path, sinks: &dyn SinkFactory) -> Result<(), Error> {
        // the manifest goes in place whole or not at all
        write_json_atomic(root, MANIFEST_FILE, self, sinks).await?;
        // the backup is finished, so the marker goes last
        sinks.remove(&root.join(MARKER_FILE)).await?;
        Ok(())
    }
}

/// Returns the refusal of a manifest whose format version is newer than this thoradm reads
///
/// # Arguments
///
/// * `probe` - The manifest parsed as plain JSON
/// * `path` - The manifest's path, for the message
fn newer_format(probe: &serde_json::Value, path: &Path) -> Option<Error> {
    // a manifest without a version reads as version 0, which every thoradm reads
    let version = probe
        .get("format_version")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if version <= u64::from(FORMAT_VERSION) {
        return None;
    }
    let writer = probe
        .get("thoradm_version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("an unknown version");
    Some(Error::new(format!(
        "{} is format version {version}, written by thoradm {writer}; this thoradm \
         ({}) reads format versions up to {FORMAT_VERSION}, so use thoradm {writer} \
         or newer",
        path.display(),
        env!("CARGO_PKG_VERSION")
    )))
}

/// What a backup directory holds
#[derive(Debug, Clone, PartialEq)]
pub enum Layout {
    /// The directory doesn't exist, or holds no files (empty directories at most)
    Empty,
    /// A finished backup: the directory holds `manifest.json`
    Finished,
    /// An interrupted backup: the directory holds `backup-in-progress.json` and no manifest
    Interrupted(InProgressMarker),
    /// Files but neither a manifest nor a marker: a backup taken before manifests existed, or
    /// files of no backup at all
    Legacy,
}

/// Returns whether a directory holds any file other than directories, at any depth
///
/// The lock file a run takes in the directory itself doesn't count.
///
/// # Arguments
///
/// * `dir` - The directory to search
async fn holds_files(dir: &Path) -> Result<bool, Error> {
    // walk depth first, stopping at the first file
    let lock = dir.join(LOCK_FILE);
    let mut pending: Vec<PathBuf> = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        let mut listing = tokio::fs::read_dir(&next).await?;
        while let Some(entry) = listing.next_entry().await? {
            if entry.file_type().await?.is_dir() {
                pending.push(entry.path());
            } else if entry.path() != lock {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Returns what a backup directory holds
///
/// # Arguments
///
/// * `root` - The backup directory
pub async fn classify(root: &Path) -> Result<Layout, Error> {
    // a directory that doesn't exist yet is empty
    if !tokio::fs::try_exists(root).await? {
        return Ok(Layout::Empty);
    }
    // a manifest means the backup finished, whatever else is there
    if tokio::fs::try_exists(root.join(MANIFEST_FILE)).await? {
        return Ok(Layout::Finished);
    }
    // a marker without a manifest means the backup was interrupted
    let marker = root.join(MARKER_FILE);
    if tokio::fs::try_exists(&marker).await? {
        let bytes = tokio::fs::read(&marker).await?;
        let marker = serde_json::from_slice(&bytes).map_err(|error| {
            Error::new(format!(
                "{} can't be read: {error}",
                root.join(MARKER_FILE).display()
            ))
        })?;
        return Ok(Layout::Interrupted(marker));
    }
    // otherwise any file at all makes it something this thoradm didn't write
    if holds_files(root).await? {
        Ok(Layout::Legacy)
    } else {
        Ok(Layout::Empty)
    }
}

/// Writes a value as JSON into a directory atomically, through [`write_atomic`]
///
/// # Arguments
///
/// * `dir` - The directory to write into
/// * `name` - The file's name
/// * `value` - The value to write
/// * `sinks` - The file-system seam to write through
pub async fn write_json_atomic<T: Serialize>(
    dir: &Path,
    name: &str,
    value: &T,
    sinks: &dyn SinkFactory,
) -> Result<(), Error> {
    // serialize first so a value that can't be written leaves nothing behind
    let bytes = serde_json::to_vec_pretty(value)?;
    Ok(write_atomic(dir, name, &bytes, sinks).await?)
}

/// Writes a file into a directory atomically: a temporary file created 0600 is written, synced,
/// and renamed into place, and the directory is synced
///
/// A stale temporary file is removed first, so the new one is created with the mode, and the
/// temporary file is removed again if any step fails, so the file is either whole or absent.
///
/// # Arguments
///
/// * `dir` - The directory to write into
/// * `name` - The file's name
/// * `bytes` - The file's contents
/// * `sinks` - The file-system seam to write through
pub async fn write_atomic(
    dir: &Path,
    name: &str,
    bytes: &[u8],
    sinks: &dyn SinkFactory,
) -> std::io::Result<()> {
    // the temporary file sits beside the final one
    let tmp = dir.join(format!("{name}.tmp"));
    let written = write_tmp(sinks, &tmp, &dir.join(name), bytes).await;
    // never leave the temporary file behind
    if written.is_err() {
        let _ = sinks.remove(&tmp).await;
    }
    written
}

/// Writes, syncs, and renames a temporary file into place, then syncs its directory
///
/// # Arguments
///
/// * `sinks` - The file-system seam to write through
/// * `tmp` - The temporary file
/// * `path` - The final path
/// * `bytes` - The file's contents
async fn write_tmp(
    sinks: &dyn SinkFactory,
    tmp: &Path,
    path: &Path,
    bytes: &[u8],
) -> std::io::Result<()> {
    // a mode only applies to a file open creates, so remove any stale copy first
    sinks.remove(tmp).await?;
    let mut file = sinks.create(tmp, MODE).await?;
    // write the contents and force them to disk
    file.write_all(bytes).await?;
    file.sync().await?;
    drop(file);
    // move it into place and make the rename durable
    sinks.rename(tmp, path).await?;
    if let Some(dir) = path.parent() {
        sinks.sync_dir(dir).await?;
    }
    Ok(())
}

/// Returns the sha256 of a file as lowercase hex
///
/// # Arguments
///
/// * `path` - The file to hash
pub async fn file_sha256(path: &Path) -> Result<String, Error> {
    // stream the file through a fixed buffer, so a large object never sits in memory whole
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; HASH_BUFFER];
    loop {
        let read = file.read(&mut buffer).await?;
        // an empty read is the end of the file
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(HEXLOWER.encode(&hasher.finalize()))
}

/// Reads an object index into a map from bucket and key to size and sha256
///
/// A missing index reads as empty, and a line that doesn't parse (the torn last line of an
/// interrupted run, which may end inside a multi-byte character) is skipped: its object is
/// downloaded again.
///
/// # Arguments
///
/// * `path` - The object index
pub async fn read_index(path: &Path) -> Result<HashMap<(String, String), (u64, String)>, Error> {
    // a component that never wrote an index has nothing to keep
    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(error) => return Err(error.into()),
    };
    // read it a line of bytes at a time, so a line that isn't valid UTF-8 fails only itself
    let mut reader = tokio::io::BufReader::new(file);
    let mut index = HashMap::new();
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line).await? > 0 {
        // keep every line that parses
        if let Ok(parsed) = serde_json::from_slice::<ObjectIndexLine>(&line) {
            index.insert((parsed.bucket, parsed.key), (parsed.size, parsed.sha256));
        }
        line.clear();
    }
    Ok(index)
}

/// Lists the sibling files a backup root holds, with their lengths and sha256s
///
/// # Arguments
///
/// * `root` - The backup root
pub async fn siblings(root: &Path) -> Result<Vec<SiblingEntry>, Error> {
    // only the files other features are known to write
    let mut found = Vec::new();
    for name in SIBLINGS {
        let path = root.join(name);
        if tokio::fs::try_exists(&path).await? {
            found.push(SiblingEntry {
                name: (*name).to_owned(),
                len: tokio::fs::metadata(&path).await?.len(),
                sha256: file_sha256(&path).await?,
            });
        }
    }
    Ok(found)
}
