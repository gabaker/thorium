//! S3 restore: it addresses buckets the way the config says, and an upload that fails is aborted
//! by bucket and key, recorded, counted as failed, and restored by a re-run once the fault clears
//!
//! Every test restores, so each holds the binary's restore guard.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use axum::http::Method;
use clap::Parser;
use common::cycle::{describe, wipe};
use common::harness::{Harness, list_keys, write_cluster_conf};
use common::mock_s3::{MockS3, S3Behavior, S3Rule, SeenRequest};
use common::names::Names;
use common::seed;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;
use thoradm::args::{NewBackup, RestoreBackup};
use thoradm::backup::controllers::{backup, restore};
use thoradm::backup::failures::{FAILURES_FILE, Operation, read_records};
use thoradm::backup::sink::TokioFs;
use thoradm::outcome::RunOutcome;
use thorium::CtlConf;

/// The components these tests back up: samples with their objects, and Redis
const COMPONENTS: &str = "samples-list,s3-ids,s3-ids-objects,redis";

/// Empties the target and initializes the system again, as a fresh install has it
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
async fn empty_target(harness: &Harness) {
    // wipe everything but the admin's login, then put back what a fresh install has
    wipe(harness).await;
    harness
        .client()
        .await
        .system
        .init()
        .await
        .expect("failed to initialize the system");
}

/// Empties the target, seeds samples, and backs them up with their objects, returning the
/// backup and the keys of the objects it holds
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `name` - The scratch directory's name
/// * `count` - How many samples to seed
async fn seeded_backup(harness: &Harness, name: &str, count: usize) -> (PathBuf, Vec<String>) {
    // samples in a group of their own
    empty_target(harness).await;
    let client = harness.client().await;
    let names = Names::new();
    let group = names.group("neptune");
    seed::group(&client, &group).await;
    for index in 0..count {
        let data = format!("{} sample {index}", names.suffix);
        seed::sample(
            &client,
            &group,
            &format!("neptune-{index}.bin"),
            data.as_bytes(),
        )
        .await;
    }
    let mut keys = list_keys(&harness.s3(), &harness.conf.thorium.files.bucket)
        .await
        .unwrap();
    keys.sort();
    // a backup of their rows and objects
    let out = harness.scratch(name).join("backup");
    let taken = harness
        .thoradm()
        .args(["backup", "new", COMPONENTS, "-o"])
        .arg(&out)
        .output()
        .expect("failed to run thoradm");
    assert_eq!(taken.status.code(), Some(0), "{}", describe(&taken));
    (out, keys)
}

/// Returns the host and port of an endpoint URL
///
/// # Arguments
///
/// * `endpoint` - The endpoint (`http://localhost:39123`)
fn authority(endpoint: &str) -> String {
    // drop the scheme and any trailing slash
    endpoint
        .split_once("://")
        .map_or(endpoint, |(_, rest)| rest)
        .trim_end_matches('/')
        .to_owned()
}

/// Checks that every request a run made to a bucket named the bucket in its `Host` header
///
/// # Arguments
///
/// * `seen` - The requests `MockS3` saw
/// * `bucket` - The bucket
/// * `what` - The run, for the assertion message
fn assert_virtual_hosted(seen: &[SeenRequest], bucket: &str, what: &str) {
    // some request reached the bucket, and each one addressed it by host
    let mine: Vec<&SeenRequest> = seen.iter().filter(|seen| seen.bucket == bucket).collect();
    assert!(
        !mine.is_empty(),
        "{what} made no request to {bucket}: {seen:#?}"
    );
    for request in mine {
        assert!(
            request.host.starts_with(&format!("{bucket}.")),
            "{what} addressed {bucket} in the path: {request:?}"
        );
    }
}

/// With `use_path_style` off, the restore addresses buckets by host name, as the backup does
#[tokio::test]
async fn restore_addresses_buckets_the_way_the_config_says() {
    // a backup of samples with their objects, and an empty target
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let (backup_dir, _) = seeded_backup(harness, "addressing", 2).await;
    empty_target(harness).await;
    // a config addressing buckets by host name, through a proxy that records each request
    let mock = MockS3::start(&harness.conf.thorium.s3.endpoint)
        .await
        .expect("failed to start MockS3");
    thoradm::backup::route_virtual_hosts_through(&authority(&mock.endpoint));
    let mut conf = harness.conf.clone();
    conf.thorium.s3.endpoint.clone_from(&mock.endpoint);
    conf.thorium.s3.use_path_style = false;
    let bucket = conf.thorium.files.bucket.clone();
    let ctl_conf = CtlConf::from_path(&harness.ctl_conf).unwrap();
    let scylla = harness.scylla().await;
    // the restore addresses the files bucket by host name
    let args = RestoreBackup::try_parse_from([
        "restore".as_ref(),
        "--yes".as_ref(),
        "-b".as_ref(),
        backup_dir.as_os_str(),
    ])
    .unwrap();
    let outcome = Box::pin(restore::run(
        &conf,
        ctl_conf.clone(),
        &scylla,
        2,
        &args,
        Arc::new(TokioFs),
    ))
    .await
    .unwrap();
    assert_eq!(outcome, RunOutcome::Complete);
    assert_virtual_hosted(&mock.seen().await, &bucket, "the restore");
    // and so does a backup
    mock.clear_seen().await;
    let out = harness.scratch("addressing-backup").join("backup");
    let args = NewBackup::try_parse_from([
        "new".as_ref(),
        "s3-ids,s3-ids-objects".as_ref(),
        "-o".as_ref(),
        out.as_os_str(),
    ])
    .unwrap();
    let outcome = Box::pin(backup::run(
        &conf,
        ctl_conf,
        &scylla,
        2,
        &args,
        Arc::new(TokioFs),
    ))
    .await
    .unwrap();
    assert_eq!(outcome, RunOutcome::Complete);
    assert_virtual_hosted(&mock.seen().await, &bucket, "the backup");
}

/// Runs a restore through a cluster config, without a terminal, awaiting it so the test's proxy
/// keeps serving
///
/// # Arguments
///
/// * `harness` - The harness naming the Thorctl config
/// * `cluster_conf` - The cluster config, pointing S3 at the proxy
/// * `backup` - The backup to restore
/// * `report_dir` - The report directory
/// * `extra` - Extra `backup restore` arguments
async fn restore_through(
    harness: &Harness,
    cluster_conf: &Path,
    backup: &Path,
    report_dir: &Path,
    extra: &[&str],
) -> Output {
    // a child process, awaited on the test's runtime
    let mut command = harness.thoradm_with(cluster_conf);
    command
        .args(["--workers", "2", "backup", "restore", "--yes", "-b"])
        .arg(backup)
        .arg("--report-dir")
        .arg(report_dir)
        .args(extra);
    tokio::process::Command::from(command)
        .output()
        .await
        .expect("failed to run thoradm")
}

/// Returns the multipart uploads a bucket still holds open
///
/// # Arguments
///
/// * `harness` - The harness naming the S3 service
/// * `bucket` - The bucket
async fn open_uploads(harness: &Harness, bucket: &str) -> Vec<String> {
    // list them straight from S3
    harness
        .s3()
        .list_multipart_uploads()
        .bucket(bucket)
        .send()
        .await
        .unwrap()
        .uploads()
        .iter()
        .filter_map(|upload| upload.key().map(ToOwned::to_owned))
        .collect()
}

/// An upload whose parts fail with 500 is aborted by bucket and key, recorded, and counted as
/// failed while every other object is uploaded and the run exits 2; a `--merge` re-run after the
/// fault clears exits 0. Under `--abort-on-failure` the same fault exits 1 with no upload started
/// after it, and the `--merge` re-run exits 0
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn a_failed_upload_is_aborted_recorded_and_counted() {
    // a backup of three samples' objects, one of which S3 will refuse to take
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let (backup_dir, keys) = seeded_backup(harness, "failed-upload", 3).await;
    assert_eq!(keys.len(), 3, "{keys:?}");
    // the object S3 refuses, and the bucket it belongs in
    let refused = keys[0].clone();
    let bucket = harness.conf.thorium.files.bucket.clone();
    // a mock in front of S3 that fails every part upload of that object
    let mock = MockS3::start(&harness.conf.thorium.s3.endpoint)
        .await
        .expect("failed to start MockS3");
    let fault = S3Rule {
        method: Method::PUT,
        key: refused.clone(),
        query: Some("partNumber".to_owned()),
        nth: None,
        behavior: S3Behavior::Status {
            status: 500,
            code: "InternalError".to_owned(),
        },
    };
    // a cluster config that sends thoradm's S3 requests through the mock
    let mut conf = harness.conf.clone();
    conf.thorium.s3.endpoint.clone_from(&mock.endpoint);
    let scratch = backup_dir.parent().unwrap().to_path_buf();
    let cluster_conf = scratch.join("thorium.yml");
    write_cluster_conf(&conf, &cluster_conf).unwrap();
    for abort in [false, true] {
        // restore into an empty target while the object's parts fail
        empty_target(harness).await;
        mock.set_rules(vec![fault.clone()]).await;
        mock.clear_seen().await;
        let report = scratch.join(format!("report-{abort}"));
        let extra: &[&str] = if abort { &["--abort-on-failure"] } else { &[] };
        let run = restore_through(harness, &cluster_conf, &backup_dir, &report, extra).await;
        // the run fails the object: exit 2, or 1 when --abort-on-failure stops the run
        let records = read_records(&report.join(FAILURES_FILE)).unwrap_or_default();
        let shown = format!("{}\n{records:#?}", describe(&run));
        assert_eq!(
            run.status.code(),
            Some(if abort { 1 } else { 2 }),
            "{shown}"
        );
        // the object is recorded by bucket and key
        assert!(
            records
                .iter()
                .any(|record| record.operation == Operation::Upload
                    && record.key.as_deref() == Some(refused.as_str())
                    && record.bucket.as_deref() == Some(bucket.as_str())),
            "{shown}"
        );
        // its multipart upload was aborted by bucket and key, leaving none open
        let seen = mock.seen().await;
        let abort_at = seen
            .iter()
            .rposition(|request| {
                request.method == Method::DELETE
                    && request.bucket == bucket
                    && request.key == refused
                    && request.path.contains("uploadId=")
            })
            .unwrap_or_else(|| panic!("no abort named {bucket}/{refused}: {seen:#?}\n{shown}"));
        // no abort failed, no upload is left open, and the object never landed
        assert!(
            !records
                .iter()
                .any(|record| record.operation == Operation::AbortUpload),
            "{shown}"
        );
        assert_eq!(open_uploads(harness, &bucket).await, Vec::<String>::new());
        let landed = list_keys(&harness.s3(), &bucket).await.unwrap();
        assert!(!landed.contains(&refused), "{refused} landed: {shown}");
        if abort {
            // no upload started after the failure was recorded, which follows the last abort of
            // the object's attempts
            let late: Vec<&SeenRequest> = seen[abort_at..]
                .iter()
                .filter(|request| {
                    request.method == Method::POST && request.path.contains("uploads")
                })
                .collect();
            assert!(
                late.is_empty(),
                "uploads started after the record: {late:#?}"
            );
        } else {
            // every other object was uploaded, and only they count as restored
            for key in &keys[1..] {
                assert!(landed.contains(key), "{key} is missing: {shown}");
            }
            let summary: serde_json::Value =
                serde_json::from_slice(&std::fs::read(report.join("summary.json")).unwrap())
                    .unwrap();
            let counts = &summary["components"]["s3-ids-objects"];
            assert_eq!(counts["handled"], 2, "{summary:#}");
            assert_eq!(counts["failed"], 1, "{summary:#}");
        }
        // once the fault clears, a --merge re-run restores everything
        mock.set_rules(Vec::new()).await;
        let report = scratch.join(format!("rerun-{abort}"));
        let rerun =
            restore_through(harness, &cluster_conf, &backup_dir, &report, &["--merge"]).await;
        assert_eq!(rerun.status.code(), Some(0), "{}", describe(&rerun));
        let landed = list_keys(&harness.s3(), &bucket).await.unwrap();
        for key in &keys {
            assert!(landed.contains(key), "{key} is missing after the re-run");
        }
        // and the cluster matches the backup
        let verified = harness
            .thoradm()
            .args(["backup", "verify", "--cluster", "-b"])
            .arg(&backup_dir)
            .arg("--report-dir")
            .arg(scratch.join(format!("verify-{abort}")))
            .output()
            .expect("failed to run thoradm");
        assert_eq!(verified.status.code(), Some(0), "{}", describe(&verified));
    }
}

/// Returns whether a request starts a multipart upload (`POST ?uploads`)
///
/// # Arguments
///
/// * `request` - A request `MockS3` saw
fn starts_upload(request: &SeenRequest) -> bool {
    // the request's query names the `uploads` operation
    request.method == Method::POST
        && request.path.split_once('?').is_some_and(|(_, query)| {
            query
                .split('&')
                .any(|pair| pair.split('=').next() == Some("uploads"))
        })
}

/// A restore uploads each graphic with the content type the API stores graphics of its
/// extension with, a graphic without a known extension and every other object as
/// `application/octet-stream`
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn restored_graphics_keep_their_content_type() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    // a sample, a profile icon, and graphics of other types and of none
    empty_target(harness).await;
    let client = harness.client().await;
    let names = Names::new();
    let group = names.group("triton");
    seed::group(&client, &group).await;
    seed::sample(&client, &group, "triton.bin", b"triton sample").await;
    seed::icon(&harness.api_url, &harness.token).await;
    let graphics_bucket = harness.conf.thorium.graphics.bucket.clone();
    let mut wanted = vec![
        (format!("{}/drawing.svg", names.suffix), "image/svg+xml"),
        (format!("{}/clip.webm", names.suffix), "video/webm"),
        (
            format!("{}/unnamed", names.suffix),
            "application/octet-stream",
        ),
    ];
    for (key, content_type) in &wanted {
        harness
            .s3()
            .put_object()
            .bucket(&graphics_bucket)
            .key(key)
            .content_type(*content_type)
            .body(ByteStream::from_static(b"triton graphic"))
            .send()
            .await
            .unwrap();
    }
    // the icon the API stored as a PNG
    let graphics = list_keys(&harness.s3(), &graphics_bucket).await.unwrap();
    let icon = graphics
        .iter()
        .find(|key| key.ends_with("/icon.png"))
        .unwrap_or_else(|| panic!("no icon in the graphics bucket: {graphics:?}"))
        .clone();
    wanted.push((icon, "image/png"));
    let samples = list_keys(&harness.s3(), &harness.conf.thorium.files.bucket)
        .await
        .unwrap();
    assert!(
        !samples.is_empty(),
        "the sample's object is in the files bucket"
    );
    // a backup of the sample, its object, and the graphics
    let backup_dir = harness.scratch("content-type").join("backup");
    let _ = std::fs::remove_dir_all(&backup_dir);
    let taken = harness
        .thoradm()
        .args([
            "backup",
            "new",
            "samples-list,s3-ids,s3-ids-objects,graphics,redis",
            "-o",
        ])
        .arg(&backup_dir)
        .output()
        .expect("failed to run thoradm");
    assert_eq!(taken.status.code(), Some(0), "{}", describe(&taken));
    // a restore into an empty target through a mock that sees every upload
    let mock = MockS3::start(&harness.conf.thorium.s3.endpoint)
        .await
        .expect("failed to start MockS3");
    let mut conf = harness.conf.clone();
    conf.thorium.s3.endpoint.clone_from(&mock.endpoint);
    let scratch = backup_dir.parent().unwrap().to_path_buf();
    let cluster_conf = scratch.join("thorium.yml");
    write_cluster_conf(&conf, &cluster_conf).unwrap();
    empty_target(harness).await;
    mock.clear_seen().await;
    let report = scratch.join("report");
    let run = restore_through(harness, &cluster_conf, &backup_dir, &report, &[]).await;
    assert_eq!(run.status.code(), Some(0), "{}", describe(&run));
    // the content type each upload started with, by bucket and key
    let started: std::collections::BTreeMap<(String, String), Option<String>> = mock
        .seen()
        .await
        .into_iter()
        .filter(starts_upload)
        .map(|request| ((request.bucket, request.key), request.content_type))
        .collect();
    // each graphic with the type its extension names, and each sample object as plain bytes
    let mut problems = Vec::new();
    let expected = wanted
        .iter()
        .map(|(key, content_type)| (graphics_bucket.clone(), key.clone(), *content_type))
        .chain(samples.iter().map(|key| {
            (
                harness.conf.thorium.files.bucket.clone(),
                key.clone(),
                "application/octet-stream",
            )
        }));
    for (bucket, key, content_type) in expected {
        match started.get(&(bucket.clone(), key.clone())) {
            Some(Some(found)) if found == content_type => (),
            found => problems.push(format!(
                "{bucket}/{key} was uploaded as {found:?}, not {content_type}"
            )),
        }
    }
    assert!(problems.is_empty(), "{}\n{started:#?}", problems.join("\n"));
}
