//! Associations, including bidirectional ones, are backed up without loss

mod common;

use common::harness::Harness;
use common::names::Names;
use thoradm::backup::failures::{FAILURES_FILE, read_records};
use thorium::models::{AssociationKind, AssociationRequest, AssociationTarget, EntityRequest};
use thorium::test_utilities::generators;

/// Creates a flag entity with a bidirectional association to a new sample, returning the
/// directions the API stored
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `names` - This test's names
async fn bidirectional_flag(harness: &Harness, names: &Names) -> Vec<String> {
    // a sample and a flag entity in one group
    let client = harness.client().await;
    let group = names.group("juno");
    common::seed::group(&client, &group).await;
    let sample = common::seed::sample(&client, &group, "juno.bin", b"both ways").await;
    let flag_name = names.user("thebe");
    let flag = EntityRequest::new(&flag_name, generators::gen_flag_meta(), vec![group.clone()]);
    let flag = client.entities.create(flag).await.unwrap();
    // associate them in both directions
    let association = AssociationRequest::new(
        AssociationKind::FlagFor,
        AssociationTarget::Entity {
            id: flag.id,
            name: flag_name,
        },
    )
    .target(AssociationTarget::File(sample))
    .groups(vec![group.clone()])
    .biderectional();
    client.associations.create(&association).await.unwrap();
    // read the directions back the way backup does
    let scylla = harness.scylla().await;
    let query = format!(
        "SELECT direction FROM {}.associations WHERE group = ? ALLOW FILTERING",
        harness.conf.thorium.namespace
    );
    scylla
        .query_unpaged(query, (group.as_str(),))
        .await
        .unwrap()
        .into_rows_result()
        .unwrap()
        .rows::<(String,)>()
        .unwrap()
        .map(|row| row.unwrap().0)
        .collect()
}

/// A bidirectional association is backed up in full: both of its rows are archived and nothing
/// is skipped
#[tokio::test]
async fn a_bidirectional_association_is_backed_up() {
    // the API stores one row per direction
    let harness = Harness::get().await;
    let _guard = harness.shared_guard().await;
    let names = Names::new();
    let directions = bidirectional_flag(harness, &names).await;
    assert_eq!(directions, vec!["Bidirectional", "Bidirectional"]);
    // back up associations
    let out = harness.scratch("bidirectional");
    let output = harness
        .thoradm()
        .args([
            "--workers",
            "1",
            "backup",
            "new",
            "associations",
            "-m",
            "1",
            "-o",
        ])
        .arg(&out)
        .output()
        .expect("failed to run thoradm");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let records = read_records(&out.join(FAILURES_FILE)).unwrap_or_default();
    // every row, both of this association's included, was archived and nothing was recorded;
    // other tests of the binary may have left associations of their own
    assert_eq!(output.status.code(), Some(0), "{records:?}\n{stderr}");
    assert!(records.is_empty(), "{records:?}");
    let scylla = harness.scylla().await;
    let rows = scylla
        .query_unpaged(
            format!(
                "SELECT direction FROM {}.associations",
                harness.conf.thorium.namespace
            ),
            &[],
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap()
        .rows_num();
    assert!(rows >= 2, "{rows} association rows");
    assert!(
        stderr.contains(&format!(
            "associations: {rows} handled, 0 skipped, 0 failed"
        )),
        "{stderr}"
    );
}

/// A bidirectional association comes back from a restore with both of its rows
#[tokio::test]
async fn a_bidirectional_association_survives_backup_and_restore() {
    // back up one bidirectional association
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let names = Names::new();
    let before = bidirectional_flag(harness, &names).await;
    let out = harness.scratch("bidirectional-restore");
    let backed_up = common::cycle::backup(harness, &out);
    assert!(matches!(backed_up.status.code(), Some(0 | 2)));
    // wipe the namespace and restore the backup
    common::cycle::wipe(harness).await;
    let restored = common::cycle::restore(harness, &out);
    assert!(matches!(restored.status.code(), Some(0 | 2)));
    // both rows are back with their direction
    let scylla = harness.scylla().await;
    let query = format!(
        "SELECT direction FROM {}.associations WHERE group = ? ALLOW FILTERING",
        harness.conf.thorium.namespace
    );
    let group = names.group("juno");
    let after: Vec<String> = scylla
        .query_unpaged(query, (group.as_str(),))
        .await
        .unwrap()
        .into_rows_result()
        .unwrap()
        .rows::<(String,)>()
        .unwrap()
        .map(|row| row.unwrap().0)
        .collect();
    assert_eq!(after, before);
}
