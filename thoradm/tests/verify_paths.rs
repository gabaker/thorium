//! `backup verify` and restore read only files inside the backup they use: a manifest can't name
//! an index or sibling outside the backup root, an index key can't climb out of `objects/`, and a
//! symlinked component, `objects/`, `maps/`, `data/`, index, map file, archive file, object,
//! `redis.json`, `schema.json`, `pruned.jsonl`, or `failures.jsonl` is refused instead of
//! followed, and a directory that can't be read is recorded
//!
//! These tests need no cluster: each copies the format-2 golden backup, damages the copy, and runs
//! `thoradm backup verify` on it, or restore's own directory walks.

use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;
use thoradm::args::BackupComponents;
use thoradm::backup::ArchiveReader;
use thoradm::backup::controllers::restore::{walk_maps, walk_objects};
use thoradm::backup::failures::{
    FAILURES_FILE, FailureLog, FailurePolicy, FailureRecord, read_records,
};
use thoradm::backup::manifest::{MANIFEST_FILE, PRUNED_FILE, file_sha256};
use thoradm::backup::sink::TokioFs;

/// The contents of the file outside every backup that no verify run may read
const SECRET: &[u8] = b"a host file that is not part of any backup\n";

/// One test's scratch directory: a copy of the golden backup and a secret file beside it
struct Case {
    /// The scratch directory holding everything below
    dir: PathBuf,
    /// The copy of the golden backup
    backup: PathBuf,
    /// A file outside the backup root
    secret: PathBuf,
    /// The secret file's sha256
    secret_sha256: String,
}

impl Case {
    /// Copies the format-2 golden backup into a fresh scratch directory beside a secret file
    ///
    /// # Arguments
    ///
    /// * `name` - The test's name, used in the scratch directory's name
    async fn new(name: &str) -> Self {
        // a fresh directory per test run
        let dir = std::env::temp_dir().join(format!("thoradm-{name}-{}", uuid::Uuid::new_v4()));
        let backup = dir.join("backup");
        // the golden backup, copied so the committed fixture never changes
        let golden =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/golden/format-2/backup");
        copy_dir(&golden, &backup);
        // a file outside the backup root whose contents no report may describe
        let secret = dir.join("secret.txt");
        std::fs::write(&secret, SECRET).unwrap();
        let secret_sha256 = file_sha256(&secret).await.unwrap();
        Case {
            dir,
            backup,
            secret,
            secret_sha256,
        }
    }

    /// Reads the copy's manifest as JSON
    fn manifest(&self) -> Value {
        // the manifest is plain JSON at the backup root
        serde_json::from_slice(&std::fs::read(self.backup.join(MANIFEST_FILE)).unwrap()).unwrap()
    }

    /// Writes the copy's manifest, the way someone who can write the backup would
    ///
    /// # Arguments
    ///
    /// * `manifest` - The manifest to write
    fn write_manifest(&self, manifest: &Value) {
        // the manifest's own checksums are unkeyed, so anyone who can write the backup can rewrite it
        std::fs::write(
            self.backup.join(MANIFEST_FILE),
            serde_json::to_vec_pretty(manifest).unwrap(),
        )
        .unwrap();
    }

    /// Runs `backup verify` on the copy with a report directory beside it
    fn verify(&self) -> Verified {
        // a report directory outside the backup, so each run's records are its own
        let reports = self.dir.join(format!("report-{}", uuid::Uuid::new_v4()));
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_thoradm"))
            .args(["--workers", "2", "backup", "verify", "-b"])
            .arg(&self.backup)
            .arg("--report-dir")
            .arg(&reports)
            .output()
            .expect("failed to run thoradm");
        // the failures it recorded and every byte it wrote to its reports
        let records = read_records(&reports.join(FAILURES_FILE)).unwrap_or_default();
        let mut reported = String::from_utf8_lossy(&output.stderr).into_owned();
        for entry in std::fs::read_dir(&reports).into_iter().flatten().flatten() {
            reported.push_str(&String::from_utf8_lossy(
                &std::fs::read(entry.path()).unwrap_or_default(),
            ));
        }
        Verified {
            output,
            records,
            reported,
        }
    }
}

impl Drop for Case {
    /// Removes the scratch directory, following no link inside it
    fn drop(&mut self) {
        // remove_dir_all removes symlinks themselves, not what they point at
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// What one verify run did
struct Verified {
    /// The process output
    output: Output,
    /// The failure records it wrote
    records: Vec<FailureRecord>,
    /// Its stderr and every report file it wrote, as text
    reported: String,
}

impl Verified {
    /// Returns whether verify passed the backup
    fn passed(&self) -> bool {
        // exit 0 is a complete run without failures
        self.output.status.code() == Some(0)
    }

    /// Describes the run for an assertion message
    fn describe(&self) -> String {
        // the exit code, each record, and stderr
        format!(
            "exit {:?}\nrecords:\n{}\nstderr:\n{}",
            self.output.status.code(),
            self.records
                .iter()
                .map(|record| format!("  {} {:?}: {}", record.component, record.key, record.error))
                .collect::<Vec<_>>()
                .join("\n"),
            String::from_utf8_lossy(&self.output.stderr)
        )
    }
}

/// Copies a directory tree, copying symlinks as files
///
/// # Arguments
///
/// * `from` - The directory to copy
/// * `to` - Where to put the copy
fn copy_dir(from: &Path, to: &Path) {
    // copy every file and directory below the root
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().filter_map(Result::ok) {
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// Moves a directory or file out of the backup and leaves a symlink to it in its place
///
/// # Arguments
///
/// * `inside` - The path inside the backup to replace with a link
/// * `outside` - Where its contents go, outside the backup root
fn replace_with_link(inside: &Path, outside: &Path) {
    // the contents move out of the backup and a link takes their place
    std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
    std::fs::rename(inside, outside).unwrap();
    std::os::unix::fs::symlink(outside, inside).unwrap();
}

/// Takes every permission away from a directory until dropped, so nothing below it can be read
struct Unreadable {
    /// The directory whose permissions were taken away
    dir: PathBuf,
}

impl Unreadable {
    /// Takes every permission away from a directory, or returns `None` when the paths below it
    /// can still be read, as they can for root
    ///
    /// # Arguments
    ///
    /// * `dir` - The directory to make unreadable
    fn new(dir: &Path) -> Option<Self> {
        // no read, write, or search permission for anyone
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let locked = Unreadable {
            dir: dir.to_path_buf(),
        };
        // a user who ignores permissions can still read below it, so the case can't be built
        let denied = std::fs::symlink_metadata(dir.join("probe"))
            .is_err_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied);
        denied.then_some(locked)
    }
}

impl Drop for Unreadable {
    /// Gives the directory its permissions back, so the scratch directory can be removed
    fn drop(&mut self) {
        // the owner's usual permissions
        let _ = std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o755));
    }
}

/// What one of restore's directory walks did
struct Walked {
    /// The paths it sent to its workers, sorted
    sent: Vec<PathBuf>,
    /// The failures it recorded
    records: Vec<FailureRecord>,
}

/// Collects what a walk sent to its workers and the failures it recorded
///
/// # Arguments
///
/// * `rx` - The channel the walk sent its orders on
/// * `log` - The failure log the walk recorded in
fn walked(rx: &kanal::AsyncReceiver<PathBuf>, log: &FailureLog) -> Walked {
    // drain every order the walk sent
    let mut sent = Vec::new();
    while let Ok(Some(path)) = rx.try_recv() {
        sent.push(path);
    }
    sent.sort();
    // and read back what it recorded
    let records = read_records(log.path()).unwrap_or_default();
    Walked { sent, records }
}

/// Runs restore's walk of an S3 component's `objects/` directory in the copy
///
/// # Arguments
///
/// * `case` - The test's case
/// * `component` - The component's directory in the backup
async fn restore_object_walk(case: &Case, component: &str) -> Walked {
    // a failure log of its own beside the backup
    let reports = case.dir.join(format!("walk-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&reports).unwrap();
    let log = FailureLog::open(&reports, FailurePolicy::Continue, &TokioFs)
        .await
        .unwrap();
    // the walk restore runs, keeping what it sends to its upload workers
    let (tx, rx) = kanal::unbounded_async();
    walk_objects(
        &case.backup.join(component),
        BackupComponents::Graphics,
        component,
        &log,
        &tx,
    )
    .await
    .unwrap();
    walked(&rx, &log)
}

/// Runs restore's walk of a table's `maps/` directory in the copy
///
/// # Arguments
///
/// * `case` - The test's case
/// * `table` - The table
async fn restore_map_walk(case: &Case, table: &str) -> Walked {
    // a failure log of its own beside the backup
    let reports = case.dir.join(format!("walk-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&reports).unwrap();
    let log = FailureLog::open(&reports, FailurePolicy::Continue, &TokioFs)
        .await
        .unwrap();
    // the walk restore runs, keeping what it sends to its workers
    let (tx, rx) = kanal::unbounded_async();
    walk_maps(
        &case.backup,
        BackupComponents::Associations,
        table,
        &log,
        &tx,
    )
    .await
    .unwrap();
    walked(&rx, &log)
}

/// The untouched golden copy verifies clean, so every failure below comes from its damage
#[tokio::test]
async fn the_golden_copy_verifies_clean() {
    // verify the copy as it is
    let case = Case::new("clean").await;
    let run = case.verify();
    assert!(run.passed(), "{}", run.describe());
}

/// Lists a sibling under the given name, with the secret file's length and a wrong sha256, and
/// checks verify refuses the name without describing the secret file
///
/// # Arguments
///
/// * `case` - The test's case
/// * `name` - The sibling name to list in the manifest
fn refuses_sibling(case: &Case, name: &str) {
    // list the sibling with a wrong sha256, so verify reports what it finds there
    let mut manifest = case.manifest();
    manifest["siblings"] =
        serde_json::json!([{ "name": name, "len": 0, "sha256": "0".repeat(64) }]);
    case.write_manifest(&manifest);
    let run = case.verify();
    // verify refuses the manifest, and reads nothing outside the backup to do so
    assert!(
        !run.passed(),
        "verify passed a sibling named {name}\n{}",
        run.describe()
    );
    assert!(
        !run.reported.contains(&case.secret_sha256),
        "verify hashed {} outside the backup and reported its sha256 for sibling {name}\n{}",
        case.secret.display(),
        run.describe()
    );
}

/// an absolute sibling name is refused without hashing the file it names
#[tokio::test]
async fn an_absolute_sibling_name_is_refused_without_reading_it() {
    // name the secret file by its absolute path
    let case = Case::new("sibling-absolute").await;
    let name = case.secret.to_string_lossy().into_owned();
    refuses_sibling(&case, &name);
}

/// a sibling name with `..` segments is refused without hashing the file it names
#[tokio::test]
async fn a_dotdot_sibling_name_is_refused_without_reading_it() {
    // name the secret file relative to the backup root
    let case = Case::new("sibling-dotdot").await;
    refuses_sibling(&case, "../secret.txt");
}

/// Points the graphics index at a decoy copy of the component outside the backup, plants an
/// object only restore would see in the real `graphics/objects`, and checks verify fails
///
/// # Arguments
///
/// * `case` - The test's case
/// * `index` - The index path to list in the manifest, given the decoy component directory
async fn refuses_decoy_index(case: &Case, index: impl Fn(&Path) -> String) {
    // a decoy copy of the clean graphics component, outside the backup root
    let decoy = case.dir.join("decoy/graphics");
    copy_dir(&case.backup.join("graphics"), &decoy);
    // an object no index lists, in the objects directory restore uploads from
    let planted = case.backup.join("graphics/objects/planted/planted.png");
    std::fs::create_dir_all(planted.parent().unwrap()).unwrap();
    std::fs::write(&planted, b"uploaded by restore, never checked by verify").unwrap();
    assert!(
        restore_object_walk(case, "graphics")
            .await
            .sent
            .contains(&planted),
        "restore's walk should upload the planted object"
    );
    // the manifest names the decoy's index, whose sha256 it already holds
    let mut manifest = case.manifest();
    let named = index(&decoy);
    manifest["s3"]["graphics"]["index"] = Value::String(named.clone());
    case.write_manifest(&manifest);
    let run = case.verify();
    // verify must refuse the index path instead of vouching for a directory restore never reads
    assert!(
        !run.passed(),
        "verify passed with the graphics index at {named}, while restore uploads the unchecked {}\n{}",
        planted.display(),
        run.describe()
    );
    // and name the manifest's index path as the problem, not just the planted object
    assert!(
        run.records
            .iter()
            .any(|record| record.key.as_deref() == Some(MANIFEST_FILE)),
        "verify didn't refuse the graphics index at {named}\n{}",
        run.describe()
    );
}

/// an absolute index path can't point verify at a decoy `objects/` directory
#[tokio::test]
async fn an_absolute_index_path_is_refused() {
    // the decoy's index by its absolute path
    let case = Case::new("index-absolute").await;
    refuses_decoy_index(&case, |decoy| {
        decoy
            .join("objects.index.jsonl")
            .to_string_lossy()
            .into_owned()
    })
    .await;
}

/// an index path with `..` segments can't point verify at a decoy `objects/` directory
#[tokio::test]
async fn a_dotdot_index_path_is_refused() {
    // the decoy's index relative to the backup root
    let case = Case::new("index-dotdot").await;
    refuses_decoy_index(&case, |_| {
        "graphics/../../decoy/graphics/objects.index.jsonl".to_owned()
    })
    .await;
}

/// an index line whose key climbs out of `objects/` is refused without hashing the file
/// it reaches
#[tokio::test]
async fn an_index_key_with_dotdot_is_refused_without_reading_it() {
    // an index line whose key resolves to the secret file: objects/<prefix "..">/../../secret.txt
    let case = Case::new("index-key").await;
    let index = case.backup.join("graphics/objects.index.jsonl");
    let line = serde_json::json!({
        "bucket": "testing-bv5genformat2-golden-graphics",
        "key": "../../secret.txt",
        "size": SECRET.len(),
        "etag": "\"\"",
        "sha256": "0".repeat(64),
    });
    let mut contents = std::fs::read(&index).unwrap();
    contents.extend_from_slice(format!("{line}\n").as_bytes());
    std::fs::write(&index, contents).unwrap();
    // the manifest pins the rewritten index and its counts
    let mut manifest = case.manifest();
    let entry = &mut manifest["s3"]["graphics"];
    entry["index_sha256"] = Value::String(file_sha256(&index).await.unwrap());
    entry["objects"] = Value::from(entry["objects"].as_u64().unwrap() + 1);
    entry["bytes"] = Value::from(entry["bytes"].as_u64().unwrap() + SECRET.len() as u64);
    case.write_manifest(&manifest);
    let run = case.verify();
    // verify refuses the key, and reads nothing outside the backup to do so
    assert!(!run.passed(), "{}", run.describe());
    assert!(
        !run.reported.contains(&case.secret_sha256),
        "verify hashed {} through an index key and reported its sha256\n{}",
        case.secret.display(),
        run.describe()
    );
}

/// a symlinked `objects/` directory is refused by verify and by restore's walk
#[tokio::test]
async fn a_symlinked_objects_directory_is_refused() {
    // move graphics/objects out of the backup and link to it
    let case = Case::new("objects-link").await;
    let objects = case.backup.join("graphics/objects");
    let outside = case.dir.join("elsewhere/objects");
    replace_with_link(&objects, &outside);
    // restore's walk refuses the link instead of uploading what it points at
    let walk = restore_object_walk(&case, "graphics").await;
    assert!(
        walk.sent.is_empty() && walk.records.len() == 1,
        "restore's walk should refuse the link: sent {:?}, recorded {:?}",
        walk.sent,
        walk.records
    );
    let run = case.verify();
    assert!(
        !run.passed(),
        "verify passed with graphics/objects a link to {}\n{}",
        outside.display(),
        run.describe()
    );
}

/// a symlinked component directory is refused by verify and by restore's object walk
#[tokio::test]
async fn a_symlinked_component_directory_is_refused() {
    // move the whole graphics component out of the backup and link to it
    let case = Case::new("component-link").await;
    let component = case.backup.join("graphics");
    let outside = case.dir.join("elsewhere/graphics");
    replace_with_link(&component, &outside);
    // restore's walk refuses the link instead of uploading the objects below it
    let walk = restore_object_walk(&case, "graphics").await;
    assert!(
        walk.sent.is_empty()
            && walk.records.len() == 1
            && walk.records[0].key.as_deref() == Some(&*component.to_string_lossy()),
        "restore's walk should refuse the linked component: sent {:?}, recorded {:?}",
        walk.sent,
        walk.records
    );
    let run = case.verify();
    assert!(
        !run.passed(),
        "verify passed with graphics a link to {}\n{}",
        outside.display(),
        run.describe()
    );
}

/// a symlinked object is a verify failure, and restore's walk records it instead of uploading it
#[tokio::test]
async fn a_symlinked_object_is_refused() {
    // move one listed object out of the backup and link to it
    let case = Case::new("object-link").await;
    let object = case
        .backup
        .join("graphics/objects/fixtur/fixture/layout-AllBytes.png");
    let outside = case.dir.join("elsewhere/layout-AllBytes.png");
    replace_with_link(&object, &outside);
    // restore's walk doesn't upload the link, and records that the object isn't restored
    let walk = restore_object_walk(&case, "graphics").await;
    let rel = object.to_string_lossy();
    assert!(
        !walk.sent.contains(&object)
            && walk
                .records
                .iter()
                .any(|record| record.key.as_deref() == Some(&*rel)),
        "restore's walk should record the link: sent {:?}, recorded {:?}",
        walk.sent,
        walk.records
    );
    let run = case.verify();
    assert!(
        !run.passed(),
        "verify passed an object restore skips (a link to {})\n{}",
        outside.display(),
        run.describe()
    );
}

/// a symlinked `maps/` directory is refused by verify and by restore's map walk
#[tokio::test]
async fn a_symlinked_maps_directory_is_refused() {
    // move associations/maps out of the backup and link to it
    let case = Case::new("maps-link").await;
    let outside = case.dir.join("elsewhere/maps");
    replace_with_link(&case.backup.join("associations/maps"), &outside);
    // restore's map walk refuses the link instead of loading the maps it points at
    let walk = restore_map_walk(&case, "associations").await;
    assert!(
        walk.sent.is_empty() && walk.records.len() == 1,
        "restore's map walk should refuse the link: sent {:?}, recorded {:?}",
        walk.sent,
        walk.records
    );
    let run = case.verify();
    assert!(
        !run.passed(),
        "verify passed with associations/maps a link to {}\n{}",
        outside.display(),
        run.describe()
    );
}

/// a symlinked `data/` directory is refused by verify and by restore's map walk, since restore
/// would read what it points at
#[tokio::test]
async fn a_symlinked_data_directory_is_refused() {
    // move associations/data out of the backup and link to it
    let case = Case::new("data-link").await;
    let outside = case.dir.join("elsewhere/data");
    replace_with_link(&case.backup.join("associations/data"), &outside);
    // restore's map walk refuses the link instead of sending maps whose data it points at
    let walk = restore_map_walk(&case, "associations").await;
    assert!(
        walk.sent.is_empty() && walk.records.len() == 1,
        "restore's map walk should refuse the linked data directory: sent {:?}, recorded {:?}",
        walk.sent,
        walk.records
    );
    let run = case.verify();
    assert!(
        !run.passed(),
        "verify passed with associations/data a link to {}\n{}",
        outside.display(),
        run.describe()
    );
}

/// a symlinked archive data file is refused by verify and by the reader the table restore opens
/// archives with, since the restore would read what it points at
#[tokio::test]
async fn a_symlinked_archive_file_is_refused() {
    // move one archive's data file out of the backup and link to it
    let case = Case::new("archive-link").await;
    let data = std::fs::read_dir(case.backup.join("associations/data"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let outside = case.dir.join("elsewhere/data-file");
    replace_with_link(&data, &outside);
    // the archive reader refuses to open the data file through the link
    let uuid = data.file_name().unwrap().to_string_lossy().into_owned();
    let map = case
        .backup
        .join("associations/maps")
        .join(format!("{uuid}.thoriummap"));
    match ArchiveReader::new(map).await {
        Ok(_) => panic!(
            "the archive reader opened {} through a link",
            data.display()
        ),
        Err(error) => assert!(
            error.to_string().contains("symbolic link"),
            "the archive reader should name the link: {error}"
        ),
    }
    let run = case.verify();
    assert!(
        !run.passed(),
        "verify passed with {} a link to {}\n{}",
        data.display(),
        outside.display(),
        run.describe()
    );
}

/// Checks that verify and one of restore's walks record a path they can't read below a directory
/// whose permissions were taken away, instead of stopping the whole run
///
/// # Arguments
///
/// * `walk` - What restore's walk did with the directory unreadable
/// * `run` - What verify did with the directory unreadable
/// * `component` - The component the directory belongs to
fn records_unreadable(walk: &Walked, run: &Verified, component: &str) {
    // restore's walk sends nothing from the directory and records why
    assert!(
        walk.sent.is_empty()
            && walk.records.len() == 1
            && walk.records[0].error.contains("can't be read"),
        "restore's walk should record the unreadable path: sent {:?}, recorded {:?}",
        walk.sent,
        walk.records
    );
    // verify finishes with failures rather than aborting, and names the component
    assert_eq!(
        run.output.status.code(),
        Some(2),
        "verify should finish with failures\n{}",
        run.describe()
    );
    assert!(
        run.records
            .iter()
            .any(|record| record.component == component && record.error.contains("can't be read")),
        "verify didn't record the unreadable {component} path\n{}",
        run.describe()
    );
}

/// an S3 component directory whose contents can't be read is recorded by verify and by
/// restore's object walk, instead of stopping either run
#[tokio::test]
async fn an_unreadable_component_directory_is_recorded() {
    // take every permission away from the graphics component directory
    let case = Case::new("component-unreadable").await;
    let Some(_locked) = Unreadable::new(&case.backup.join("graphics")) else {
        eprintln!("skipped: permissions don't stop this user from reading");
        return;
    };
    // both the restore walk and verify record it
    let walk = restore_object_walk(&case, "graphics").await;
    let run = case.verify();
    records_unreadable(&walk, &run, "graphics");
}

/// a table directory whose contents can't be read is recorded by verify and by restore's map
/// walk, instead of stopping either run
#[tokio::test]
async fn an_unreadable_table_directory_is_recorded() {
    // take every permission away from the associations table directory
    let case = Case::new("table-unreadable").await;
    let Some(_locked) = Unreadable::new(&case.backup.join("associations")) else {
        eprintln!("skipped: permissions don't stop this user from reading");
        return;
    };
    // both the restore walk and verify record it
    let walk = restore_map_walk(&case, "associations").await;
    let run = case.verify();
    records_unreadable(&walk, &run, "associations");
}

/// Checks that verify refused a file in the backup because it is a symbolic link
///
/// # Arguments
///
/// * `run` - What verify did
/// * `file` - The file's path relative to the backup root
fn refused_link(run: &Verified, file: &str) {
    // verify fails, and the failure names the file as a link
    assert!(
        !run.passed(),
        "verify passed a {file} that links outside the backup\n{}",
        run.describe()
    );
    assert!(
        run.records
            .iter()
            .any(|record| record.key.as_deref() == Some(file)
                && record.error.contains("symbolic link")),
        "verify didn't refuse the linked {file}\n{}",
        run.describe()
    );
}

/// a symlinked `pruned.jsonl` is refused without hashing the file it points at, even when the
/// manifest pins that file's sha256
#[tokio::test]
async fn a_symlinked_pruned_file_is_refused_without_reading_it() {
    // link pruned.jsonl to the secret file and pin the secret's sha256 in the manifest
    let case = Case::new("pruned-link").await;
    std::os::unix::fs::symlink(&case.secret, case.backup.join(PRUNED_FILE)).unwrap();
    let mut manifest = case.manifest();
    manifest["pruned"] = serde_json::json!({ "count": 1, "sha256": case.secret_sha256 });
    case.write_manifest(&manifest);
    let run = case.verify();
    // verify refuses the link instead of vouching for the secret file
    refused_link(&run, PRUNED_FILE);
}

/// a symlinked `failures.jsonl` is refused, even when what it points at matches the manifest
#[tokio::test]
async fn a_symlinked_failures_file_is_refused() {
    // move failures.jsonl out of the backup, unchanged, and link to it
    let case = Case::new("failures-link").await;
    let outside = case.dir.join("elsewhere/failures.jsonl");
    replace_with_link(&case.backup.join(FAILURES_FILE), &outside);
    let run = case.verify();
    // verify refuses the link instead of reading the file it points at
    refused_link(&run, FAILURES_FILE);
}

/// a `maps/` directory that can't be read, below a table directory that can, is recorded by
/// verify and by restore's map walk, instead of stopping either run
#[tokio::test]
async fn an_unreadable_maps_directory_is_recorded() {
    // take every permission away from the associations maps directory
    let case = Case::new("maps-unreadable").await;
    let Some(_locked) = Unreadable::new(&case.backup.join("associations/maps")) else {
        eprintln!("skipped: permissions don't stop this user from reading");
        return;
    };
    // restore's walk sends nothing from it and records the directory it couldn't list
    let maps = case.backup.join("associations/maps");
    let walk = restore_map_walk(&case, "associations").await;
    assert!(
        walk.sent.is_empty()
            && walk.records.len() == 1
            && walk.records[0].key.as_deref() == Some(&*maps.to_string_lossy()),
        "restore's walk should record the unreadable maps: sent {:?}, recorded {:?}",
        walk.sent,
        walk.records
    );
    // verify finishes with failures rather than aborting, and names the directory
    let run = case.verify();
    assert_eq!(
        run.output.status.code(),
        Some(2),
        "verify should finish with failures\n{}",
        run.describe()
    );
    assert!(
        run.records
            .iter()
            .any(|record| record.key.as_deref() == Some("associations/maps")
                && record.error.contains("can't be read")),
        "verify didn't record the unreadable associations/maps\n{}",
        run.describe()
    );
}

/// a `data/` directory that can't be read, below a table directory that can, is recorded by
/// verify instead of stopping the run
#[tokio::test]
async fn an_unreadable_data_directory_is_recorded() {
    // take every permission away from the associations data directory
    let case = Case::new("data-unreadable").await;
    let Some(_locked) = Unreadable::new(&case.backup.join("associations/data")) else {
        eprintln!("skipped: permissions don't stop this user from reading");
        return;
    };
    let run = case.verify();
    // verify finishes with failures rather than aborting, and names the directory
    assert_eq!(
        run.output.status.code(),
        Some(2),
        "verify should finish with failures\n{}",
        run.describe()
    );
    assert!(
        run.records
            .iter()
            .any(|record| record.key.as_deref() == Some("associations/data")
                && record.error.contains("can't be read")),
        "verify didn't record the unreadable associations/data\n{}",
        run.describe()
    );
}

/// a symlinked `redis.json` is refused, even when what it points at matches the manifest
#[tokio::test]
async fn a_symlinked_redis_file_is_refused() {
    // move redis.json out of the backup, unchanged, and link to it
    let case = Case::new("redis-link").await;
    let outside = case.dir.join("elsewhere/redis.json");
    replace_with_link(&case.backup.join("redis.json"), &outside);
    let run = case.verify();
    // verify refuses the link instead of reading the Redis state it points at
    refused_link(&run, "redis.json");
}

/// a symlinked `schema.json` sibling is refused, even when the manifest pins the sha256 of the
/// file it points at
#[tokio::test]
async fn a_symlinked_sibling_is_refused() {
    // link schema.json to the secret file and list it with the secret's length and sha256
    let case = Case::new("sibling-link").await;
    std::os::unix::fs::symlink(&case.secret, case.backup.join("schema.json")).unwrap();
    let mut manifest = case.manifest();
    manifest["siblings"] = serde_json::json!([{
        "name": "schema.json",
        "len": SECRET.len(),
        "sha256": case.secret_sha256,
    }]);
    case.write_manifest(&manifest);
    let run = case.verify();
    // verify refuses the link instead of vouching for the secret file
    refused_link(&run, "schema.json");
}

/// a symlinked S3 index is refused, even when what it points at matches the manifest
#[tokio::test]
async fn a_symlinked_index_is_refused() {
    // move the graphics index out of the backup, unchanged, and link to it
    let case = Case::new("index-link").await;
    let outside = case.dir.join("elsewhere/objects.index.jsonl");
    replace_with_link(&case.backup.join("graphics/objects.index.jsonl"), &outside);
    let run = case.verify();
    // verify refuses the link instead of checking objects against the index it points at
    refused_link(&run, "graphics/objects.index.jsonl");
}

/// a symlinked archive map file is refused by verify and by restore's map walk, since restore
/// would load the map it points at
#[tokio::test]
async fn a_symlinked_map_file_is_refused() {
    // move one archive map out of the backup, unchanged, and link to it
    let case = Case::new("map-link").await;
    let maps = case.backup.join("associations/maps");
    let map = std::fs::read_dir(&maps)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let name = map.file_name().unwrap().to_string_lossy().into_owned();
    let outside = case.dir.join("elsewhere").join(&name);
    replace_with_link(&map, &outside);
    // restore's map walk records the link and sends only the other maps
    let walk = restore_map_walk(&case, "associations").await;
    assert!(
        !walk.sent.is_empty()
            && !walk.sent.contains(&map)
            && walk.records.len() == 1
            && walk.records[0].key.as_deref() == Some(&*map.to_string_lossy()),
        "restore's map walk should record the linked map: sent {:?}, recorded {:?}",
        walk.sent,
        walk.records
    );
    // verify refuses the link instead of checking the map it points at
    let run = case.verify();
    refused_link(&run, &format!("associations/maps/{name}"));
}
