//! What a restore leaves for the cluster to work: nodes that must check in again, census streams
//! rebuilt so listings work at once, and a summary of what was and wasn't done
//!
//! Every test restores, so each holds the binary's restore guard.

mod common;

use common::cycle::{describe, wipe};
use common::fixture;
use common::harness::{Harness, redis_pool};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use thorium::models::{
    EntityListOpts, FileListOpts, ImageScaler, NodeHealth, NodeListParams, NodeUpdate,
    RepoListOpts, Resources,
};

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

/// Backs the namespace up into a fresh scratch directory and returns it
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `name` - The scratch directory's name
/// * `components` - The components to back up
fn backup(harness: &Harness, name: &str, components: &str) -> PathBuf {
    // a backup of the components asked for
    let out = harness.scratch(name).join("backup");
    let taken = harness
        .thoradm()
        .args(["backup", "new", components, "-o"])
        .arg(&out)
        .output()
        .expect("failed to run thoradm");
    assert_eq!(taken.status.code(), Some(0), "{}", describe(&taken));
    out
}

/// Restores a backup without a terminal, with its report beside the backup
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `backup` - The backup to restore
fn restore(harness: &Harness, backup: &Path) -> (Output, PathBuf) {
    // confirm with --yes
    let report = backup.parent().unwrap().join("report");
    let output = harness
        .thoradm()
        .args(["--workers", "2", "backup", "restore", "--yes", "-b"])
        .arg(backup)
        .arg("--report-dir")
        .arg(&report)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run thoradm");
    (output, report)
}

/// Returns the namespace's Redis keys that start with a prefix, without the namespace
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `prefix` - The prefix after the namespace (`workers:`)
async fn keys(harness: &Harness, prefix: &str) -> Vec<String> {
    // list them all; the test namespace is small
    let ns = &harness.conf.thorium.namespace;
    let pool = redis_pool(&harness.conf).await.unwrap();
    let mut conn = pool.get().await.unwrap();
    let mut found: Vec<String> = redis::cmd("KEYS")
        .arg(format!("{ns}:{prefix}*"))
        .query_async(&mut *conn)
        .await
        .unwrap();
    found.sort();
    found
}

/// Returns every census stream of some kinds with its members
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `kinds` - The census kinds (`entities`, `associations`)
async fn census(harness: &Harness, kinds: &[&str]) -> BTreeMap<String, Vec<String>> {
    // each stream is a sorted set of buckets
    let pool = redis_pool(&harness.conf).await.unwrap();
    let mut conn = pool.get().await.unwrap();
    let mut streams = BTreeMap::new();
    for kind in kinds {
        for key in keys(harness, &format!("census:{kind}:")).await {
            let members: Vec<String> = redis::cmd("ZRANGE")
                .arg(&key)
                .arg(0)
                .arg(-1)
                .query_async(&mut *conn)
                .await
                .unwrap();
            streams.insert(key, members);
        }
    }
    streams
}

/// Lists the nodes of a cluster through `GET /api/system/nodes/details/`
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `cluster` - The cluster
async fn cluster_nodes(harness: &Harness, cluster: &str) -> Vec<thorium::models::Node> {
    // every scaler, so no node is left out
    let params = NodeListParams {
        clusters: vec![cluster.to_owned()],
        scalers: vec![
            ImageScaler::K8s,
            ImageScaler::BareMetal,
            ImageScaler::Windows,
            ImageScaler::Kvm,
            ImageScaler::External,
        ],
        ..NodeListParams::default()
    };
    harness
        .client()
        .await
        .system
        .list_node_details(&params)
        .await
        .unwrap()
        .data
}

/// A node backed up healthy, with a heartbeat and a worker, comes back registered, with no
/// heartbeat and no workers, so the API lists it as never checked in
#[tokio::test]
async fn restored_nodes_come_back_registered() {
    // a node with a worker, then marked healthy by a heartbeat
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    empty_target(harness).await;
    fixture::broad(harness).await;
    // the fixture's node `forge`, by its cluster
    let client = harness.client().await;
    let scylla = harness.scylla().await;
    let (cluster,) = scylla
        .query_unpaged(
            format!(
                "SELECT cluster FROM {}.nodes WHERE node = 'forge' ALLOW FILTERING",
                harness.conf.thorium.namespace
            ),
            &[],
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap()
        .first_row::<(String,)>()
        .expect("the fixture registered no node");
    // a heartbeat marks it healthy, and the fixture gave it workers
    let update = NodeUpdate::new(NodeHealth::Healthy, Resources::default()).heart_beat();
    client
        .system
        .update_node(&cluster, "forge", &update)
        .await
        .unwrap();
    assert!(
        !keys(harness, "workers:").await.is_empty(),
        "the fixture has no workers"
    );
    let nodes = cluster_nodes(harness, &cluster).await;
    assert!(
        nodes
            .iter()
            .any(|node| matches!(node.health, NodeHealth::Healthy) && node.heart_beat.is_some()),
        "{nodes:?}"
    );
    // back up its row and the Redis state, then restore them into an emptied target
    let out = backup(harness, "nodes", "nodes,redis");
    empty_target(harness).await;
    let (restored, _) = restore(harness, &out);
    assert_eq!(restored.status.code(), Some(0), "{}", describe(&restored));
    // the node is registered, with no heartbeat and no workers
    let nodes = cluster_nodes(harness, &cluster).await;
    let node = nodes
        .iter()
        .find(|node| node.name == "forge")
        .unwrap_or_else(|| panic!("the node wasn't restored: {nodes:?}"));
    assert!(matches!(node.health, NodeHealth::Registered), "{node:?}");
    assert_eq!(node.heart_beat, None, "{node:?}");
    assert_eq!(keys(harness, "workers:").await, Vec::<String>::new());
}

/// The names each listing returns for the fixture's main group and sample
#[derive(Debug, PartialEq, Eq)]
struct Listings {
    /// Samples by sha256
    files: Vec<String>,
    /// Samples carrying one of the sample's tags, by sha256
    tagged: Vec<String>,
    /// Repos by url
    repos: Vec<String>,
    /// Entities by id
    entities: Vec<String>,
    /// How many associations the sample has
    associations: usize,
}

/// Lists the fixture's samples, tagged samples, repos, entities, and the sample's associations
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `group` - The group to list in
/// * `sample` - The sample whose tags and associations are listed
async fn listings(harness: &Harness, group: &str, sample: &str) -> Listings {
    // samples, and samples with one of the sample's tags
    let client = harness.client().await;
    let mut files = Vec::new();
    let mut cursor = client
        .files
        .list(&FileListOpts::default().groups(vec![group]).limit(1_000))
        .await
        .unwrap();
    files.extend(cursor.data.drain(..).map(|line| line.sha256));
    while !cursor.exhausted() {
        cursor.refill().await.unwrap();
        files.extend(cursor.data.drain(..).map(|line| line.sha256));
    }
    files.sort();
    let tags = client.files.get(sample).await.unwrap().tags;
    let (key, values) = tags
        .iter()
        .find(|(key, _)| !key.starts_with("submitter"))
        .expect("the sample has a tag");
    let value = values.keys().next().expect("the tag has a value").clone();
    let opts = FileListOpts::default()
        .groups(vec![group])
        .tag(key.clone(), value)
        .limit(1_000);
    let mut tagged: Vec<String> = client
        .files
        .list(&opts)
        .await
        .unwrap()
        .data
        .into_iter()
        .map(|line| line.sha256)
        .collect();
    tagged.sort();
    // repos and entities
    let mut repos: Vec<String> = client
        .repos
        .list(&RepoListOpts::default().groups(vec![group]).limit(1_000))
        .await
        .unwrap()
        .data
        .into_iter()
        .map(|line| line.url)
        .collect();
    repos.sort();
    let mut entities: Vec<String> = client
        .entities
        .list_details(&EntityListOpts::default().groups(vec![group]).limit(1_000))
        .await
        .unwrap()
        .data
        .into_iter()
        .map(|entity| entity.id.to_string())
        .collect();
    entities.sort();
    // the sample's associations, through the route the UI lists them with
    let body: serde_json::Value = reqwest::Client::new()
        .get(format!(
            "{}/api/files/associations/{sample}?groups={group}&limit=1000",
            harness.api_url
        ))
        .header(
            "authorization",
            format!(
                "token {}",
                data_encoding::BASE64.encode(harness.token.as_bytes())
            ),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let associations = body["data"].as_array().map_or(0, Vec::len);
    Listings {
        files,
        tagged,
        repos,
        entities,
        associations,
    }
}

/// Right after a restore, listings of files, tagged files, repos, entities, and associations
/// return the restored objects, since restore rebuilds the census; `thoradm census new` rebuilds
/// the entity and association streams on its own
#[tokio::test]
async fn listings_work_right_after_a_restore() {
    // one of every resource, listed through the API
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    empty_target(harness).await;
    let seeded = fixture::broad(harness).await;
    // every listing returns something, and the entity and association census has streams
    let before = listings(harness, &seeded.group, &seeded.sample).await;
    assert!(
        !before.files.is_empty() && !before.tagged.is_empty(),
        "{before:?}"
    );
    assert!(
        !before.repos.is_empty() && !before.entities.is_empty(),
        "{before:?}"
    );
    assert!(before.associations > 0, "{before:?}");
    let streams = census(harness, &["entities", "associations"]).await;
    assert!(!streams.is_empty());
    // back up everything, restore it into an emptied target, and list again at once
    let out = backup(harness, "listings", "all");
    empty_target(harness).await;
    let (restored, _) = restore(harness, &out);
    assert_eq!(restored.status.code(), Some(0), "{}", describe(&restored));
    // the listings and the census streams are as before
    let after = listings(harness, &seeded.group, &seeded.sample).await;
    assert_eq!(after, before);
    assert_eq!(
        census(harness, &["entities", "associations"]).await,
        streams
    );
    // the entity and association census kinds rebuild their streams on their own
    let pool = redis_pool(&harness.conf).await.unwrap();
    let mut conn = pool.get().await.unwrap();
    for key in streams.keys() {
        let _: () = redis::cmd("DEL")
            .arg(key)
            .query_async(&mut *conn)
            .await
            .unwrap();
    }
    // with the streams deleted, each kind's census brings them back
    assert!(
        census(harness, &["entities", "associations"])
            .await
            .is_empty()
    );
    for kind in ["entities", "associations"] {
        let counted = harness
            .thoradm()
            .args(["census", "new", kind])
            .output()
            .expect("failed to run thoradm");
        assert_eq!(
            counted.status.code(),
            Some(0),
            "{kind}: {}",
            describe(&counted)
        );
    }
    assert_eq!(
        census(harness, &["entities", "associations"]).await,
        streams
    );
}

/// A restore's summary, on stderr and in its report directory, lists each component's counts,
/// the failures file, and the step restore leaves to the admin: reindexing Elastic
#[tokio::test]
async fn the_summary_names_counts_failures_and_the_reindex_step() {
    // restore a small backup
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    empty_target(harness).await;
    // a group with one sample
    let client = harness.client().await;
    let names = common::names::Names::new();
    let group = names.group("vesta");
    common::seed::group(&client, &group).await;
    common::seed::sample(&client, &group, "hearth.bin", names.suffix.as_bytes()).await;
    // back up its row, object, and Redis, and restore them into an emptied target
    let out = backup(
        harness,
        "summary",
        "samples-list,s3-ids,s3-ids-objects,redis",
    );
    empty_target(harness).await;
    let (restored, report) = restore(harness, &out);
    assert_eq!(restored.status.code(), Some(0), "{}", describe(&restored));
    // stderr names each component's counts, the failures file, and the reindex step
    let stderr = String::from_utf8_lossy(&restored.stderr);
    for component in ["samples-list", "s3-ids", "s3-ids-objects", "redis"] {
        assert!(
            stderr.contains(&format!("restore summary: {component}: ")),
            "{component}: {stderr}"
        );
    }
    let failures = report.join("failures.jsonl");
    assert!(stderr.contains(&failures.display().to_string()), "{stderr}");
    assert!(stderr.contains("--reindex"), "{stderr}");
    // so does summary.json
    let summary: serde_json::Value =
        serde_json::from_slice(&std::fs::read(report.join("summary.json")).unwrap()).unwrap();
    assert_eq!(
        summary["components"]["samples-list"]["handled"], 1,
        "{summary:#}"
    );
    assert_eq!(summary["failures_file"], failures.display().to_string());
    // the step left to the admin is reindexing
    let not_done = summary["not_done"].as_array().expect("no not_done");
    assert!(
        not_done
            .iter()
            .any(|step| step.as_str().is_some_and(|step| step.contains("--reindex"))),
        "{summary:#}"
    );
    // the summary warns that the Redis restore cleared the whole database, and, with no
    // notifications in the backup, says nothing of their TTLs
    let warnings = summary["warnings"].as_array().cloned().unwrap_or_default();
    let says = |text: &str| {
        warnings.iter().any(|warning| {
            warning
                .as_str()
                .is_some_and(|warning| warning.contains(text))
        })
    };
    assert!(says("deleted every key of namespace"), "{summary:#}");
    assert!(!says("whole Redis database"), "{summary:#}");
    assert!(!says("default TTL"), "{summary:#}");
    assert!(
        stderr.contains("deleted every key of namespace"),
        "{stderr}"
    );
}

/// Runs `backup verify --cluster` against the namespace and returns its output
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `backup` - The backup to check the cluster against
fn verify_cluster(harness: &Harness, backup: &Path) -> Output {
    // a fresh report directory beside the backup
    let report = backup
        .parent()
        .unwrap()
        .join(format!("cluster-{}", uuid::Uuid::new_v4()));
    harness
        .thoradm()
        .args(["--workers", "2", "backup", "verify", "--cluster", "-b"])
        .arg(backup)
        .arg("--report-dir")
        .arg(&report)
        .output()
        .expect("failed to run thoradm")
}

/// A backup holding Redis but not every censused table, restored with `--merge`, leaves the census
/// of every table rebuilt (the Redis restore deleted every stream), and `verify --cluster` checks
/// the census of the tables the backup doesn't hold too; a backup without Redis leaves, and is
/// checked against, only the census of the tables it holds
#[tokio::test]
async fn a_partial_backup_with_redis_leaves_every_census_rebuilt() {
    // a tagged sample, so the tags table has census streams
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    empty_target(harness).await;
    let client = harness.client().await;
    let names = common::names::Names::new();
    let group = names.group("janus");
    common::seed::group(&client, &group).await;
    let request = thorium::models::SampleRequest::new_buffer(
        thorium::models::Buffer::new(names.suffix.as_bytes().to_vec()).name("gate.bin"),
        vec![group.clone()],
    )
    .tag("doorway", &names.suffix);
    client.files.create(request).await.unwrap();
    let before = census(harness, &["tags", "tags_case_insensitive"]).await;
    assert!(!before.is_empty(), "the tagged sample has no tags census");
    // back up Redis and the samples table only, and restore it over the target
    let out = backup(harness, "partial", "redis,samples-list");
    let report = out.parent().unwrap().join("report");
    let restored = harness
        .thoradm()
        .args([
            "--workers",
            "2",
            "backup",
            "restore",
            "--yes",
            "--merge",
            "-b",
        ])
        .arg(&out)
        .arg("--report-dir")
        .arg(&report)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run thoradm");
    assert_eq!(restored.status.code(), Some(0), "{}", describe(&restored));
    // the tags census is back, and the cluster verifies
    assert_eq!(
        census(harness, &["tags", "tags_case_insensitive"]).await,
        before,
        "{}",
        describe(&restored)
    );
    let verified = verify_cluster(harness, &out);
    assert_eq!(verified.status.code(), Some(0), "{}", describe(&verified));
    // without the tags census, verify --cluster fails naming a tags stream
    let pool = redis_pool(&harness.conf).await.unwrap();
    let mut conn = pool.get().await.unwrap();
    for stream in before.keys() {
        let _: () = redis::cmd("DEL")
            .arg(stream)
            .query_async(&mut *conn)
            .await
            .unwrap();
    }
    let verified = verify_cluster(harness, &out);
    assert_eq!(verified.status.code(), Some(2), "{}", describe(&verified));
    let stderr = String::from_utf8_lossy(&verified.stderr);
    assert!(stderr.contains(":census:tags"), "{stderr}");
    // a partial backup without Redis, restored with --merge, leaves the census of the tables it
    // doesn't hold as it was, and verify --cluster doesn't check them
    let out = backup(harness, "partial-no-redis", "samples-list");
    let report = out.parent().unwrap().join("report");
    let restored = harness
        .thoradm()
        .args([
            "--workers",
            "2",
            "backup",
            "restore",
            "--yes",
            "--merge",
            "-b",
        ])
        .arg(&out)
        .arg("--report-dir")
        .arg(&report)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run thoradm");
    assert_eq!(restored.status.code(), Some(0), "{}", describe(&restored));
    assert!(
        census(harness, &["tags", "tags_case_insensitive"])
            .await
            .is_empty(),
        "a restore without Redis rebuilt the tags census"
    );
    let verified = verify_cluster(harness, &out);
    assert_eq!(verified.status.code(), Some(0), "{}", describe(&verified));
}
