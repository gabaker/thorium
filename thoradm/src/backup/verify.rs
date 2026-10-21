//! Checks a backup directory against its manifest
//!
//! `backup verify` checks everything a finished backup's manifest lists: every table's archives
//! (map lengths and checksums, data lengths, every partition's checksum, and the rows each archive
//! decodes to), every S3 component's objects against its object index, `redis.json`, and the
//! sibling files, and it lists the objects the backup itself recorded as failed. A backup without
//! a manifest can't be checked for completeness: an interrupted one is reported as such, and a
//! legacy one gets the archive checks only. Every failed check is a record in the report
//! directory's `failures.jsonl`, and `verify.json` there sums up every check.

use chrono::{DateTime, Utc};
use clap::ValueEnum;
use data_encoding::HEXLOWER;
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

use super::archive::check_partition;
use super::controllers::restore::report_dir;
use super::coverage::{COVERAGE, Coverage, Store};
use super::failures::{FAILURES_FILE, FailureLog, FailurePolicy, FailureRecord, Operation};
use super::manifest::{
    self, ArchiveEntry, INDEX_FILE, Layout, MANIFEST_FILE, Manifest, PRUNED_FILE, RedisEntry,
    S3Entry, SIBLINGS, TableEntry, file_sha256,
};
use super::s3::{keepable_key, object_relative_path};
use super::sink::{SinkFactory, TokioFs, create_private_dir};
use super::tables::{
    self, Association, Comment, Commitish, CommitishList, Entity, NetworkPolicy, Node,
    Notification, Output, OutputStream, RepoData, RepoList, S3Id, SamplesList, Tag, Versioned,
};
use super::utils::is_link;
use super::{ArchiveReader, MAP_ENTRY_LEN, Utils};
use crate::Error;
use crate::args::{Args, BackupComponents, VerifyBackup};
use crate::outcome::RunOutcome;

/// The name of the report verify writes in its report directory
pub const REPORT_FILE: &str = "verify.json";

/// The component verify records problems of the manifest itself under
pub const MANIFEST_COMPONENT: &str = "manifest";

/// The component verify records problems of the sibling files under
pub const SIBLINGS_COMPONENT: &str = "siblings";

/// Which end verify checks
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VerifyMode {
    /// A backup directory against its manifest
    Backup,
    /// A restored cluster against the backup's manifest
    Cluster,
}

/// One check verify ran, for one component
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyCheck {
    /// The component checked (`samples-list`, `result-files`, `redis`, `manifest`, `siblings`)
    pub component: String,
    /// The check (`map-length`, `partition-sha256`, `object`, `redis-json`, …)
    pub check: String,
    /// How many objects the check examined
    pub checked: u64,
    /// How many failed; each failure is also a record in the report's `failures.jsonl`
    pub failed: u64,
}

/// The result of `thoradm backup verify`, printed as a summary and written as `verify.json` in
/// the report directory
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerifyReport {
    /// Which end was checked
    pub mode: VerifyMode,
    /// The backup checked against
    pub backup: PathBuf,
    /// The backup's format version, or `None` for a backup without a manifest
    pub format_version: Option<u32>,
    /// Whether the backup has a manifest (false for an interrupted or legacy backup)
    pub complete_backup: bool,
    /// Every check run, in the order each was first run
    pub checks: Vec<VerifyCheck>,
    /// Whether `--abort-on-failure` stopped verify at its first failed check
    #[serde(default)]
    pub stopped_early: bool,
    /// When verify started
    pub started: DateTime<Utc>,
    /// When verify finished
    pub finished: DateTime<Utc>,
}

impl VerifyReport {
    /// Returns the outcome verify exits with
    #[must_use]
    pub fn outcome(&self) -> RunOutcome {
        // a verify that --abort-on-failure stopped didn't run every check, so it is an abort
        if self.stopped_early {
            return RunOutcome::Aborted;
        }
        // any failed check means the backup doesn't match its manifest
        if self.checks.iter().any(|check| check.failed > 0) {
            RunOutcome::CompletedWithFailures
        } else {
            RunOutcome::Complete
        }
    }

    /// Returns how many checks failed
    #[must_use]
    pub fn failed(&self) -> u64 {
        // every failed check of every component
        self.checks.iter().map(|check| check.failed).sum()
    }
}

/// Decodes one partition of a table's archive, written in a format version, and returns how many
/// rows it holds
type RowCounter = fn(&[u8], u32) -> Result<u64, Error>;

/// A table's row counter, with the format version its archives were written in
#[derive(Clone, Copy)]
struct Counter {
    /// Decodes one of the table's partitions and counts its rows
    count: RowCounter,
    /// The format version the backup was written in; a legacy backup's is 1
    format: u32,
}

/// Decodes a partition with the archive struct of a table's format and counts its rows
///
/// # Arguments
///
/// * `bytes` - The partition's bytes
/// * `format` - The format version the backup was written in
fn count_rows<T: Versioned>(bytes: &[u8], format: u32) -> Result<u64, Error> {
    // every partition is an archived list of the table's rows
    tables::count_rows::<T>(bytes, format)
        .map_err(|error| Error::new(format!("the partition doesn't decode: {error}")))
}

/// Every backed-up table with the function that decodes its partitions
pub const TABLES: &[(&str, RowCounter)] = &[
    ("samples_list", count_rows::<SamplesList>),
    ("s3_ids", count_rows::<S3Id>),
    ("comments", count_rows::<Comment>),
    ("results", count_rows::<Output>),
    ("results_stream", count_rows::<OutputStream>),
    ("tags", count_rows::<Tag>),
    ("repo_data", count_rows::<RepoData>),
    ("repos_list", count_rows::<RepoList>),
    ("commitishes", count_rows::<Commitish>),
    ("commitish_list", count_rows::<CommitishList>),
    ("nodes", count_rows::<Node>),
    ("network_policies", count_rows::<NetworkPolicy>),
    ("entities", count_rows::<Entity>),
    ("associations", count_rows::<Association>),
    ("notifications", count_rows::<Notification>),
];

/// Returns the names every table archive struct gives its table, in the order of [`TABLES`]
#[must_use]
pub fn archive_struct_names() -> [&'static str; 15] {
    // the archive structs name the directories their archives live in
    [
        SamplesList::name(),
        S3Id::name(),
        Comment::name(),
        Output::name(),
        OutputStream::name(),
        Tag::name(),
        RepoData::name(),
        RepoList::name(),
        Commitish::name(),
        CommitishList::name(),
        Node::name(),
        NetworkPolicy::name(),
        Entity::name(),
        Association::name(),
        Notification::name(),
    ]
}

/// Returns the component that backs up a table, as runs name it
///
/// # Arguments
///
/// * `table` - The table
#[must_use]
pub fn table_component(table: &str) -> String {
    // the registry names the component of every backed-up table
    COVERAGE
        .iter()
        .find(|entry| entry.store == Store::ScyllaTable && entry.name == table)
        .and_then(|entry| match entry.coverage {
            Coverage::BackedUp(component) => Some(component.to_string()),
            Coverage::Excluded { .. } => None,
        })
        .unwrap_or_else(|| table.to_owned())
}

/// Returns the table a table component backs up, or `None` for other components
///
/// # Arguments
///
/// * `component` - The component
#[must_use]
pub fn component_table(component: BackupComponents) -> Option<&'static str> {
    // the registry names the table each table component backs up
    COVERAGE
        .iter()
        .find(|entry| {
            entry.store == Store::ScyllaTable
                && matches!(entry.coverage, Coverage::BackedUp(backed_up) if backed_up == component)
        })
        .map(|entry| entry.name)
}

/// Returns the directory an S3 component keeps its index and objects in, relative to the backup
/// root, or `None` for a component that isn't in S3
///
/// # Arguments
///
/// * `component` - The S3 component
#[must_use]
pub fn s3_component_dir(component: BackupComponents) -> Option<&'static str> {
    // the components that read a table's archives keep their objects beside that table's
    match component.s3_input() {
        Some(table) => component_table(table),
        None if component == BackupComponents::Graphics => Some("graphics"),
        None => None,
    }
}

/// Returns why a directory's entries can't be listed, or `None` when they can or the path is
/// missing or not a directory
///
/// # Arguments
///
/// * `path` - The path to try listing
async fn unlistable(path: &Path) -> Option<std::io::Error> {
    // only a directory has entries to list; a missing path is left to the check that needs it
    let metadata = tokio::fs::symlink_metadata(path).await.ok()?;
    if !metadata.is_dir() {
        return None;
    }
    // a directory without read permission has metadata but no listing
    tokio::fs::read_dir(path).await.err()
}

/// Returns a path relative to the backup root, for records and messages
///
/// # Arguments
///
/// * `root` - The backup root
/// * `path` - A path inside it
fn relative(root: &Path, path: &Path) -> String {
    // a path outside the root is shown whole
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// The checks of one run and their counts, shared by every concurrent check
pub struct Checks {
    /// The run's failure log
    log: Arc<FailureLog>,
    /// Every check run so far, in the order each was first run
    tally: Mutex<Vec<VerifyCheck>>,
}

impl Checks {
    /// Starts an empty tally that records failures in a log
    ///
    /// # Arguments
    ///
    /// * `log` - The run's failure log
    #[must_use]
    pub fn new(log: Arc<FailureLog>) -> Self {
        // nothing checked yet
        Checks {
            log,
            tally: Mutex::default(),
        }
    }

    /// Counts one examined object of a check
    ///
    /// # Arguments
    ///
    /// * `component` - The component checked
    /// * `check` - The check
    /// * `failed` - Whether the object failed the check
    fn count(&self, component: &str, check: &str, failed: bool) {
        // find or add the check, keeping the order checks were first run in
        let mut tally = self
            .tally
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // the check's entry, added on its first object
        let position = tally
            .iter()
            .position(|entry| entry.component == component && entry.check == check);
        let entry = if let Some(position) = position {
            &mut tally[position]
        } else {
            tally.push(VerifyCheck {
                component: component.to_owned(),
                check: check.to_owned(),
                checked: 0,
                failed: 0,
            });
            tally.last_mut().expect("a check was just added")
        };
        // count the object, and its failure if it failed
        entry.checked += 1;
        entry.failed += u64::from(failed);
    }

    /// Counts an object that passed a check
    ///
    /// # Arguments
    ///
    /// * `component` - The component checked
    /// * `check` - The check
    pub fn pass(&self, component: &str, check: &str) {
        // one more examined object, none failed
        self.count(component, check, false);
    }

    /// Counts an object that failed a check and records it
    ///
    /// # Arguments
    ///
    /// * `check` - The check
    /// * `record` - The record naming the object, whose component is the component checked
    pub async fn fail(&self, check: &str, record: FailureRecord) -> Result<(), Error> {
        // count it, then record it so it survives this run
        self.count(&record.component, check, true);
        self.log.record(&record).await
    }

    /// Returns true once `--abort-on-failure` stopped the run
    #[must_use]
    pub fn stopped(&self) -> bool {
        // only the abort policy ever stops a log
        self.log.stopped()
    }

    /// Returns every check run so far, in order
    #[must_use]
    pub fn snapshot(&self) -> Vec<VerifyCheck> {
        // copy the tally out from under its lock
        self.tally
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// Builds a failed verify record for a component
///
/// # Arguments
///
/// * `component` - The component checked
/// * `error` - What is wrong
fn problem<C: Into<String>, E: std::fmt::Display>(component: C, error: E) -> FailureRecord {
    // every verify record has the verify operation
    FailureRecord::failed(component, Operation::Verify, error)
}

/// The files of one archive being checked, with the names records give them
struct ArchiveFiles<'a> {
    /// The backup root
    root: &'a Path,
    /// The table's component
    component: &'a str,
    /// The table
    table: String,
    /// The archive's map
    map: &'a Path,
    /// The archive's data file
    data: &'a Path,
    /// The map's path relative to the backup root
    rel_map: String,
    /// The data file's path relative to the backup root
    rel_data: String,
}

impl<'a> ArchiveFiles<'a> {
    /// Names the files of an archive
    ///
    /// # Arguments
    ///
    /// * `root` - The backup root
    /// * `component` - The table's component
    /// * `map` - The archive's map, in `<table>/maps/`
    /// * `data` - The archive's data file
    fn new(root: &'a Path, component: &'a str, map: &'a Path, data: &'a Path) -> Self {
        // the table is the directory holding the map's directory
        let table = map
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        ArchiveFiles {
            root,
            component,
            table,
            map,
            data,
            rel_map: relative(root, map),
            rel_data: relative(root, data),
        }
    }

    /// Builds the record of a failed check of this archive
    ///
    /// # Arguments
    ///
    /// * `key` - The file the problem is in, relative to the backup root
    /// * `error` - What is wrong
    fn record(&self, key: &str, error: String) -> FailureRecord {
        // name the table and the file
        problem(self.component, error).table(&self.table).key(key)
    }
}

/// Checks a backup directory, recording every problem it finds
pub struct Verifier {
    /// The checks run and their counts
    pub checks: Checks,
    /// How many archives or objects are checked at once
    workers: usize,
}

impl Verifier {
    /// Builds a verifier that records problems in a log
    ///
    /// # Arguments
    ///
    /// * `log` - The run's failure log
    /// * `workers` - How many archives or objects are checked at once
    #[must_use]
    pub fn new(log: Arc<FailureLog>, workers: usize) -> Self {
        // at least one check at a time
        Verifier {
            checks: Checks::new(log),
            workers: workers.max(1),
        }
    }

    /// Records a problem of the backup as a whole, such as a missing manifest
    ///
    /// # Arguments
    ///
    /// * `root` - The backup root
    /// * `error` - What is wrong
    pub async fn manifest_problem<E: std::fmt::Display>(
        &self,
        root: &Path,
        error: E,
    ) -> Result<(), Error> {
        // name the manifest's path, which is what is missing or wrong
        let record =
            problem(MANIFEST_COMPONENT, error).key(relative(root, &root.join(MANIFEST_FILE)));
        self.checks.fail("manifest", record).await
    }

    /// Records each of some paths that is a symbolic link, whose own metadata can't be read, or
    /// that is a directory whose entries can't be listed, as a failure, returning whether any was
    ///
    /// # Arguments
    ///
    /// * `root` - The backup root
    /// * `component` - The component the paths belong to
    /// * `table` - The table the paths belong to, if any
    /// * `paths` - The paths to check, none of which may be a link
    async fn refuse_links(
        &self,
        root: &Path,
        component: &str,
        table: Option<&str>,
        paths: &[&Path],
    ) -> Result<bool, Error> {
        let mut linked = false;
        for path in paths {
            // a link would make verify check what it points at, outside the backup
            let key = relative(root, path);
            let error = match is_link(path).await {
                Ok(false) => match unlistable(path).await {
                    None => continue,
                    Some(error) => format!("{key} can't be read: {error}"),
                },
                Ok(true) => format!(
                    "{key} is a symbolic link; a backup holds only real files and directories, so it isn't followed"
                ),
                Err(error) => format!("{key} can't be read: {error}"),
            };
            linked = true;
            let mut record = problem(component, error).key(key);
            if let Some(table) = table {
                record = record.table(table);
            }
            self.checks.fail("link", record).await?;
        }
        Ok(linked)
    }

    /// Runs every check a manifest makes possible
    ///
    /// # Arguments
    ///
    /// * `root` - The backup root
    /// * `manifest` - The backup's manifest
    /// * `list_recorded` - Whether to record each object the backup itself recorded as failed;
    ///   restore leaves this to its own check, which `--accept-failures` overrides
    pub async fn manifest_checks(
        &self,
        root: &Path,
        manifest: &Manifest,
        list_recorded: bool,
    ) -> Result<(), Error> {
        // the manifest itself was read
        self.checks.pass(MANIFEST_COMPONENT, "manifest");
        // every component the manifest names has its entry
        self.check_components(manifest).await?;
        // every table's archives
        for (table, entry) in &manifest.tables {
            if self.checks.stopped() {
                return Ok(());
            }
            self.check_table(root, table, entry, manifest.format_version)
                .await?;
        }
        // every S3 component's objects
        for (component, entry) in &manifest.s3 {
            if self.checks.stopped() {
                return Ok(());
            }
            self.check_objects(root, component, entry, &manifest.buckets)
                .await?;
        }
        // the Redis state and the sibling files
        if let Some(entry) = &manifest.redis
            && !self.checks.stopped()
        {
            self.check_redis(root, entry).await?;
        }
        if !self.checks.stopped() {
            self.check_siblings(root, manifest).await?;
        }
        if !self.checks.stopped() {
            self.check_pruned(root, manifest).await?;
        }
        // and the objects the backup itself couldn't back up
        if !self.checks.stopped() {
            self.check_recorded_failures(root, manifest, list_recorded)
                .await?;
        }
        Ok(())
    }

    /// Runs the checks a backup without a manifest allows: every archive's map, data length,
    /// partition checksums, and rows, without knowing what the backup should hold
    ///
    /// # Arguments
    ///
    /// * `root` - The backup root
    pub async fn archive_checks(&self, root: &Path) -> Result<(), Error> {
        // every table directory the backup holds
        for (table, counter) in TABLES {
            if self.checks.stopped() {
                return Ok(());
            }
            let maps = root.join(table).join("maps");
            let component = table_component(table);
            // a linked table, maps, or data directory isn't followed
            let dirs = [&root.join(table), &maps, &root.join(table).join("data")];
            if self
                .refuse_links(root, &component, Some(table), &dirs.map(PathBuf::as_path))
                .await?
            {
                continue;
            }
            if !tokio::fs::try_exists(&maps).await? {
                continue;
            }
            // every archive map in it
            let mut archives = Vec::new();
            let mut listing = tokio::fs::read_dir(&maps).await?;
            while let Some(entry) = listing.next_entry().await? {
                let path = entry.path();
                if let Some(uuid) = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.strip_suffix(".thoriummap"))
                {
                    let data = root.join(table).join("data").join(uuid);
                    archives.push((path.clone(), data));
                }
            }
            archives.sort();
            stream::iter(archives)
                .map(|(map, data)| {
                    let component = &component;
                    async move {
                        let counter = Counter {
                            count: *counter,
                            format: 1,
                        };
                        self.check_archive(root, component, &map, &data, None, counter)
                            .await
                    }
                })
                .buffer_unordered(self.workers)
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<()>, Error>>()?;
        }
        Ok(())
    }

    /// Checks that every component a manifest names has its entry: each table its table entry,
    /// each S3 component its object entry, and `redis` its Redis entry
    ///
    /// # Arguments
    ///
    /// * `manifest` - The backup's manifest
    async fn check_components(&self, manifest: &Manifest) -> Result<(), Error> {
        for component in &manifest.components {
            // what entry this component needs
            let listed = if *component == BackupComponents::Redis {
                manifest.redis.is_some()
            } else if component.is_in_s3() {
                manifest.s3.contains_key(&component.to_string())
            } else if let Some(table) = component_table(*component) {
                manifest.tables.contains_key(table)
            } else {
                true
            };
            if listed {
                self.checks.pass(&component.to_string(), "coverage");
            } else {
                let record = problem(
                    component.to_string(),
                    format!(
                        "the backup ran {component}, but the manifest lists nothing it backed up"
                    ),
                )
                .key(MANIFEST_FILE);
                self.checks.fail("coverage", record).await?;
                if self.checks.stopped() {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Checks one table: its directory holds exactly the archives the manifest lists, each
    /// archive matches its entry, and the archives' rows add up to the table's
    ///
    /// # Arguments
    ///
    /// * `root` - The backup root
    /// * `table` - The table
    /// * `entry` - The table's manifest entry
    /// * `format` - The format version the backup was written in
    async fn check_table(
        &self,
        root: &Path,
        table: &str,
        entry: &TableEntry,
        format: u32,
    ) -> Result<(), Error> {
        let component = table_component(table);
        let dir = root.join(table);
        // a table without a row counter isn't one this thoradm can read
        let Some((_, counter)) = TABLES.iter().find(|(name, _)| *name == table) else {
            let record = problem(
                &component,
                format!("the manifest lists {table}, a table this thoradm doesn't know"),
            )
            .key(MANIFEST_FILE);
            return self.checks.fail("coverage", record).await;
        };
        // a linked table, maps, or data directory isn't followed
        let dirs = [&dir, &dir.join("maps"), &dir.join("data")];
        if self
            .refuse_links(root, &component, Some(table), &dirs.map(PathBuf::as_path))
            .await?
        {
            return Ok(());
        }
        // the table's directory is there
        if !tokio::fs::try_exists(&dir).await? {
            let record = problem(
                &component,
                format!("the manifest lists {table}, but its directory is missing"),
            )
            .table(table)
            .key(relative(root, &dir));
            return self.checks.fail("coverage", record).await;
        }
        // every listed archive's map and data file are there
        let mut present = Vec::new();
        let mut listed = HashSet::new();
        for archive in &entry.archives {
            let uuid = archive.uuid.to_string();
            listed.insert(uuid.clone());
            let map = dir.join("maps").join(format!("{uuid}.thoriummap"));
            let data = dir.join("data").join(&uuid);
            let mut whole = true;
            for path in [&map, &data] {
                if !tokio::fs::try_exists(path).await? {
                    whole = false;
                    let record = problem(
                        &component,
                        format!(
                            "{} is listed in the manifest but missing",
                            relative(root, path)
                        ),
                    )
                    .table(table)
                    .key(relative(root, path));
                    self.checks.fail("coverage", record).await?;
                    if self.checks.stopped() {
                        return Ok(());
                    }
                }
            }
            if whole {
                self.checks.pass(&component, "coverage");
                present.push((map, data, archive));
            }
        }
        // no archive file is there that the manifest doesn't list, since restore would load it
        self.check_unlisted_archives(root, table, &component, &listed)
            .await?;
        if self.checks.stopped() {
            return Ok(());
        }
        // each archive against its entry
        stream::iter(present)
            .map(|(map, data, archive)| {
                let component = &component;
                async move {
                    let counter = Counter {
                        count: *counter,
                        format,
                    };
                    self.check_archive(root, component, &map, &data, Some(archive), counter)
                        .await
                }
            })
            .buffer_unordered(self.workers)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<()>, Error>>()?;
        // the manifest's own counts agree: the archives' rows and the ranges' rows are the table's
        let archived: u64 = entry.archives.iter().map(|archive| archive.rows).sum();
        let ranged: u64 = entry.ranges.iter().map(|range| range.rows).sum();
        if archived == entry.rows && ranged == entry.rows {
            self.checks.pass(&component, "table-rows");
        } else if !self.checks.stopped() {
            let record = problem(
                &component,
                format!(
                    "the manifest says {table} holds {} rows, but its archives hold {archived} and its token ranges {ranged}",
                    entry.rows
                ),
            )
            .table(table)
            .key(MANIFEST_FILE);
            self.checks.fail("table-rows", record).await?;
        }
        Ok(())
    }

    /// Records every archive file in a table's directory that the manifest doesn't list
    ///
    /// # Arguments
    ///
    /// * `root` - The backup root
    /// * `table` - The table
    /// * `component` - The table's component
    /// * `listed` - The uuids of the archives the manifest lists
    async fn check_unlisted_archives(
        &self,
        root: &Path,
        table: &str,
        component: &str,
        listed: &HashSet<String>,
    ) -> Result<(), Error> {
        // maps and data files are named by their archive's uuid
        for (sub, suffix) in [("maps", ".thoriummap"), ("data", "")] {
            let sub_dir = root.join(table).join(sub);
            if !tokio::fs::try_exists(&sub_dir).await? {
                continue;
            }
            let mut listing = tokio::fs::read_dir(&sub_dir).await?;
            let mut strays = Vec::new();
            while let Some(item) = listing.next_entry().await? {
                let name = item.file_name().to_string_lossy().into_owned();
                let uuid = name.strip_suffix(suffix).unwrap_or(&name);
                if !listed.contains(uuid) {
                    strays.push(item.path());
                }
            }
            strays.sort();
            for stray in strays {
                let key = relative(root, &stray);
                let error = format!("{key} is an archive file the manifest doesn't list");
                let record = problem(component, error).table(table).key(key);
                self.checks.fail("coverage", record).await?;
                if self.checks.stopped() {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Checks one archive: its map's length and checksum, its data file's length, every
    /// partition's checksum, and the rows its partitions decode to
    ///
    /// The first failed check ends the archive's checks, so one damaged archive is one record.
    ///
    /// # Arguments
    ///
    /// * `root` - The backup root
    /// * `component` - The table's component
    /// * `map` - The archive's map
    /// * `data` - The archive's data file
    /// * `expected` - The archive's manifest entry, or `None` for a backup without a manifest
    /// * `counter` - Decodes one of the table's partitions, in its format, and counts its rows
    async fn check_archive(
        &self,
        root: &Path,
        component: &str,
        map: &Path,
        data: &Path,
        expected: Option<&ArchiveEntry>,
        counter: Counter,
    ) -> Result<(), Error> {
        // take no new archive once --abort-on-failure stopped the run
        if self.checks.stopped() {
            return Ok(());
        }
        let archive = ArchiveFiles::new(root, component, map, data);
        // the map and data file are as long as they should be, and the map is the one written
        let Some(data_len) = self.check_archive_files(&archive, expected).await? else {
            return Ok(());
        };
        // every partition's bytes match their checksum and decode
        let Some((rows, end)) = self.check_partitions(&archive, counter).await? else {
            return Ok(());
        };
        // the data file ends where its last partition does
        if end != data_len {
            let error = format!(
                "{} is {data_len} bytes long, but its last partition ends at byte {end}",
                archive.rel_data
            );
            return self
                .checks
                .fail("data-length", archive.record(&archive.rel_data, error))
                .await;
        }
        self.checks.pass(component, "data-length");
        // the archive holds the rows the manifest says it does
        if let Some(entry) = expected
            && entry.rows != rows
        {
            let error = format!(
                "{} decodes to {rows} rows; the manifest says {}",
                archive.rel_map, entry.rows
            );
            return self
                .checks
                .fail("rows", archive.record(&archive.rel_map, error))
                .await;
        }
        self.checks.pass(component, "rows");
        Ok(())
    }

    /// Checks an archive's map length and checksum and its data file's length, returning the data
    /// file's length when every check passed
    ///
    /// # Arguments
    ///
    /// * `archive` - The archive's files
    /// * `expected` - The archive's manifest entry, or `None` for a backup without a manifest
    async fn check_archive_files(
        &self,
        archive: &ArchiveFiles<'_>,
        expected: Option<&ArchiveEntry>,
    ) -> Result<Option<u64>, Error> {
        let (rel_map, rel_data) = (&archive.rel_map, &archive.rel_data);
        // a linked map or data file isn't followed
        if self
            .refuse_links(
                archive.root,
                archive.component,
                Some(&archive.table),
                &[archive.map, archive.data],
            )
            .await?
        {
            return Ok(None);
        }
        // the map holds whole entries, as many as the manifest says
        let map_len = match tokio::fs::metadata(archive.map).await {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                let error = format!("{rel_map} can't be read: {error}");
                self.checks
                    .fail("map-length", archive.record(rel_map, error))
                    .await?;
                return Ok(None);
            }
        };
        let expected_len = expected.map(|entry| entry.entries * MAP_ENTRY_LEN);
        if map_len % MAP_ENTRY_LEN != 0 || expected_len.is_some_and(|len| len != map_len) {
            let wanted = expected_len.map_or_else(
                || format!("a multiple of {MAP_ENTRY_LEN}"),
                |len| len.to_string(),
            );
            let error = format!("{rel_map} is {map_len} bytes long; it should be {wanted}");
            self.checks
                .fail("map-length", archive.record(rel_map, error))
                .await?;
            return Ok(None);
        }
        self.checks.pass(archive.component, "map-length");
        // the map's bytes are the ones the backup wrote
        if let Some(entry) = expected {
            let sha256 = file_sha256(archive.map).await?;
            if sha256 != entry.map_sha256 {
                let error = format!(
                    "{rel_map} has sha256 {sha256}; the manifest says {}",
                    entry.map_sha256
                );
                self.checks
                    .fail("map-sha256", archive.record(rel_map, error))
                    .await?;
                return Ok(None);
            }
            self.checks.pass(archive.component, "map-sha256");
        }
        // the data file is as long as the manifest says
        let data_len = match tokio::fs::metadata(archive.data).await {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                let error = format!("{rel_data} can't be read: {error}");
                self.checks
                    .fail("data-length", archive.record(rel_data, error))
                    .await?;
                return Ok(None);
            }
        };
        if let Some(entry) = expected
            && entry.data_len != data_len
        {
            let error = format!(
                "{rel_data} is {data_len} bytes long; the manifest says {}",
                entry.data_len
            );
            self.checks
                .fail("data-length", archive.record(rel_data, error))
                .await?;
            return Ok(None);
        }
        Ok(Some(data_len))
    }

    /// Checks every partition of an archive against its checksum and decodes it, returning the
    /// rows the archive holds and where its last partition ends when every partition passed
    ///
    /// # Arguments
    ///
    /// * `archive` - The archive's files
    /// * `counter` - Decodes one of the table's partitions, in its format, and counts its rows
    async fn check_partitions(
        &self,
        archive: &ArchiveFiles<'_>,
        counter: Counter,
    ) -> Result<Option<(u64, u64)>, Error> {
        let rel_map = &archive.rel_map;
        let mut reader = ArchiveReader::new(archive.map.to_path_buf()).await?;
        let (mut rows, mut end) = (0_u64, 0_u64);
        loop {
            // stop after the current partition once --abort-on-failure stopped the run
            if self.checks.stopped() {
                return Ok(None);
            }
            // a map or data file the reader finds damaged names the file and the offset
            let (bytes, entry) = match reader.next_partition().await {
                Ok(Some(partition)) => partition,
                Ok(None) => return Ok(Some((rows, end))),
                Err(Error::Corruption {
                    path,
                    offset,
                    problem: what,
                }) => {
                    let key = relative(archive.root, &path);
                    let check = if path == archive.map {
                        "map-length"
                    } else {
                        "data-length"
                    };
                    let error = format!("{key} is damaged at byte {offset}: {what}");
                    self.checks.fail(check, archive.record(&key, error)).await?;
                    return Ok(None);
                }
                Err(error) => {
                    let error = format!("{rel_map} can't be read: {error}");
                    self.checks
                        .fail("partition-sha256", archive.record(rel_map, error))
                        .await?;
                    return Ok(None);
                }
            };
            // the partition's bytes are the ones the backup wrote
            if let Err(mismatch) = check_partition(bytes, &entry) {
                let record = archive
                    .record(rel_map, format!("{rel_map}: {mismatch}"))
                    .object(entry.partition.to_string());
                self.checks.fail("partition-sha256", record).await?;
                return Ok(None);
            }
            self.checks.pass(archive.component, "partition-sha256");
            // and decode with the table's archive struct
            match (counter.count)(bytes, counter.format) {
                Ok(count) => rows += count,
                Err(error) => {
                    let error = format!(
                        "{rel_map}: partition {} doesn't decode: {error}",
                        entry.partition
                    );
                    let record = archive
                        .record(rel_map, error)
                        .object(entry.partition.to_string());
                    self.checks.fail("rows", record).await?;
                    return Ok(None);
                }
            }
            end = entry.end;
        }
    }

    /// Returns an S3 component, its index, and its objects directory, all at the fixed places
    /// restore reads them from, or `None` once a failure that stops its checks is recorded
    ///
    /// # Arguments
    ///
    /// * `root` - The backup root
    /// * `name` - The S3 component's name
    /// * `entry` - The component's manifest entry
    async fn component_paths(
        &self,
        root: &Path,
        name: &str,
        entry: &S3Entry,
    ) -> Result<Option<(BackupComponents, PathBuf, PathBuf)>, Error> {
        // the component must be one this thoradm backs up to a directory of its own
        let known = BackupComponents::from_str(name, true)
            .ok()
            .and_then(|component| Some((component, s3_component_dir(component)?)));
        let Some((component, dir)) = known else {
            let record = problem(
                name,
                format!("the manifest lists {name}, a component this thoradm doesn't know"),
            )
            .key(MANIFEST_FILE);
            self.checks.fail("coverage", record).await?;
            return Ok(None);
        };
        // the index is always in the component's own directory, where restore reads its objects
        let wanted = format!("{dir}/{INDEX_FILE}");
        if entry.index != wanted {
            let record = problem(
                name,
                format!(
                    "the manifest names {:?} as the index of {name}, which is always {wanted}",
                    entry.index
                ),
            )
            .key(MANIFEST_FILE);
            self.checks.fail("manifest", record).await?;
            return Ok(None);
        }
        let component_dir = root.join(dir);
        let index = component_dir.join(INDEX_FILE);
        let objects_dir = component_dir.join("objects");
        // a linked or unreadable component directory, index, or objects directory isn't followed
        if self
            .refuse_links(root, name, None, &[&component_dir, &index, &objects_dir])
            .await?
        {
            return Ok(None);
        }
        Ok(Some((component, index, objects_dir)))
    }

    /// Checks one S3 component's objects: its index is the one the manifest pins, every object
    /// it lists is in the backup with its size and sha256, and nothing else is
    ///
    /// # Arguments
    ///
    /// * `root` - The backup root
    /// * `name` - The S3 component's name
    /// * `entry` - The component's manifest entry
    /// * `buckets` - The bucket name of each bucket role when the backup ran
    async fn check_objects(
        &self,
        root: &Path,
        name: &str,
        entry: &S3Entry,
        buckets: &BTreeMap<String, String>,
    ) -> Result<(), Error> {
        // the component's index and objects, from its fixed directory in the backup
        let Some((component, index, objects_dir)) = self.component_paths(root, name, entry).await?
        else {
            return Ok(());
        };
        // the index is the one the manifest pins
        let index_sha256 = if tokio::fs::try_exists(&index).await? {
            file_sha256(&index).await?
        } else {
            String::new()
        };
        if index_sha256 == entry.index_sha256 {
            self.checks.pass(name, "index-sha256");
        } else {
            let record = problem(
                name,
                format!(
                    "{} has sha256 {index_sha256:?}; the manifest says {}",
                    entry.index, entry.index_sha256
                ),
            )
            .key(&entry.index);
            self.checks.fail("index-sha256", record).await?;
            if self.checks.stopped() {
                return Ok(());
            }
        }
        // every object the index lists, where the backup keeps it
        let roles: BTreeMap<&str, &str> = buckets
            .iter()
            .map(|(role, bucket)| (bucket.as_str(), role.as_str()))
            .collect();
        let lines = manifest::read_index(&index).await?;
        let mut expected = HashSet::new();
        let mut listed = Vec::new();
        let (mut count, mut bytes) = (0_u64, 0_u64);
        for ((bucket, key), (size, sha256)) in lines {
            count += 1;
            bytes += size;
            // a key that can't be kept as a path inside objects/ is never turned into one
            if !keepable_key(&key) {
                let error = format!(
                    "{} lists the key {key:?}, which can't be kept as a path inside the backup",
                    entry.index
                );
                let record = problem(name, error).bucket(&bucket).key(&entry.index);
                self.checks.fail("object", record).await?;
                continue;
            }
            let role = roles.get(bucket.as_str()).copied().unwrap_or("unknown");
            let path = objects_dir.join(object_relative_path(component, role, &key));
            expected.insert(path.clone());
            listed.push((path, bucket, key, size, sha256));
        }
        listed.sort();
        stream::iter(listed)
            .map(|(path, bucket, key, size, sha256)| async move {
                self.check_object(root, name, &path, &bucket, &key, size, &sha256)
                    .await
            })
            .buffer_unordered(self.workers)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<()>, Error>>()?;
        // no file is under objects/ that the index doesn't list, since restore would upload it
        self.check_unlisted_objects(root, name, &objects_dir, &expected)
            .await?;
        // the index lists as many objects and bytes as the manifest says
        if count == entry.objects && bytes == entry.bytes {
            self.checks.pass(name, "object-count");
        } else if !self.checks.stopped() {
            let record = problem(
                name,
                format!(
                    "{} lists {count} objects of {bytes} bytes; the manifest says {} objects of {} bytes",
                    entry.index, entry.objects, entry.bytes
                ),
            )
            .key(&entry.index);
            self.checks.fail("object-count", record).await?;
        }
        Ok(())
    }

    /// Records every file under an S3 component's `objects/` that its index doesn't list
    ///
    /// # Arguments
    ///
    /// * `root` - The backup root
    /// * `name` - The S3 component's name
    /// * `objects_dir` - The component's `objects/` directory
    /// * `expected` - Where the index's objects are kept
    async fn check_unlisted_objects(
        &self,
        root: &Path,
        name: &str,
        objects_dir: &Path,
        expected: &HashSet<PathBuf>,
    ) -> Result<(), Error> {
        // walk every directory under objects/
        let mut pending = vec![objects_dir.to_path_buf()];
        let mut strays = Vec::new();
        while let Some(dir) = pending.pop() {
            let Ok(mut listing) = tokio::fs::read_dir(&dir).await else {
                continue;
            };
            while let Some(item) = listing.next_entry().await? {
                if item.file_type().await?.is_dir() {
                    pending.push(item.path());
                } else if !expected.contains(&item.path()) {
                    strays.push(item.path());
                }
            }
        }
        // record each file no index entry names, in path order
        strays.sort();
        for stray in strays {
            if self.checks.stopped() {
                return Ok(());
            }
            let key = relative(root, &stray);
            let error = format!("{key} is an object the index doesn't list");
            self.checks
                .fail("unlisted-object", problem(name, error).key(key))
                .await?;
        }
        Ok(())
    }

    /// Checks one object the index lists: it is in the backup with its size and sha256
    ///
    /// # Arguments
    ///
    /// * `root` - The backup root
    /// * `name` - The S3 component's name
    /// * `path` - Where the backup keeps the object
    /// * `bucket` - The bucket it came from
    /// * `key` - Its key
    /// * `size` - Its size in the index
    /// * `sha256` - Its sha256 in the index
    #[allow(clippy::too_many_arguments)]
    async fn check_object(
        &self,
        root: &Path,
        name: &str,
        path: &Path,
        bucket: &str,
        key: &str,
        size: u64,
        sha256: &str,
    ) -> Result<(), Error> {
        // take no new object once --abort-on-failure stopped the run
        if self.checks.stopped() {
            return Ok(());
        }
        let rel = relative(root, path);
        let fail = |error: String| {
            let record = problem(name, error).bucket(bucket).key(&rel).object(key);
            self.checks.fail("object", record)
        };
        // a linked object isn't followed, and restore doesn't upload it
        match is_link(path).await {
            Ok(false) => (),
            Ok(true) => {
                return fail(format!(
                    "{rel} is a symbolic link; a backup holds only real files, so it isn't followed"
                ))
                .await;
            }
            Err(error) => {
                return fail(format!(
                    "{rel} is listed in the index but can't be read: {error}"
                ))
                .await;
            }
        }
        // the object is there, as long as it was
        let found = match tokio::fs::metadata(path).await {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                return fail(format!(
                    "{rel} is listed in the index but can't be read: {error}"
                ))
                .await;
            }
        };
        if found != size {
            return fail(format!(
                "{rel} is {found} bytes long; the index says {size}"
            ))
            .await;
        }
        // and holds the bytes the backup copied
        let found = match file_sha256(path).await {
            Ok(found) => found,
            Err(error) => return fail(format!("{rel} can't be read: {error}")).await,
        };
        if found != sha256 {
            return fail(format!("{rel} has sha256 {found}; the index says {sha256}")).await;
        }
        self.checks.pass(name, "object");
        Ok(())
    }

    /// Checks `redis.json`: its length and sha256 are the manifest's, it parses as the API's
    /// backup, and it holds as many records of each type as the manifest says
    ///
    /// # Arguments
    ///
    /// * `root` - The backup root
    /// * `entry` - The manifest's Redis entry
    async fn check_redis(&self, root: &Path, entry: &RedisEntry) -> Result<(), Error> {
        let component = BackupComponents::Redis.to_string();
        let path = root.join("redis.json");
        // a linked redis.json isn't followed
        if self.refuse_links(root, &component, None, &[&path]).await? {
            return Ok(());
        }
        // compare what is there with what the backup wrote
        let found = match tokio::fs::read(&path).await {
            Ok(bytes) => redis_problem(&bytes, entry),
            Err(error) => Some(format!("redis.json can't be read: {error}")),
        };
        match found {
            None => {
                self.checks.pass(&component, "redis-json");
                Ok(())
            }
            Some(error) => {
                let record = problem(&component, error).key("redis.json");
                self.checks.fail("redis-json", record).await
            }
        }
    }

    /// Checks every sibling file the manifest lists, and that no known sibling is there unlisted
    ///
    /// # Arguments
    ///
    /// * `root` - The backup root
    /// * `manifest` - The backup's manifest
    async fn check_siblings(&self, root: &Path, manifest: &Manifest) -> Result<(), Error> {
        // each listed sibling is there with its length and sha256
        for sibling in &manifest.siblings {
            // only the files a backup writes beside its components are siblings
            if !SIBLINGS.contains(&sibling.name.as_str()) {
                let error = format!(
                    "the manifest lists {:?} as a sibling, which isn't a file a backup writes",
                    sibling.name
                );
                let record = problem(SIBLINGS_COMPONENT, error).key(MANIFEST_FILE);
                self.checks.fail("sibling", record).await?;
                if self.checks.stopped() {
                    return Ok(());
                }
                continue;
            }
            let path = root.join(&sibling.name);
            // a linked sibling isn't followed
            if self
                .refuse_links(root, SIBLINGS_COMPONENT, None, &[&path])
                .await?
            {
                continue;
            }
            let found = if tokio::fs::try_exists(&path).await? {
                let len = tokio::fs::metadata(&path).await?.len();
                let sha256 = file_sha256(&path).await?;
                (len != sibling.len || sha256 != sibling.sha256).then(|| {
                    format!(
                        "{} is {len} bytes with sha256 {sha256}; the manifest says {} bytes with sha256 {}",
                        sibling.name, sibling.len, sibling.sha256
                    )
                })
            } else {
                Some(format!(
                    "{} is listed in the manifest but missing",
                    sibling.name
                ))
            };
            match found {
                None => self.checks.pass(SIBLINGS_COMPONENT, "sibling"),
                Some(error) => {
                    let record = problem(SIBLINGS_COMPONENT, error).key(&sibling.name);
                    self.checks.fail("sibling", record).await?;
                    if self.checks.stopped() {
                        return Ok(());
                    }
                }
            }
        }
        // a sibling another feature writes isn't there without being listed
        for name in SIBLINGS {
            let listed = manifest
                .siblings
                .iter()
                .any(|sibling| sibling.name == *name);
            if !listed && tokio::fs::try_exists(root.join(name)).await? {
                let record = problem(
                    SIBLINGS_COMPONENT,
                    format!("{name} is in the backup but not in its manifest"),
                )
                .key(*name);
                self.checks.fail("sibling", record).await?;
            }
        }
        Ok(())
    }

    /// Checks `pruned.jsonl`, the list of what `--prune-dangling` dropped from the Redis state,
    /// against the manifest: it is there with the manifest's sha256 when the manifest lists it,
    /// and absent when it doesn't; a backup that pruned nothing has nothing to check
    ///
    /// # Arguments
    ///
    /// * `root` - The backup root
    /// * `manifest` - The backup's manifest
    async fn check_pruned(&self, root: &Path, manifest: &Manifest) -> Result<(), Error> {
        let path = root.join(PRUNED_FILE);
        // a linked pruned.jsonl isn't followed, since hashing it would vouch for a file elsewhere
        if self
            .refuse_links(root, MANIFEST_COMPONENT, None, &[&path])
            .await?
        {
            return Ok(());
        }
        // what is there, and what the manifest pins
        let found = if tokio::fs::try_exists(&path).await? {
            Some(file_sha256(&path).await?)
        } else {
            None
        };
        let expected = manifest.pruned.as_ref().map(|entry| entry.sha256.clone());
        // a backup that pruned nothing has neither
        if found.is_none() && expected.is_none() {
            return Ok(());
        }
        // the file is the one the manifest pins
        if found == expected {
            self.checks.pass(MANIFEST_COMPONENT, "pruned-sha256");
            return Ok(());
        }
        // otherwise say which side is missing or how they differ
        let error = match (&found, &expected) {
            (Some(_), None) => format!("{PRUNED_FILE} is in the backup but not in its manifest"),
            (None, Some(_)) => format!("{PRUNED_FILE} is listed in the manifest but missing"),
            _ => format!("{PRUNED_FILE} has sha256 {found:?}; the manifest says {expected:?}"),
        };
        let record = problem(MANIFEST_COMPONENT, error).key(PRUNED_FILE);
        self.checks.fail("pruned-sha256", record).await
    }

    /// Checks the backup's own `failures.jsonl` against the manifest, and records each object
    /// the backup recorded as failed or skipped, since the backup doesn't hold it
    ///
    /// # Arguments
    ///
    /// * `root` - The backup root
    /// * `manifest` - The backup's manifest
    /// * `list_recorded` - Whether to record each object the backup recorded as failed
    async fn check_recorded_failures(
        &self,
        root: &Path,
        manifest: &Manifest,
        list_recorded: bool,
    ) -> Result<(), Error> {
        let path = root.join(FAILURES_FILE);
        // a linked failures.jsonl isn't followed, since its records would come from elsewhere
        if self
            .refuse_links(root, MANIFEST_COMPONENT, None, &[&path])
            .await?
        {
            return Ok(());
        }
        // the failures file is the one the manifest pins: absent or empty when nothing failed
        let found = if tokio::fs::try_exists(&path).await? {
            let len = tokio::fs::metadata(&path).await?.len();
            (len > 0).then_some(file_sha256(&path).await?)
        } else {
            None
        };
        if found == manifest.failures_sha256 {
            self.checks.pass(MANIFEST_COMPONENT, "failures-sha256");
        } else {
            let record = problem(
                MANIFEST_COMPONENT,
                format!(
                    "{FAILURES_FILE} has sha256 {found:?}; the manifest says {:?}",
                    manifest.failures_sha256
                ),
            )
            .key(FAILURES_FILE);
            self.checks.fail("failures-sha256", record).await?;
            if self.checks.stopped() {
                return Ok(());
            }
        }
        // a backup that recorded failures is missing those objects, unless the caller checks that
        if !list_recorded {
            return Ok(());
        }
        let recorded = manifest.failures + manifest.skipped;
        if recorded == 0 {
            self.checks.pass(MANIFEST_COMPONENT, "recorded-failures");
            return Ok(());
        }
        let records = super::failures::read_records(&path).unwrap_or_default();
        if records.is_empty() {
            let record = problem(MANIFEST_COMPONENT, format!("the manifest records {recorded} failed or skipped objects, but {FAILURES_FILE} lists none")).key(FAILURES_FILE);
            return self.checks.fail("recorded-failures", record).await;
        }
        for original in records {
            if self.checks.stopped() {
                return Ok(());
            }
            let record = missing_object(&original);
            self.checks.fail("recorded-failures", record).await?;
        }
        Ok(())
    }

    /// Builds the report of the checks run so far
    ///
    /// # Arguments
    ///
    /// * `backup` - The backup checked
    /// * `format_version` - Its format version, or `None` without a manifest
    /// * `complete_backup` - Whether it has a manifest
    /// * `started` - When verify started
    #[must_use]
    pub fn report(
        &self,
        backup: &Path,
        format_version: Option<u32>,
        complete_backup: bool,
        started: DateTime<Utc>,
    ) -> VerifyReport {
        // every check, and whether the run stopped early
        VerifyReport {
            mode: VerifyMode::Backup,
            backup: backup.to_path_buf(),
            format_version,
            complete_backup,
            checks: self.checks.snapshot(),
            stopped_early: self.checks.stopped(),
            started,
            finished: Utc::now(),
        }
    }
}

/// Builds the verify record of an object a backup recorded as failed or skipped
///
/// # Arguments
///
/// * `original` - The backup's record
#[must_use]
pub fn missing_object(original: &FailureRecord) -> FailureRecord {
    // keep everything that names the object, so the record says what the backup lacks
    let mut record = problem(
        &original.component,
        format!(
            "the backup doesn't hold this object: its {:?} {:?} failed with: {}",
            original.kind, original.operation, original.error
        ),
    );
    // copy where the object lives from the backup's record
    record.table.clone_from(&original.table);
    record.bucket.clone_from(&original.bucket);
    record.token_range = original.token_range;
    record.key.clone_from(&original.key);
    record.object.clone_from(&original.object);
    record
}

/// Returns what is wrong with a `redis.json`, or `None` when it matches its manifest entry
///
/// # Arguments
///
/// * `bytes` - The file's contents
/// * `entry` - The manifest's Redis entry
fn redis_problem(bytes: &[u8], entry: &RedisEntry) -> Option<String> {
    // the length and sha256 are the ones the backup wrote
    let sha256 = HEXLOWER.encode(&Sha256::digest(bytes));
    if bytes.len() as u64 != entry.len || sha256 != entry.sha256 {
        return Some(format!(
            "redis.json is {} bytes with sha256 {sha256}; the manifest says {} bytes with sha256 {}",
            bytes.len(),
            entry.len,
            entry.sha256
        ));
    }
    // it parses as the API's backup, holding the records the manifest counts
    let backup: thorium::models::Backup = match serde_json::from_slice(bytes) {
        Ok(backup) => backup,
        Err(error) => {
            return Some(format!(
                "redis.json doesn't parse as a Redis backup: {error}"
            ));
        }
    };
    let counts: BTreeMap<String, u64> = [
        ("groups", backup.groups.len()),
        ("images", backup.images.len()),
        ("pipelines", backup.pipelines.len()),
        ("users", backup.users.len()),
    ]
    .into_iter()
    .map(|(name, count)| (name.to_owned(), count as u64))
    .collect();
    (counts != entry.counts).then(|| {
        format!(
            "redis.json holds {counts:?} records; the manifest says {:?}",
            entry.counts
        )
    })
}

/// Checks a backup directory and returns what it found
///
/// A finished backup gets every check its manifest makes possible, unless its manifest can't be
/// read or is of a newer format version; an interrupted or empty one is a problem of its own; a
/// legacy backup is a problem too, and its archives are still checked.
///
/// # Arguments
///
/// * `root` - The backup root
/// * `verifier` - The verifier to check with
///
/// # Returns
///
/// The backup's format version, whether it has a manifest, and whether verify could check its
/// completeness at all
pub async fn check_backup(
    root: &Path,
    verifier: &Verifier,
) -> Result<(Option<u32>, bool, bool), Error> {
    // what the directory holds decides what can be checked
    match manifest::classify(root).await? {
        Layout::Finished => match Manifest::read(root).await {
            Ok(Some(manifest)) => {
                verifier.manifest_checks(root, &manifest, true).await?;
                Ok((Some(manifest.format_version), true, true))
            }
            Ok(None) => {
                verifier
                    .manifest_problem(root, "the manifest disappeared while verify read it")
                    .await?;
                Ok((None, false, false))
            }
            Err(error) => {
                verifier.manifest_problem(root, error).await?;
                Ok((None, false, false))
            }
        },
        Layout::Interrupted(marker) => {
            let reason = format!(
                "{} holds a backup that was interrupted (backup-in-progress.json from a run started at {}) and is incomplete; run the same backup new into it again to finish it",
                root.display(),
                marker.started
            );
            verifier.manifest_problem(root, reason).await?;
            Ok((None, false, false))
        }
        Layout::Legacy => {
            let reason = format!(
                "{} has no {MANIFEST_FILE}: a legacy backup, or files of no backup; its completeness can't be checked, so only its archives are",
                root.display()
            );
            verifier.manifest_problem(root, reason).await?;
            verifier.archive_checks(root).await?;
            Ok((None, false, false))
        }
        Layout::Empty => {
            verifier
                .manifest_problem(root, format!("{} holds no backup", root.display()))
                .await?;
            Ok((None, false, false))
        }
    }
}

/// Logs one summary line per check
///
/// # Arguments
///
/// * `report` - The report to sum up
pub fn log_checks(report: &VerifyReport) {
    for check in &report.checks {
        // failed checks stand out; every failure is also in failures.jsonl
        if check.failed > 0 {
            warn!(
                component = check.component,
                check = check.check,
                checked = check.checked,
                failed = check.failed,
                "{} {}: {} checked, {} failed",
                check.component,
                check.check,
                check.checked,
                check.failed
            );
        } else {
            info!(
                component = check.component,
                check = check.check,
                checked = check.checked,
                failed = check.failed,
                "{} {}: {} checked, {} failed",
                check.component,
                check.check,
                check.checked,
                check.failed
            );
        }
    }
}

/// Opens the report directory a run names, or makes a fresh one under the backup
///
/// # Arguments
///
/// * `backup` - The backup being read
/// * `given` - The directory `--report-dir` named, if any
/// * `operation` - The run's operation (`verify`, `restore`)
pub async fn open_report_dir(
    backup: &Path,
    given: Option<&Path>,
    operation: &str,
) -> Result<PathBuf, Error> {
    // a named directory is created private if it is missing; otherwise one goes in the backup
    match given {
        Some(dir) => {
            create_private_dir(dir).await?;
            Ok(dir.to_path_buf())
        }
        None => report_dir(backup, operation).await,
    }
}

/// Verifies a backup directory and returns how the run ended
///
/// # Arguments
///
/// * `args` - The `backup verify` arguments
/// * `workers` - How many archives or objects are checked at once
/// * `sinks` - The file-system seam the report is written through
pub async fn run(
    args: &VerifyBackup,
    workers: usize,
    sinks: Arc<dyn SinkFactory>,
) -> Result<RunOutcome, Error> {
    // record failures in a report directory so the backup's own files never change
    let reports = open_report_dir(&args.backup, args.report_dir.as_deref(), "verify").await?;
    let policy = FailurePolicy::from_flag(args.abort_on_failure);
    let log = Arc::new(FailureLog::open(&reports, policy, sinks.as_ref()).await?);
    let started = Utc::now();
    info!("Verifying {}", args.backup.display());
    // check everything the backup allows
    let verifier = Verifier::new(log.clone(), workers);
    let (format_version, complete, checked) = check_backup(&args.backup, &verifier).await?;
    // sum it up in verify.json and the log
    let report = verifier.report(&args.backup, format_version, complete, started);
    manifest::write_json_atomic(&reports, REPORT_FILE, &report, sinks.as_ref()).await?;
    log_checks(&report);
    // a backup whose completeness couldn't be checked can't pass
    let outcome = if checked {
        report.outcome()
    } else {
        RunOutcome::Aborted
    };
    match outcome {
        RunOutcome::Complete => info!("verify complete: the backup matches its manifest"),
        RunOutcome::CompletedWithFailures => warn!(
            "verify finished with {} failed checks, listed in {}",
            report.failed(),
            log.path().display()
        ),
        RunOutcome::Aborted => warn!(
            "verify stopped: {} failed checks, listed in {}",
            report.failed(),
            log.path().display()
        ),
    }
    Ok(outcome)
}

/// Handle the backup verify command
///
/// # Arguments
///
/// * `args` - The `backup verify` arguments
/// * `thoradm` - Thoradm's arguments, naming the configs of the cluster to check
pub async fn handle(args: &VerifyBackup, thoradm: &Args) -> Result<RunOutcome, Error> {
    // a backup directory needs no cluster
    if !args.cluster {
        return run(args, thoradm.workers, Arc::new(TokioFs)).await;
    }
    // a cluster is reached through its config and its API
    let conf = thorium::Conf::new(&thoradm.cluster_conf)?;
    let ctl_conf = thorium::CtlConf::from_path(&thoradm.ctl_conf)?;
    let scylla = crate::shared::scylla::get_client(&conf).await?;
    Box::pin(super::verify_cluster::run(
        args,
        &conf,
        &ctl_conf,
        &scylla,
        thoradm.workers,
        Arc::new(TokioFs),
    ))
    .await
}

#[cfg(test)]
mod tests {
    use super::{TABLES, archive_struct_names, component_table, s3_component_dir, table_component};
    use crate::args::BackupComponents;
    use crate::backup::Utils;
    use crate::backup::coverage::{COVERAGE, Coverage, Store};
    use crate::backup::graphics::Graphic;
    use crate::backup::tables::{Comment, Output, S3Id};

    /// Every backed-up table has a row counter under its archive struct's name, and maps to its
    /// component and back
    #[test]
    fn every_backed_up_table_has_a_row_counter() {
        // the registry's backed-up tables
        let mut backed_up: Vec<&str> = COVERAGE
            .iter()
            .filter(|entry| {
                entry.store == Store::ScyllaTable && matches!(entry.coverage, Coverage::BackedUp(_))
            })
            .map(|entry| entry.name)
            .collect();
        backed_up.sort_unstable();
        // the counters' tables, which the archive structs name the same way
        let mut counted: Vec<&str> = TABLES.iter().map(|(name, _)| *name).collect();
        assert_eq!(counted, archive_struct_names());
        counted.sort_unstable();
        assert_eq!(counted, backed_up);
        // each table's component backs up that table
        for table in backed_up {
            let component: BackupComponents =
                clap::ValueEnum::from_str(&table_component(table), true).unwrap();
            assert_eq!(component_table(component), Some(table));
        }
    }

    /// Every S3 component's directory is the one its backup writes and its restore reads
    #[test]
    fn every_s3_component_has_its_directory() {
        // the directory each S3 component's backup and restore use
        let dirs = [
            (BackupComponents::S3IdsObjects, S3Id::name()),
            (BackupComponents::CommentAttachments, Comment::name()),
            (BackupComponents::ResultFiles, Output::name()),
            (BackupComponents::Graphics, Graphic::name()),
        ];
        for (component, dir) in dirs {
            assert_eq!(s3_component_dir(component), Some(dir), "{component}");
        }
        // every S3 component is in that list, and no other component has a directory
        for component in <BackupComponents as clap::ValueEnum>::value_variants() {
            let listed = dirs.iter().any(|(listed, _)| listed == component);
            assert_eq!(listed, component.is_in_s3(), "{component}");
            assert_eq!(
                s3_component_dir(*component).is_some(),
                listed,
                "{component}"
            );
        }
    }
}
