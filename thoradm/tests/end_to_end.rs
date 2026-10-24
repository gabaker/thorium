//! A deployed Thorium restores its own backup, run by hand on a cluster
//!
//! Point the test at a cluster with `THORADM_E2E_CLUSTER_CONF` (the cluster config thoradm reads)
//! and `THORADM_E2E_CTL_CONF` (a Thorctl config holding an admin's token for that cluster's API),
//! and optionally `THORADM_E2E_DIR` (the directory the backup goes in, which must not exist yet; a
//! new directory under the system temp directory by default). Pause every writer but the API (the
//! scaler, event handler, search streamer, and reactors) first. The test seeds a sample, a result
//! with an entity blob, an entity, and a repo through the API, backs the namespace up, and restores
//! the backup with `--wipe`, which empties the namespace's backed-up tables and buckets before the
//! restore deletes the namespace's Redis keys and writes the backup back. It then checks the
//! cluster with `backup verify --cluster` and reads the seeded data back through the API. The
//! seeded data stays in the cluster.
//!
//! With `THORADM_E2E_HARNESS=1` instead, the same steps run on the test harness's own namespace.

mod common;

use common::cluster::{CLUSTER_CONF_VAR, CTL_CONF_VAR, Cluster, HARNESS_VAR};
use common::cycle::describe;
use common::harness::Harness;
use common::names::Names;
use common::seed;
use thorium::client::ResultsClient;
use thorium::models::{EntityKinds, EntityRequest, FileDownloadOpts, ResultGetParams};
use thorium::test_utilities::generators;

/// A cluster backs itself up, restores the backup over its own namespace with `--wipe`, and
/// `backup verify --cluster` passes; the seeded sample, result, entity, and repo read back through
/// the API
#[tokio::test]
#[ignore = "needs a deployed cluster with its writers paused; run by hand: THORADM_E2E_CLUSTER_CONF=<cluster config> THORADM_E2E_CTL_CONF=<Thorctl config with an admin token> cargo test -p thoradm --test end_to_end -- --ignored a_cluster_restores_its_own_backup (or THORADM_E2E_HARNESS=1 instead of the two configs, for the test harness namespace)"]
#[allow(clippy::too_many_lines)]
async fn a_cluster_restores_its_own_backup() {
    // the cluster the environment names, or the harness's namespace, held alone while it restores
    let on_harness = std::env::var(HARNESS_VAR).is_ok_and(|value| value == "1");
    let (cluster, _guard) = if on_harness {
        let harness = Harness::get().await;
        (
            Cluster::of_harness(harness),
            Some(harness.restore_guard().await),
        )
    } else {
        let cluster = Cluster::from_env().unwrap_or_else(|err| {
            panic!(
                "{err}: set {CLUSTER_CONF_VAR} and {CTL_CONF_VAR}, or {HARNESS_VAR}=1 for the \
                 test harness"
            )
        });
        (cluster, None)
    };
    // seed a sample, a result with an entity blob, an entity, and a repo
    let client = cluster.client().await;
    let names = Names::new();
    let group = names.group("vesta");
    seed::group(&client, &group).await;
    let data = format!("end to end sample {}", names.suffix).into_bytes();
    let sample = seed::sample(&client, &group, "end-to-end.bin", &data).await;
    let blob = br#"[{"name":"end to end device"}]"#;
    let result =
        seed::result_with_entities(&client, &sample, &[(EntityKinds::Device, 1, blob)]).await;
    let entity_name = names.user("thebe");
    let entity = client
        .entities
        .create(EntityRequest::new(
            &entity_name,
            generators::gen_vendor_meta(),
            [&group],
        ))
        .await
        .expect("failed to create an entity")
        .id;
    let repo = generators::repos(&group, 1, &client)
        .await
        .expect("failed to create a repo")
        .remove(0)
        .url;
    // back the namespace up into a new directory
    let backup = cluster.backup_dir();
    let output = cluster
        .thoradm()
        .args(["backup", "new", "-o"])
        .arg(&backup)
        .output()
        .expect("failed to run thoradm");
    assert_eq!(
        output.status.code(),
        Some(0),
        "backup: {}",
        describe(&output)
    );
    // restore it over the namespace, wiping what the namespace holds first
    let output = cluster
        .thoradm()
        .args([
            "backup",
            "restore",
            "--confirm",
            "--wipe",
            &cluster.namespace,
            "-b",
        ])
        .arg(&backup)
        .output()
        .expect("failed to run thoradm");
    assert_eq!(
        output.status.code(),
        Some(0),
        "restore: {}",
        describe(&output)
    );
    // the cluster matches the backup
    let output = cluster
        .thoradm()
        .args(["backup", "verify", "--cluster", "-b"])
        .arg(&backup)
        .output()
        .expect("failed to run thoradm");
    assert_eq!(
        output.status.code(),
        Some(0),
        "verify --cluster: {}",
        describe(&output)
    );
    // the sample's details and bytes read back
    let client = cluster.client().await;
    let details = client
        .files
        .get(&sample)
        .await
        .expect("the sample doesn't read back");
    assert!(details.groups().contains(group.as_str()), "{details:?}");
    // the download goes beside the backup, which stays as thoradm wrote it
    let downloaded = backup.with_extension("sample");
    client
        .files
        .download(
            &sample,
            &downloaded,
            &mut FileDownloadOpts::default().uncart(),
        )
        .await
        .expect("the sample's bytes don't download");
    assert_eq!(
        std::fs::read(&downloaded).expect("failed to read the downloaded sample"),
        data
    );
    // the result reads back with its entity blob
    let results = client
        .files
        .get_results(&sample, &ResultGetParams::default())
        .await
        .expect("the sample's results don't read back");
    assert!(
        results
            .results
            .values()
            .flatten()
            .any(|output| output.id == result),
        "result {result} isn't among {results:?}"
    );
    // the entity and the repo read back
    let read = client
        .entities
        .get(entity)
        .await
        .expect("the entity doesn't read back");
    assert_eq!(read.name, entity_name);
    let read = client
        .repos
        .get(&repo)
        .await
        .expect("the repo doesn't read back");
    assert!(read.groups().contains(&group), "{read:?}");
    // the harness's namespace is emptied again once the test has passed
    if on_harness {
        Harness::get()
            .await
            .cleanup()
            .await
            .expect("failed to clean up the harness namespace");
    }
}
