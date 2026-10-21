//! `backup new --accept-broken-links`: a reference to an S3 object the cluster itself lacks is
//! recorded as a broken link and kept, instead of failing the object, and a restore of a backup
//! whose only records are such links needs no `--accept-failures`
//!
//! Every test holds the binary's restore guard, so no other test's backup sees the graphics a
//! test removes or adds.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use axum::http::Method;
use common::cycle::{describe, restore_with, wipe};
use common::harness::{Harness, bucket_for, list_keys, redis_pool, write_cluster_conf};
use common::mock_s3::{MockS3, S3Behavior, S3Rule};
use common::names::Names;
use common::seed;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Output;
use thoradm::args::BackupComponents;
use thoradm::backup::failures::{
    FAILURES_FILE, FailureKind, FailureRecord, Operation, read_records,
};
use thoradm::backup::manifest::{InProgressMarker, MANIFEST_FILE, Manifest};
use thoradm::backup::sink::TokioFs;
use thorium::models::Backup;
use uuid::Uuid;

/// The components these tests back up: samples with their objects, the entities, the Redis state,
/// and the graphics the last two name
const COMPONENTS: &str = "samples-list,s3-ids,s3-ids-objects,entities,redis,graphics";

/// The guide that documents `backup new`
const TAKING_BACKUPS: &str = "../api/docs/src/admins/thoradm/taking_backups.md";

/// The guide that documents every backup command
const THORADM_GUIDE: &str = "../api/docs/src/admins/thoradm/thoradm.md";

/// What a seeded cluster holds
struct Seeded {
    /// The group the sample and the device are in
    group: String,
    /// The admin's icon, named by the admin's `image` in Redis
    icon: String,
    /// The device's image, named by the entity row's `image` column
    image: String,
    /// The entity's id
    entity: Uuid,
    /// The sample's object key in the files bucket
    sample: String,
}

/// Empties the cluster, then seeds an icon, an entity image, and a sample through the API
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
async fn seed_cluster(harness: &Harness) -> Seeded {
    // start from what a fresh install holds
    wipe(harness).await;
    let client = harness.client().await;
    client
        .system
        .init()
        .await
        .expect("failed to initialize the system");
    // a group with a sample, the admin's icon, and a device with an image
    let names = Names::new();
    let group = names.group("hestia");
    seed::group(&client, &group).await;
    seed::sample(&client, &group, "hestia.bin", names.suffix.as_bytes()).await;
    seed::icon(&harness.api_url, &harness.token).await;
    let entity = seed::entity_image(
        &harness.api_url,
        &harness.token,
        &group,
        &names.user("vesta"),
        b"\x89PNG\r\n\x1a\nhestia device image",
    )
    .await;
    // the keys the API stored the graphics under: the entity's id names its image
    let graphics = list_keys(&harness.s3(), &bucket_for(&harness.conf, "graphics"))
        .await
        .unwrap();
    let image = graphics
        .iter()
        .find(|key| key.starts_with(&entity.to_string()))
        .unwrap_or_else(|| panic!("no image of entity {entity} in {graphics:?}"))
        .clone();
    let icon = graphics
        .iter()
        .find(|key| key.starts_with("users/") && key.contains("/icon."))
        .unwrap_or_else(|| panic!("no icon in {graphics:?}"))
        .clone();
    let sample = list_keys(&harness.s3(), &harness.conf.thorium.files.bucket)
        .await
        .unwrap()
        .first()
        .expect("no sample object")
        .clone();
    Seeded {
        group,
        icon,
        image,
        entity,
        sample,
    }
}

/// Removes an object from the graphics bucket
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `key` - The object's key
async fn delete_graphic(harness: &Harness, key: &str) {
    // a delete of a key that is there
    harness
        .s3()
        .delete_object()
        .bucket(bucket_for(&harness.conf, "graphics"))
        .key(key)
        .send()
        .await
        .unwrap_or_else(|err| panic!("failed to delete {key}: {err}"));
}

/// Puts a graphic back into the graphics bucket, so the next test finds it
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `key` - The object's key
async fn put_graphic(harness: &Harness, key: &str) {
    // a PNG-typed object, as the API stores graphics
    harness
        .s3()
        .put_object()
        .bucket(bucket_for(&harness.conf, "graphics"))
        .key(key)
        .content_type("image/png")
        .body(ByteStream::from_static(b"\x89PNG\r\n\x1a\na graphic"))
        .send()
        .await
        .unwrap_or_else(|err| panic!("failed to put {key}: {err}"));
}

/// Runs `thoradm backup new` into a directory
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `components` - The comma-separated component list
/// * `dir` - The backup directory
/// * `extra` - Extra arguments for `backup new`
fn backup_of(harness: &Harness, components: &str, dir: &Path, extra: &[&str]) -> Output {
    // a small worker count and chunk multiplier keep the run quick
    harness
        .thoradm()
        .args([
            "--workers",
            "4",
            "backup",
            "new",
            components,
            "-m",
            "4",
            "-o",
        ])
        .arg(dir)
        .args(extra)
        .output()
        .expect("failed to run thoradm")
}

/// Reads a backup's manifest
///
/// # Arguments
///
/// * `dir` - The backup
fn manifest_of(dir: &Path) -> Manifest {
    // the manifest a finished run writes last
    serde_json::from_slice(&std::fs::read(dir.join(MANIFEST_FILE)).expect("no manifest"))
        .expect("the manifest parses")
}

/// Reads the records of a backup's own failures file
///
/// # Arguments
///
/// * `dir` - The backup
fn records_of(dir: &Path) -> Vec<FailureRecord> {
    // the backup's file, not the reports of later runs beneath it
    read_records(&dir.join(FAILURES_FILE)).unwrap_or_default()
}

/// Returns the record naming a key
///
/// # Arguments
///
/// * `records` - The records
/// * `key` - The missing object's key
fn record_of<'a>(records: &'a [FailureRecord], key: &str) -> &'a FailureRecord {
    // a record names the key of the object it is about
    records
        .iter()
        .find(|record| record.key.as_deref() == Some(key))
        .unwrap_or_else(|| panic!("no record of {key} in {records:?}"))
}

/// Reads the Redis state a backup holds
///
/// # Arguments
///
/// * `dir` - The backup
fn redis_of(dir: &Path) -> Backup {
    // the API's backup as the redis component wrote it
    serde_json::from_slice(&std::fs::read(dir.join("redis.json")).expect("no redis.json"))
        .expect("redis.json parses")
}

/// Returns the user whose icon is a key
///
/// # Arguments
///
/// * `dir` - The backup
/// * `icon` - The icon's key
fn user_of(dir: &Path, icon: &str) -> String {
    // the user record that names the icon
    redis_of(dir)
        .users
        .into_iter()
        .find(|user| user.image.as_deref() == Some(icon))
        .unwrap_or_else(|| panic!("no user names {icon}"))
        .username
}

/// Checks a record is a broken link in the graphics component
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `record` - The record
fn assert_graphics_link(harness: &Harness, record: &FailureRecord) {
    // a broken link of a download in the graphics bucket
    assert_eq!(record.kind, FailureKind::BrokenLink, "{record:?}");
    assert_eq!(record.component, "graphics", "{record:?}");
    assert_eq!(record.operation, Operation::Download, "{record:?}");
    assert_eq!(
        record.bucket.as_deref(),
        Some(bucket_for(&harness.conf, "graphics").as_str()),
        "{record:?}"
    );
}

/// Runs `backup verify` with its report in a directory beside the backup
///
/// Returns the run and the report's checks.
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `dir` - The backup
fn verify_of(harness: &Harness, dir: &Path) -> (Output, Vec<Value>) {
    // a report directory of its own, so the backup stays as it was written
    let report = dir
        .parent()
        .expect("the backup has a parent")
        .join(format!("verify-{}", Uuid::new_v4()));
    let output = harness
        .thoradm()
        .args(["backup", "verify", "-b"])
        .arg(dir)
        .arg("--report-dir")
        .arg(&report)
        .output()
        .expect("failed to run thoradm");
    let checks = std::fs::read(report.join("verify.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|report| report["checks"].as_array().cloned())
        .unwrap_or_default();
    (output, checks)
}

/// Returns the check of a component and name that a verify report holds
///
/// # Arguments
///
/// * `checks` - The report's checks
/// * `component` - The component checked
/// * `name` - The check
fn check_of<'a>(checks: &'a [Value], component: &str, name: &str) -> Option<&'a Value> {
    // one entry per component and check
    checks
        .iter()
        .find(|check| check["component"] == component && check["check"] == name)
}

/// Restores a backup with its report in a directory beside it
///
/// Returns the run and the report directory.
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `dir` - The backup
/// * `extra` - Extra arguments for `backup restore`
fn restore_of(harness: &Harness, dir: &Path, extra: &[&str]) -> (Output, PathBuf) {
    // the report beside the backup, so the backup's own files never change
    let report = dir
        .parent()
        .expect("the backup has a parent")
        .join(format!("restore-{}", Uuid::new_v4()));
    let report_arg = report.to_string_lossy().into_owned();
    let mut args = vec!["--report-dir", report_arg.as_str()];
    args.extend_from_slice(extra);
    (restore_with(harness, dir, &args), report)
}

/// Reads the summary a restore wrote
///
/// # Arguments
///
/// * `report` - The restore's report directory
fn summary_of(report: &Path) -> Value {
    // the summary sits beside the restore's failures file
    let bytes = std::fs::read(report.join("summary.json"))
        .unwrap_or_else(|error| panic!("no summary.json in {}: {error}", report.display()));
    serde_json::from_slice(&bytes).unwrap()
}

/// Returns the keys a summary list names
///
/// # Arguments
///
/// * `list` - A list of records in a summary
fn keys_of(list: &Value) -> Vec<String> {
    // each entry is a failure record
    let mut keys: Vec<String> = list.as_array().map_or_else(Vec::new, |entries| {
        entries
            .iter()
            .filter_map(|entry| entry["key"].as_str().map(ToOwned::to_owned))
            .collect()
    });
    keys.sort();
    keys
}

/// Returns the image column of every entity row
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
async fn entity_images(harness: &Harness) -> Vec<String> {
    // the column the rows name their graphic by
    let session = harness.scylla().await;
    let query = format!(
        "SELECT image FROM \"{}\".entities",
        harness.conf.thorium.namespace
    );
    let rows = session
        .query_unpaged(query, &[])
        .await
        .expect("failed to read the entities")
        .into_rows_result()
        .expect("the entities come back as rows");
    rows.rows::<(Option<String>,)>()
        .expect("the image column decodes")
        .filter_map(|row| row.expect("a row decodes").0)
        .collect()
}

/// With the flag, a user's icon and an entity's image that the bucket lacks are recorded as
/// broken links and kept, the backup carries on and exits 2, the manifest counts them apart from
/// failed objects, and the objects that are there are still backed up
#[tokio::test]
async fn accepted_broken_graphics_are_recorded_kept_and_the_backup_continues() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let seeded = seed_cluster(harness).await;
    // both graphics are gone from the bucket while the rows still name them
    delete_graphic(harness, &seeded.image).await;
    delete_graphic(harness, &seeded.icon).await;
    let dir = harness.scratch("links-accepted").join("backup");
    let taken = backup_of(harness, COMPONENTS, &dir, &["--accept-broken-links"]);
    put_graphic(harness, &seeded.image).await;
    put_graphic(harness, &seeded.icon).await;
    // the run finishes with exit 2 and says how many links it accepted
    assert_eq!(taken.status.code(), Some(2), "{}", describe(&taken));
    let stderr = String::from_utf8_lossy(&taken.stderr);
    assert!(stderr.contains("2 broken links"), "{stderr}");
    // each link is one record naming the missing key and the row that names it
    let records = records_of(&dir);
    assert_eq!(records.len(), 2, "{records:?}");
    for record in &records {
        assert_graphics_link(harness, record);
    }
    assert_eq!(
        record_of(&records, &seeded.image).referrer,
        Some(format!("entities:{}", seeded.entity))
    );
    assert_eq!(
        record_of(&records, &seeded.icon).referrer,
        Some(format!("users:{}", user_of(&dir, &seeded.icon)))
    );
    // the manifest counts them apart from failed and skipped objects
    let manifest = manifest_of(&dir);
    assert_eq!(manifest.broken_links, 2);
    assert_eq!((manifest.failures, manifest.skipped), (0, 0));
    assert!(manifest.failures_sha256.is_some());
    // the user's link is kept as the cluster holds it, not stripped
    assert_eq!(
        redis_of(&dir)
            .users
            .iter()
            .filter_map(|user| user.image.clone())
            .collect::<Vec<_>>(),
        vec![seeded.icon.clone()]
    );
    // the backup carried on: the sample's object is in it and no graphic object is
    assert!(manifest.s3["s3-ids-objects"].objects >= 1);
    assert_eq!(
        manifest.s3.get("graphics").map_or(0, |entry| entry.objects),
        0
    );
    // verify accepts the backup, listing the links as accepted rather than failed
    let (verified, checks) = verify_of(harness, &dir);
    assert_eq!(verified.status.code(), Some(0), "{}", describe(&verified));
    let accepted = check_of(&checks, "graphics", "broken-link")
        .unwrap_or_else(|| panic!("no broken-link check in {checks:?}"));
    assert_eq!(
        (accepted["checked"].as_u64(), accepted["failed"].as_u64()),
        (Some(2), Some(0))
    );
}

/// Without the flag the same missing graphics are failed objects: kind `failed`, nothing counted
/// as a broken link, and a restore refused without `--accept-failures`
#[tokio::test]
async fn without_the_flag_a_missing_graphic_is_still_a_failed_object() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let seeded = seed_cluster(harness).await;
    delete_graphic(harness, &seeded.image).await;
    let dir = harness.scratch("links-without").join("backup");
    let taken = backup_of(harness, COMPONENTS, &dir, &[]);
    put_graphic(harness, &seeded.image).await;
    assert_eq!(taken.status.code(), Some(2), "{}", describe(&taken));
    // the record is a failed object and the manifest counts no broken link
    let records = records_of(&dir);
    let record = record_of(&records, &seeded.image);
    assert_eq!(record.kind, FailureKind::Failed, "{record:?}");
    assert_eq!(record.referrer, None, "{record:?}");
    let manifest = manifest_of(&dir);
    assert_eq!((manifest.failures, manifest.broken_links), (1, 0));
    // the restore is refused, naming the flag that accepts the missing object
    wipe(harness).await;
    let (restored, _) = restore_of(harness, &dir, &[]);
    assert_eq!(restored.status.code(), Some(1), "{}", describe(&restored));
    assert!(
        String::from_utf8_lossy(&restored.stderr).contains("--accept-failures"),
        "{}",
        describe(&restored)
    );
}

/// A restore of a backup whose only records are accepted broken links needs no
/// `--accept-failures`: it exits 0, says which links it knows, and puts back the rows with the
/// links as they were
#[tokio::test]
async fn a_restore_of_only_accepted_broken_links_needs_no_accept_failures() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let seeded = seed_cluster(harness).await;
    delete_graphic(harness, &seeded.image).await;
    delete_graphic(harness, &seeded.icon).await;
    let dir = harness.scratch("links-restore").join("backup");
    let taken = backup_of(harness, COMPONENTS, &dir, &["--accept-broken-links"]);
    assert_eq!(taken.status.code(), Some(2), "{}", describe(&taken));
    // a wipe and a restore with no flag at all
    wipe(harness).await;
    let (restored, report) = restore_of(harness, &dir, &[]);
    assert_eq!(restored.status.code(), Some(0), "{}", describe(&restored));
    // the log and the summary list the links as known, and the backup lacks no object
    let stderr = String::from_utf8_lossy(&restored.stderr);
    assert!(stderr.contains("2 broken links"), "{stderr}");
    for key in [&seeded.image, &seeded.icon] {
        assert!(stderr.contains(key.as_str()), "{key}: {stderr}");
    }
    let summary = summary_of(&report);
    let mut expected = vec![seeded.image.clone(), seeded.icon.clone()];
    expected.sort();
    assert_eq!(keys_of(&summary["broken_links"]), expected, "{summary:#}");
    assert!(
        keys_of(&summary["missing_from_backup"]).is_empty(),
        "{summary:#}"
    );
    // the rows came back naming the same graphics, which the bucket still lacks
    assert_eq!(entity_images(harness).await, vec![seeded.image.clone()]);
    let client = harness.client().await;
    let details = client.users.list_details().await.unwrap();
    assert!(
        details.iter().any(|user| user.has_image),
        "no restored user names an icon"
    );
    let held = list_keys(&harness.s3(), &bucket_for(&harness.conf, "graphics"))
        .await
        .unwrap();
    assert!(held.is_empty(), "the restore invented graphics: {held:?}");
}

/// A `--wipe` restore of a backup with accepted broken links over the cluster it was taken from
/// needs no `--accept-failures`: the plan, printed before the wipe is confirmed, names the broken
/// links and what the wipe does with Scylla snapshots, the wipe runs, the final summary lists the
/// links, and the rows come back naming the same graphics
#[tokio::test]
async fn a_wipe_restore_of_accepted_broken_links_plans_both() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let seeded = seed_cluster(harness).await;
    delete_graphic(harness, &seeded.image).await;
    delete_graphic(harness, &seeded.icon).await;
    let dir = harness.scratch("links-wipe").join("backup");
    let taken = backup_of(harness, COMPONENTS, &dir, &["--accept-broken-links"]);
    assert_eq!(taken.status.code(), Some(2), "{}", describe(&taken));
    // a wipe restore over the populated cluster, with no flag naming the links
    let ns = harness.conf.thorium.namespace.clone();
    let (restored, report) = restore_of(harness, &dir, &["--wipe", ns.as_str()]);
    assert_eq!(restored.status.code(), Some(0), "{}", describe(&restored));
    let stderr = String::from_utf8_lossy(&restored.stderr).into_owned();
    // the plan is everything logged before the wipe was confirmed
    let confirmed = stderr
        .find("confirmed by --wipe")
        .unwrap_or_else(|| panic!("no confirmation:\n{stderr}"));
    let (plan, after) = stderr.split_at(confirmed);
    // the plan names the links and what the wipe leaves of the truncated tables on disk
    assert!(plan.contains("2 broken links"), "{plan}");
    assert!(
        plan.contains("auto_snapshot is off") || plan.contains("hold the wiped data and its disk"),
        "{plan}"
    );
    // the final summary lists each link as known
    assert!(after.contains("2 broken links"), "{after}");
    for key in [&seeded.image, &seeded.icon] {
        assert!(after.contains(key.as_str()), "{key}: {after}");
    }
    // the summary holds both the wipe and the links
    let summary = summary_of(&report);
    assert!(summary["wipe"]["tables"].is_array(), "{summary:#}");
    let mut expected = vec![seeded.image.clone(), seeded.icon.clone()];
    expected.sort();
    assert_eq!(keys_of(&summary["broken_links"]), expected, "{summary:#}");
    // the rows came back naming the same graphics
    assert_eq!(entity_images(harness).await, vec![seeded.image.clone()]);
}

/// A real failed object beside accepted broken links still needs `--accept-failures`: the
/// refusal and the summary list the failed object alone, the links are known, and verify fails
/// the failed object and accepts the links
#[tokio::test]
async fn a_failed_object_beside_broken_links_still_needs_accept_failures() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let seeded = seed_cluster(harness).await;
    delete_graphic(harness, &seeded.image).await;
    // S3 answers 503 to every download of the sample's object, which one attempt can't get past
    let mock = MockS3::start(&harness.conf.thorium.s3.endpoint)
        .await
        .expect("failed to start MockS3");
    mock.set_rules(vec![S3Rule {
        method: Method::GET,
        key: seeded.sample.clone(),
        query: None,
        nth: None,
        behavior: S3Behavior::Status {
            status: 503,
            code: "SlowDown".to_owned(),
        },
    }])
    .await;
    let mut conf = harness.conf.clone();
    conf.thorium.s3.endpoint.clone_from(&mock.endpoint);
    let scratch = harness.scratch("links-mixed");
    let cluster_conf = scratch.join("thorium.yml");
    write_cluster_conf(&conf, &cluster_conf).unwrap();
    let dir = scratch.join("backup");
    let mut command = harness.thoradm_with(&cluster_conf);
    command
        .args([
            "backup",
            "new",
            COMPONENTS,
            "--retries",
            "1",
            "--accept-broken-links",
            "-o",
        ])
        .arg(&dir);
    let taken = tokio::process::Command::from(command)
        .output()
        .await
        .expect("failed to run thoradm");
    put_graphic(harness, &seeded.image).await;
    assert_eq!(taken.status.code(), Some(2), "{}", describe(&taken));
    // one failed object and one broken link, counted apart
    let records = records_of(&dir);
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(
        record_of(&records, &seeded.sample).kind,
        FailureKind::Failed
    );
    assert_eq!(
        record_of(&records, &seeded.image).kind,
        FailureKind::BrokenLink
    );
    let manifest = manifest_of(&dir);
    assert_eq!((manifest.failures, manifest.broken_links), (1, 1));
    // verify fails the object the backup lacks and accepts the link
    let (verified, checks) = verify_of(harness, &dir);
    assert_eq!(verified.status.code(), Some(2), "{}", describe(&verified));
    let failed = check_of(&checks, "s3-ids-objects", "recorded-failures");
    assert_eq!(
        failed.map(|check| check["failed"].as_u64()),
        Some(Some(1)),
        "{checks:?}"
    );
    let accepted = check_of(&checks, "graphics", "broken-link");
    assert_eq!(
        accepted.map(|check| check["failed"].as_u64()),
        Some(Some(0)),
        "{checks:?}"
    );
    // the restore is refused, listing the failed object and not the link
    wipe(harness).await;
    let (refused, report) = restore_of(harness, &dir, &[]);
    assert_eq!(refused.status.code(), Some(1), "{}", describe(&refused));
    let listed = read_records(&report.join(FAILURES_FILE)).unwrap_or_default();
    assert!(
        listed
            .iter()
            .any(|record| record.key.as_deref() == Some(seeded.sample.as_str())),
        "{listed:?}"
    );
    assert!(
        listed
            .iter()
            .all(|record| record.key.as_deref() != Some(seeded.image.as_str())),
        "{listed:?}"
    );
    // with the flag it restores, listing the failed object as missing and the link as known
    let (restored, report) = restore_of(harness, &dir, &["--accept-failures"]);
    assert_eq!(restored.status.code(), Some(0), "{}", describe(&restored));
    let summary = summary_of(&report);
    assert_eq!(
        keys_of(&summary["missing_from_backup"]),
        vec![seeded.sample.clone()],
        "{summary:#}"
    );
    assert_eq!(
        keys_of(&summary["broken_links"]),
        vec![seeded.image.clone()],
        "{summary:#}"
    );
}

/// A run that resumes an interrupted backup keeps the flag's rule: it accepts broken links when
/// the interrupted run did, or when the resuming command says so, and not otherwise
#[tokio::test]
async fn resume_keeps_the_broken_link_rule() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let seeded = seed_cluster(harness).await;
    delete_graphic(harness, &seeded.image).await;
    // the interrupted run's marker, accepting or not, and the flags the resuming command gives
    let cases: [(&str, bool, &[&str], FailureKind); 3] = [
        ("marker", true, &[], FailureKind::BrokenLink),
        (
            "command",
            false,
            &["--accept-broken-links"],
            FailureKind::BrokenLink,
        ),
        ("neither", false, &[], FailureKind::Failed),
    ];
    for (name, marker_accepts, flags, expected) in cases {
        let dir = harness
            .scratch(&format!("links-resume-{name}"))
            .join("backup");
        std::fs::create_dir_all(&dir).unwrap();
        let components = vec![
            BackupComponents::Entities,
            BackupComponents::Redis,
            BackupComponents::Graphics,
        ];
        let mut marker = InProgressMarker::new(chrono::Utc::now(), components);
        marker.accept_broken_links = marker_accepts;
        marker.write(&dir, &TokioFs).await.unwrap();
        // the resuming command names only the entities, and the run keeps the marker's selection
        let resumed = backup_of(harness, "entities", &dir, flags);
        assert_eq!(
            resumed.status.code(),
            Some(2),
            "{name}: {}",
            describe(&resumed)
        );
        let records = records_of(&dir);
        assert_eq!(record_of(&records, &seeded.image).kind, expected, "{name}");
        let manifest = manifest_of(&dir);
        let links = u64::from(expected == FailureKind::BrokenLink);
        assert_eq!(
            (manifest.broken_links, manifest.failures),
            (links, 1 - links),
            "{name}"
        );
    }
    put_graphic(harness, &seeded.image).await;
}

/// Under `--abort-on-failure` an accepted broken link doesn't stop the backup: it finishes with
/// exit 2 and a manifest, not exit 1
#[tokio::test]
async fn abort_on_failure_does_not_stop_at_an_accepted_broken_link() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let seeded = seed_cluster(harness).await;
    delete_graphic(harness, &seeded.image).await;
    let dir = harness.scratch("links-abort").join("backup");
    let flags = ["--abort-on-failure", "--accept-broken-links"];
    let taken = backup_of(harness, COMPONENTS, &dir, &flags);
    put_graphic(harness, &seeded.image).await;
    assert_eq!(taken.status.code(), Some(2), "{}", describe(&taken));
    // the run reached its end: the manifest counts the link and the sample landed
    let manifest = manifest_of(&dir);
    assert_eq!((manifest.broken_links, manifest.failures), (1, 0));
    assert!(manifest.s3["s3-ids-objects"].objects >= 1);
    // without the flag the same missing graphic stops the run
    let stopped = harness.scratch("links-abort-without").join("backup");
    delete_graphic(harness, &seeded.image).await;
    let taken = backup_of(harness, COMPONENTS, &stopped, &["--abort-on-failure"]);
    put_graphic(harness, &seeded.image).await;
    assert_eq!(taken.status.code(), Some(1), "{}", describe(&taken));
    assert!(!stopped.join(MANIFEST_FILE).exists());
}

/// The flag changes nothing when no link is broken, and works beside every selection `backup new`
/// accepts: a selection without S3 components, and `graphics` with its prerequisites; `graphics`
/// without them is refused as a usage error whatever the flag says
#[tokio::test]
async fn the_flag_combines_with_every_component_selection() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let seeded = seed_cluster(harness).await;
    // nothing is broken, so the flag records nothing and the run is complete
    let whole = harness.scratch("links-whole").join("backup");
    let taken = backup_of(harness, COMPONENTS, &whole, &["--accept-broken-links"]);
    assert_eq!(taken.status.code(), Some(0), "{}", describe(&taken));
    assert_eq!(manifest_of(&whole).broken_links, 0);
    assert_eq!(records_of(&whole).len(), 0);
    // a selection with no S3 component takes the flag too
    let redis = harness.scratch("links-redis").join("backup");
    let taken = backup_of(harness, "redis", &redis, &["--accept-broken-links"]);
    assert_eq!(taken.status.code(), Some(0), "{}", describe(&taken));
    // the graphics with entities and redis in another order, one graphic missing
    delete_graphic(harness, &seeded.icon).await;
    let order = harness.scratch("links-order").join("backup");
    let taken = backup_of(
        harness,
        "redis,graphics,entities",
        &order,
        &["--accept-broken-links"],
    );
    put_graphic(harness, &seeded.icon).await;
    assert_eq!(taken.status.code(), Some(2), "{}", describe(&taken));
    let records = records_of(&order);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(
        record_of(&records, &seeded.icon).kind,
        FailureKind::BrokenLink
    );
    // graphics without what it needs is refused before anything is written
    let refused = harness.scratch("links-refused").join("backup");
    let taken = backup_of(harness, "graphics", &refused, &["--accept-broken-links"]);
    let stderr = String::from_utf8_lossy(&taken.stderr);
    assert_eq!(taken.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("graphics needs entities and redis"),
        "{stderr}"
    );
    assert_eq!(std::fs::read_dir(&refused).map_or(0, Iterator::count), 0);
}

/// When the `redis` component fails, the users' icons can't be checked: the run says so with a
/// `skipped` record and a warning instead of silently leaving them out of the broken-link list,
/// and a healthy `redis` component records nothing of the kind
#[tokio::test]
async fn a_failed_redis_component_records_that_user_icons_were_not_checked() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let seeded = seed_cluster(harness).await;
    // the admin's icon is gone from the bucket while the user row still names it
    delete_graphic(harness, &seeded.icon).await;
    let components = "entities,graphics,redis";
    // with a healthy redis component the icon is a recorded broken link and nothing is skipped
    let healthy = harness.scratch("links-icons-healthy").join("backup");
    let taken = backup_of(harness, components, &healthy, &["--accept-broken-links"]);
    assert_eq!(taken.status.code(), Some(2), "{}", describe(&taken));
    let records = records_of(&healthy);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(
        record_of(&records, &seeded.icon).kind,
        FailureKind::BrokenLink
    );
    assert_eq!(manifest_of(&healthy).skipped, 0);
    // a group owner that isn't a user makes the API refuse the Redis backup
    let ns = harness.conf.thorium.namespace.clone();
    let ghost = Names::new().user("carme");
    let pool = redis_pool(&harness.conf).await.unwrap();
    let mut conn = pool.get().await.unwrap();
    let sets = [
        format!("{ns}:groups_direct:{}:owners", seeded.group),
        format!("{ns}:groups_combined:{}:owners", seeded.group),
    ];
    for set in &sets {
        let _: redis::Value = redis::cmd("SADD")
            .arg(set)
            .arg(&ghost)
            .query_async(&mut *conn)
            .await
            .unwrap();
    }
    let broken = harness.scratch("links-icons-failed").join("backup");
    let taken = backup_of(harness, components, &broken, &["--accept-broken-links"]);
    // remove the dangling owner and put the icon back before any assertion can end the test
    for set in &sets {
        let _: redis::Value = redis::cmd("SREM")
            .arg(set)
            .arg(&ghost)
            .query_async(&mut *conn)
            .await
            .unwrap();
    }
    put_graphic(harness, &seeded.icon).await;
    // the run exits 2 and warns that the icons weren't checked
    assert_eq!(taken.status.code(), Some(2), "{}", describe(&taken));
    let stderr = String::from_utf8_lossy(&taken.stderr);
    assert!(
        stderr
            .lines()
            .any(|line| line.contains("WARN") && line.contains("user icons")),
        "{stderr}"
    );
    // the failed redis record is beside a skipped record of the graphics component
    let records = records_of(&broken);
    assert_eq!(records.len(), 2, "{records:?}");
    let redis = records
        .iter()
        .find(|record| record.component == "redis")
        .unwrap_or_else(|| panic!("no redis record in {records:?}"));
    assert_eq!(redis.kind, FailureKind::Failed, "{redis:?}");
    let skipped = records
        .iter()
        .find(|record| record.component == "graphics")
        .unwrap_or_else(|| panic!("no graphics record in {records:?}"));
    assert_eq!(skipped.kind, FailureKind::Skipped, "{skipped:?}");
    assert!(
        skipped.error.contains("user icons") && skipped.error.contains("redis"),
        "{skipped:?}"
    );
    // the manifest counts both, and no state was written
    let manifest = manifest_of(&broken);
    assert_eq!((manifest.failures, manifest.skipped), (1, 1));
    assert_eq!(manifest.broken_links, 0);
    assert!(!broken.join("redis.json").exists());
}

/// Reads a guide
///
/// # Arguments
///
/// * `path` - The guide's path from the crate's directory
fn read_guide(path: &str) -> String {
    // the guides are part of the repository
    std::fs::read_to_string(path).unwrap_or_else(|error| panic!("failed to read {path}: {error}"))
}

/// The guides document the flag, the record kind and its fields, the manifest count, and the
/// marker's setting
#[test]
fn the_guides_document_accepting_broken_links() {
    // what each guide must name
    let taking = read_guide(TAKING_BACKUPS);
    let thoradm = read_guide(THORADM_GUIDE);
    for text in ["`--accept-broken-links`", "broken link"] {
        assert!(taking.contains(text), "taking_backups.md lacks {text}");
        assert!(thoradm.contains(text), "thoradm.md lacks {text}");
    }
    for text in [
        "`broken-link`",
        "`referrer`",
        "`broken_links`",
        "`accept_broken_links`",
    ] {
        assert!(taking.contains(text), "taking_backups.md lacks {text}");
    }
}

/// `thoradm backup coverage` says that a reference to a missing S3 object is a failed object,
/// or with `--accept-broken-links` a recorded and kept broken link
#[test]
fn the_coverage_page_names_broken_links() {
    // the page needs no cluster or config
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_thoradm"))
        .args(["backup", "coverage"])
        .output()
        .expect("failed to run thoradm");
    assert_eq!(output.status.code(), Some(0));
    let page = String::from_utf8_lossy(&output.stdout);
    assert!(page.contains("--accept-broken-links"), "{page}");
    assert!(page.contains("broken link"), "{page}");
}
