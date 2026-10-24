//! The manifest a finished backup writes describes exactly what the backup holds, and a backup
//! in a newer format is refused

mod common;

use common::archives;
use common::harness::Harness;
use data_encoding::HEXLOWER;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use thoradm::backup::coverage::{Registry, Store};
use thoradm::backup::failures::Operation;
use thoradm::backup::manifest::{
    FORMAT_VERSION, Manifest, ObjectIndexLine, S3Entry, SiblingEntry, TableEntry,
};
use thoradm::backup::tables::{
    Association, Comment, Commitish, CommitishList, Entity, NetworkPolicy, Node, Notification,
    Output, OutputStream, RepoData, RepoList, S3Id, SamplesList, Tag,
};

/// Returns the lowercase hex sha256 of a file
///
/// # Arguments
///
/// * `path` - The file to hash
fn sha256_of(path: &Path) -> String {
    // hash the whole file
    HEXLOWER.encode(&Sha256::digest(std::fs::read(path).unwrap()))
}

/// Counts the rows a table's archives hold by reading every partition
///
/// # Arguments
///
/// * `out` - The backup directory
/// * `table` - The table's name
async fn archived_rows(out: &Path, table: &str) -> usize {
    // read the archives with the table's own row type
    let dir = out.join(table);
    match table {
        "associations" => archives::rows::<Association, _, _>(&dir, |_| ())
            .await
            .len(),
        "comments" => archives::rows::<Comment, _, _>(&dir, |_| ()).await.len(),
        "commitishes" => archives::rows::<Commitish, _, _>(&dir, |_| ()).await.len(),
        "commitish_list" => archives::rows::<CommitishList, _, _>(&dir, |_| ())
            .await
            .len(),
        "entities" => archives::rows::<Entity, _, _>(&dir, |_| ()).await.len(),
        "network_policies" => archives::rows::<NetworkPolicy, _, _>(&dir, |_| ())
            .await
            .len(),
        "nodes" => archives::rows::<Node, _, _>(&dir, |_| ()).await.len(),
        "notifications" => archives::rows::<Notification, _, _>(&dir, |_| ())
            .await
            .len(),
        "repo_data" => archives::rows::<RepoData, _, _>(&dir, |_| ()).await.len(),
        "repos_list" => archives::rows::<RepoList, _, _>(&dir, |_| ()).await.len(),
        "results" => archives::rows::<Output, _, _>(&dir, |_| ()).await.len(),
        "results_stream" => archives::rows::<OutputStream, _, _>(&dir, |_| ())
            .await
            .len(),
        "s3_ids" => archives::rows::<S3Id, _, _>(&dir, |_| ()).await.len(),
        "samples_list" => archives::rows::<SamplesList, _, _>(&dir, |_| ())
            .await
            .len(),
        "tags" => archives::rows::<Tag, _, _>(&dir, |_| ()).await.len(),
        other => panic!("no row type for table {other}"),
    }
}

/// Returns every file under a directory
///
/// # Arguments
///
/// * `dir` - The directory to walk
fn files_under(dir: &Path) -> Vec<PathBuf> {
    // walk depth first and keep every regular file
    let mut files = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        let Ok(listing) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in listing.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files
}

/// Asserts one table's manifest entry matches its files, its archives' own rows, and the
/// registry
///
/// # Arguments
///
/// * `out` - The backup directory
/// * `table` - The table's name
/// * `entry` - The table's manifest entry
async fn check_table(out: &Path, table: &str, entry: &TableEntry) {
    let dir = out.join(table);
    // the map directory holds exactly the listed archives
    let listed: BTreeSet<String> = entry
        .archives
        .iter()
        .map(|archive| format!("{}.thoriummap", archive.uuid))
        .collect();
    let on_disk: BTreeSet<String> = archives::maps(&dir)
        .iter()
        .map(|map| map.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        listed, on_disk,
        "{table}: listed and stored archives differ"
    );
    for archive in &entry.archives {
        let map = dir
            .join("maps")
            .join(format!("{}.thoriummap", archive.uuid));
        let data = dir.join("data").join(archive.uuid.to_string());
        let (map_len, data_len) = (
            std::fs::metadata(&map).unwrap().len(),
            std::fs::metadata(&data).unwrap().len(),
        );
        assert_eq!(map_len, archive.entries * 96, "{table}");
        assert_eq!(data_len, archive.data_len, "{table}");
        assert_eq!(sha256_of(&map), archive.map_sha256, "{table}");
    }
    // the ranges cover the ring, and every count agrees with the archives' own rows
    let mut next = i128::from(i64::MIN);
    for range in &entry.ranges {
        assert_eq!(
            i128::from(range.start),
            next,
            "{table}: ranges aren't contiguous"
        );
        next = i128::from(range.end) + 1;
    }
    assert_eq!(
        next,
        i128::from(i64::MAX) + 1,
        "{table}: ranges stop short of the ring"
    );
    let range_rows: u64 = entry.ranges.iter().map(|range| range.rows).sum();
    let archive_rows: u64 = entry.archives.iter().map(|archive| archive.rows).sum();
    assert_eq!(range_rows, entry.rows, "{table}");
    assert_eq!(archive_rows, entry.rows, "{table}");
    let counted = archived_rows(out, table).await as u64;
    assert_eq!(counted, entry.rows, "{table}");
    assert_eq!(entry.skipped, 0, "{table}");
    // the columns are the registry's
    let archived = Registry::classify(Store::ScyllaTable, table)
        .and_then(|registered| registered.columns)
        .map_or_else(Vec::new, |columns| columns.archived.to_vec());
    assert_eq!(entry.columns, archived, "{table}");
}

/// Asserts one S3 component's index is pinned by the manifest and each line matches its object
/// and its file
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `out` - The backup directory
/// * `component` - The component's name
/// * `entry` - The component's manifest entry
async fn check_s3(harness: &Harness, out: &Path, component: &str, entry: &S3Entry) {
    // the index is the one the manifest pins, and its lines add up to the entry
    let index = out.join(&entry.index);
    assert_eq!(sha256_of(&index), entry.index_sha256, "{component}");
    let lines: Vec<ObjectIndexLine> = std::fs::read_to_string(&index)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len() as u64, entry.objects, "{component}");
    let bytes: u64 = lines.iter().map(|line| line.size).sum();
    assert_eq!(bytes, entry.bytes, "{component}");
    assert_ne!(lines.len(), 0, "{component} indexed nothing");
    // each line matches a HEAD of its object and the sha256 of its file
    let s3 = harness.s3();
    let files = files_under(index.parent().unwrap().join("objects").as_path());
    for line in &lines {
        let head = s3
            .head_object()
            .bucket(&line.bucket)
            .key(&line.key)
            .send()
            .await
            .unwrap_or_else(|err| panic!("{component}: no object {}: {err}", line.key));
        assert_eq!(
            head.content_length(),
            i64::try_from(line.size).ok(),
            "{}",
            line.key
        );
        assert_eq!(head.e_tag(), Some(line.etag.as_str()), "{}", line.key);
        let file = files
            .iter()
            .find(|file| file.to_string_lossy().ends_with(&line.key))
            .unwrap_or_else(|| panic!("{component}: no file for {}", line.key));
        assert_eq!(sha256_of(file), line.sha256, "{}", line.key);
    }
}

/// After a backup of the broad fixture, every count and checksum in the manifest matches the
/// files, the stores, and the archives' own rows
#[tokio::test]
async fn the_manifest_matches_what_the_backup_holds() {
    let harness = Harness::get().await;
    let _guard = harness.shared_guard().await;
    common::fixture::broad(harness).await;
    // back up every component
    let out = harness.scratch("full");
    let output = harness
        .thoradm()
        .args(["--workers", "2", "backup", "new", "-m", "2", "-o"])
        .arg(&out)
        .output()
        .expect("failed to run thoradm");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    let manifest = Manifest::read(&out).await.unwrap().expect("no manifest");
    // the run, its producer, and its cluster
    assert_eq!(manifest.format_version, FORMAT_VERSION);
    assert_eq!(manifest.namespace, harness.conf.thorium.namespace);
    assert_eq!((manifest.failures, manifest.skipped), (0, 0));
    assert_eq!(manifest.failures_sha256, None);
    assert!(manifest.started <= manifest.finished);
    let files_bucket = manifest.buckets.get("files");
    assert_eq!(files_bucket, Some(&harness.conf.thorium.files.bucket));
    assert!(manifest.partition_sizes.contains_key("files"));
    // the schema record is the one sibling, listed as the file holds it
    let schema = std::fs::read(out.join("schema.json")).unwrap();
    assert_eq!(
        manifest.siblings,
        vec![SiblingEntry {
            name: "schema.json".to_owned(),
            len: schema.len() as u64,
            sha256: HEXLOWER.encode(&Sha256::digest(&schema)),
        }]
    );
    // every table the backup ran has an entry, and each entry matches its files
    let tables: BTreeSet<&str> = Registry::backed_up(Store::ScyllaTable)
        .map(|entry| entry.name)
        .collect();
    let listed: BTreeSet<&str> = manifest.tables.keys().map(String::as_str).collect();
    assert_eq!(listed, tables);
    for (table, entry) in &manifest.tables {
        check_table(&out, table, entry).await;
    }
    // every S3 component's index is pinned, and each line matches its object and its file
    assert_eq!(manifest.s3.len(), 4, "{:?}", manifest.s3.keys());
    for (component, entry) in &manifest.s3 {
        check_s3(harness, &out, component, entry).await;
    }
    // the Redis state matches redis.json
    let redis = manifest.redis.as_ref().expect("no redis entry");
    let path = out.join("redis.json");
    assert_eq!(std::fs::metadata(&path).unwrap().len(), redis.len);
    assert_eq!(sha256_of(&path), redis.sha256);
    let state: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for kind in ["users", "groups", "images", "pipelines"] {
        let count = state[kind].as_array().map_or(0, Vec::len) as u64;
        assert_eq!(redis.counts.get(kind), Some(&count), "{kind}");
    }
}

/// The schema record a backup writes beside its archives is listed with its length and sha256
#[tokio::test]
async fn a_sibling_file_is_listed_with_its_length_and_sha256() {
    let harness = Harness::get().await;
    let _guard = harness.shared_guard().await;
    // a backup of one table
    let out = harness.scratch("sibling");
    let output = harness
        .thoradm()
        .args(["backup", "new", "tags", "-o"])
        .arg(&out)
        .output()
        .expect("failed to run thoradm");
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // the manifest lists the schema record exactly as the file holds it
    let schema = std::fs::read(out.join("schema.json")).unwrap();
    let manifest = Manifest::read(&out).await.unwrap().expect("no manifest");
    assert_eq!(
        manifest.siblings,
        vec![SiblingEntry {
            name: "schema.json".to_owned(),
            len: schema.len() as u64,
            sha256: HEXLOWER.encode(&Sha256::digest(&schema)),
        }]
    );
}

/// Restoring a backup whose manifest is a newer format version is refused before anything is
/// restored, naming the version in a preflight record
#[tokio::test]
async fn restoring_a_newer_format_is_refused() {
    let harness = Harness::get().await;
    // a refusal that failed would restore, so keep every other test of the binary out
    let _guard = harness.restore_guard().await;
    let out = harness.scratch("newer");
    let manifest = serde_json::json!({
        "format_version": FORMAT_VERSION + 1,
        "thoradm_version": "9.9.9",
    });
    std::fs::write(out.join("manifest.json"), manifest.to_string()).unwrap();
    let output = harness
        .thoradm()
        .args(["backup", "restore", "--confirm", "-b"])
        .arg(&out)
        .output()
        .expect("failed to run thoradm");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    // the refusal is a preflight record of the manifest check, naming the version and its writer
    let records = common::cycle::failure_records(&out);
    let refusal = records
        .iter()
        .find(|record| record.operation == Operation::Preflight)
        .unwrap_or_else(|| panic!("no preflight record: {records:?}\n{stderr}"));
    assert_eq!(refusal.object.as_deref(), Some("manifest"), "{refusal:?}");
    assert!(
        refusal
            .error
            .contains(&format!("format version {}", FORMAT_VERSION + 1))
            && refusal.error.contains("9.9.9"),
        "{refusal:?}"
    );
    // nothing but the restore's report was written beside the manifest
    let mut entries: Vec<String> = std::fs::read_dir(&out)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(entries, ["manifest.json", "reports"]);
}
