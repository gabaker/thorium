//! Restore checks every partition against its checksum as it reads it, so a file that changes
//! after the preflight's verify still never writes damaged rows

mod common;

use common::cycle::{describe, wipe};
use common::harness::Harness;
use common::names::Names;
use common::seed;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use thoradm::backup::controllers::restore::RestoreController;
use thoradm::backup::failures::{
    FAILURES_FILE, FailureLog, FailurePolicy, Operation, read_records,
};
use thoradm::backup::sink::TokioFs;
use thoradm::backup::tables::SamplesList;
use thoradm::backup::{ArchiveReader, PartitionArchive};
use thoradm::outcome::RunOutcome;
use thorium::CtlConf;

/// Returns the sha256s of the samples the namespace's samples list holds
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
async fn listed_samples(harness: &Harness) -> BTreeSet<String> {
    // every row of the samples list
    let scylla = harness.scylla().await;
    let query = format!(
        "SELECT sha256 FROM {}.samples_list",
        harness.conf.thorium.namespace
    );
    scylla
        .query_unpaged(query, &[])
        .await
        .unwrap()
        .into_rows_result()
        .unwrap()
        .rows::<(String,)>()
        .unwrap()
        .map(|row| row.unwrap().0)
        .collect()
}

/// Returns every partition of every archive of a table, with its map and the sha256s it holds
///
/// # Arguments
///
/// * `table_dir` - The table's directory in a backup
async fn partitions(table_dir: &std::path::Path) -> Vec<(PathBuf, PartitionArchive, Vec<String>)> {
    // read each archive's partitions and decode their samples
    let mut found = Vec::new();
    for map in common::archives::maps(table_dir) {
        let mut reader = ArchiveReader::new(map.clone()).await.unwrap();
        while let Some((bytes, entry)) = reader.next_partition().await.unwrap() {
            let rows = rkyv::check_archived_root::<Vec<SamplesList>>(bytes).unwrap();
            let shas = rows
                .iter()
                .filter_map(|row| row.sha256.as_ref().map(ToString::to_string))
                .collect();
            found.push((map.clone(), entry, shas));
        }
    }
    found
}

/// The table restore step, driven past the preflight on a backup with one byte of one partition
/// flipped, records that partition, writes none of its rows, restores every other partition, and
/// finishes with failures
#[tokio::test]
async fn a_partition_failing_its_checksum_is_recorded_and_not_written() {
    // samples in three groups, so the samples list has several partitions
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let client = harness.client().await;
    let names = Names::new();
    for god in ["apollo", "diana", "mercury"] {
        let group = names.group(god);
        seed::group(&client, &group).await;
        seed::sample(&client, &group, &format!("{god}.bin"), god.as_bytes()).await;
    }
    // back up the samples list
    let out = harness.scratch("flipped").join("backup");
    let taken = harness
        .thoradm()
        .args(["backup", "new", "samples-list", "-o"])
        .arg(&out)
        .output()
        .expect("failed to run thoradm");
    assert_eq!(taken.status.code(), Some(0), "{}", describe(&taken));
    // flip a byte inside one partition's stored sha256, which still decodes as a row
    let found = partitions(&out.join("samples_list")).await;
    assert!(found.len() >= 2, "too few partitions: {}", found.len());
    let (map, entry, damaged) = found.iter().find(|(_, _, shas)| !shas.is_empty()).unwrap();
    // the data file beside the partition's map, and where the sha256 sits in it
    let data = map
        .parent()
        .and_then(std::path::Path::parent)
        .unwrap()
        .join("data")
        .join(map.file_stem().unwrap());
    let bytes = std::fs::read(&data).unwrap();
    let partition =
        &bytes[usize::try_from(entry.start).unwrap()..usize::try_from(entry.end).unwrap()];
    let at = partition
        .windows(damaged[0].len())
        .position(|window| window == damaged[0].as_bytes())
        .expect("the sha256 isn't in its partition's bytes");
    // change its first character to another hex digit
    let mut changed = bytes.clone();
    let offset = usize::try_from(entry.start).unwrap() + at;
    changed[offset] = if changed[offset] == b'a' { b'b' } else { b'a' };
    std::fs::write(&data, changed).unwrap();
    // restore the table through the controller into the emptied namespace
    wipe(harness).await;
    let reports = harness.scratch("flipped-report");
    let log = Arc::new(
        FailureLog::open(&reports, FailurePolicy::Continue, &TokioFs)
            .await
            .unwrap(),
    );
    // restore through a controller of this cluster
    let ctl_conf = CtlConf::from_path(&harness.ctl_conf).unwrap();
    let scylla = harness.scylla().await;
    let mut controller = RestoreController::new(&harness.conf, ctl_conf, &scylla, 2);
    Box::pin(controller.restore(&out, &log))
        .await
        .expect("the restore stopped instead of recording the partition");
    // it finished with the partition recorded: table, archive, offset, and partition hash
    assert_eq!(
        RunOutcome::finished(&log),
        RunOutcome::CompletedWithFailures
    );
    // one verify record names the table, the map, the partition, and its bytes
    let records = read_records(&reports.join(FAILURES_FILE)).unwrap();
    let record = records
        .iter()
        .find(|record| record.operation == Operation::Verify)
        .unwrap_or_else(|| panic!("no record of the damaged partition: {records:#?}"));
    assert_eq!(record.table.as_deref(), Some("samples_list"), "{record:?}");
    assert_eq!(
        record.key.as_deref(),
        Some(map.to_string_lossy().as_ref()),
        "{record:?}"
    );
    assert_eq!(
        record.object,
        Some(entry.partition.to_string()),
        "{record:?}"
    );
    assert!(record.error.contains("checksum"), "{record:?}");
    assert!(
        record
            .error
            .contains(&format!("bytes {} to {}", entry.start, entry.end)),
        "{record:?}"
    );
    // none of its rows were written, not even with the damaged value, and every other
    // partition's were
    let restored = listed_samples(harness).await;
    let intact: BTreeSet<String> = found
        .iter()
        .filter(|(_, other, _)| !(other.partition == entry.partition && other.start == entry.start))
        .flat_map(|(_, _, shas)| shas.iter().cloned())
        .collect();
    assert_eq!(
        restored, intact,
        "the restored rows aren't exactly the intact ones"
    );
    // row by row, the damaged partition's are missing and every other one is there
    for (_, other, shas) in &found {
        for sha in shas {
            if other.partition == entry.partition && other.start == entry.start {
                assert!(!restored.contains(sha), "a damaged row was restored: {sha}");
            } else {
                assert!(restored.contains(sha), "an intact row is missing: {sha}");
            }
        }
    }
}
