//! The committed golden backups, one per archive format, restore on this build
//!
//! Each golden under `tests/fixtures/golden/<name>/` holds `backup/`, a backup of the golden
//! fixture written by the thoradm its `PROVENANCE` names, and `expected/snapshot.json`, the
//! oracle's snapshot of the namespace it was taken from. `generate.sh` beside them writes one.
//!
//! The tests restore, so each holds the binary's restore guard. A golden's Redis state replaces
//! the harness admin's login, so each restoring test writes that login back afterwards.

mod common;

use common::classes::FORMAT_1_REQUIRED;
use common::corrupt::flip_byte;
use common::cycle::{
    admin_login, census, describe, failure_records, put_admin_login, restore_with, wipe,
};
use common::fixture::{self, FixtureCases};
use common::harness::Harness;
use common::oracle::{Difference, ORACLE_RULES, Oracle, Snapshot};
use scylla::client::session::Session;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use thoradm::backup::failures::Operation;
use thoradm::backup::manifest::FORMAT_VERSION;
use tokio::sync::OnceCell;

/// The harness admin's login, read before any golden's Redis state replaces it
static ADMIN_LOGIN: OnceCell<BTreeMap<String, String>> = OnceCell::const_new();

/// What a golden's backup doesn't hold because the thoradm that wrote it didn't back it up, so a
/// restore of it lacks what its source held there
struct GoldenLoss {
    /// The golden
    golden: &'static str,
    /// The registry entry the lost data belongs to, as the oracle names it
    entry: &'static str,
    /// Text every lost location holds, or nothing for the whole entry
    holds: &'static str,
    /// Why the golden's thoradm didn't back it up
    reason: &'static str,
}

/// Every loss a golden's restore is known to have against its source
const GOLDEN_LOSSES: &[GoldenLoss] = &[
    GoldenLoss {
        golden: "legacy",
        entry: "associations",
        holds: "Text(\"Bidirectional\")",
        reason: "its thoradm skipped bidirectional associations when reading them back",
    },
    GoldenLoss {
        golden: "legacy",
        entry: "associations",
        holds: "census:associations:stream:",
        reason: "its thoradm skipped bidirectional associations and their census streams",
    },
    GoldenLoss {
        golden: "legacy",
        entry: "netpols:*",
        holds: "",
        reason: "the API's Redis backup held no network-policy sets before its thoradm",
    },
    GoldenLoss {
        golden: "legacy",
        entry: "scoped_tokens:*",
        holds: "",
        reason: "the API's Redis backup held no scoped tokens before its thoradm",
    },
    GoldenLoss {
        golden: "legacy",
        entry: "scoped_token_map",
        holds: "",
        reason: "the API's Redis backup held no scoped tokens before its thoradm",
    },
    GoldenLoss {
        golden: "format-1",
        entry: "s3:graphics",
        holds: "",
        reason: "its thoradm had no graphics component",
    },
    GoldenLoss {
        golden: "format-1",
        entry: "s3:results",
        holds: "/__thorium_entities/",
        reason: "its thoradm didn't back up result entity blobs",
    },
];

/// The views the goldens' source namespaces held that the API's setup no longer creates, so a
/// restore target holds them only when an older release created them there
const UNCREATED_VIEWS: &[&str] = &["entities_by_name"];

/// Returns the harness admin's login, reading it the first time, before any golden is restored
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
async fn harness_login(harness: &Harness) -> &'static BTreeMap<String, String> {
    // every restoring test asks before it restores, so the first read is the harness's own login
    ADMIN_LOGIN
        .get_or_init(|| async { admin_login(harness).await })
        .await
}

/// Returns every golden by name, oldest first: `legacy`, then one per format version through
/// today's
fn golden_names() -> Vec<String> {
    // the legacy layout predates format versions
    std::iter::once("legacy".to_owned())
        .chain((1..=FORMAT_VERSION).map(|version| format!("format-{version}")))
        .collect()
}

/// Returns the format version a golden's backup was written in; a legacy backup's is 1
///
/// # Arguments
///
/// * `name` - The golden
fn golden_format(name: &str) -> u32 {
    // the name carries the version
    name.strip_prefix("format-")
        .and_then(|version| version.parse().ok())
        .unwrap_or(1)
}

/// Returns whether a difference is a lost location of a listed loss
///
/// # Arguments
///
/// * `loss` - The listed loss
/// * `difference` - The difference
fn is_loss(loss: &GoldenLoss, difference: &Difference) -> bool {
    // a location of the loss's entry that the restore lacks
    difference.entry == loss.entry
        && difference.location.contains(loss.holds)
        && difference.found.is_none()
}

/// Returns the directory holding the committed goldens
fn golden_root() -> PathBuf {
    // beside this test, in the crate's tests directory
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/golden")
}

/// Reads a required environment variable of the generator
///
/// # Arguments
///
/// * `name` - The variable
fn required(name: &str) -> String {
    // the generator can't guess what it is asked to write
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set; see generate.sh"))
}

/// Builds a command running a given thoradm binary against the harness's namespace, with the
/// harness's configs and none of the environment's overrides
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `binary` - The thoradm binary
fn thoradm_at(harness: &Harness, binary: &Path) -> Command {
    // the configs alone decide where it connects
    let mut command = Command::new(binary);
    for (key, _) in std::env::vars() {
        if key.starts_with("THORIUM__") || key.starts_with("THORCTL__") {
            command.env_remove(key);
        }
    }
    command
        .arg("--cluster-conf")
        .arg(&harness.cluster_conf)
        .arg("--ctl-conf")
        .arg(&harness.ctl_conf);
    command
}

/// Backs up the namespace with a given binary into a directory, the way its admins ran it: a
/// legacy build copies attachments and result files only on a second run into the directory
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `binary` - The thoradm binary
/// * `legacy` - Whether the binary predates manifests
/// * `dir` - Where to write the backup
fn backup_with(harness: &Harness, binary: &Path, legacy: bool, dir: &Path) -> Vec<Output> {
    // every component, then the object components again for a legacy build
    let runs: &[&str] = if legacy {
        &["all", "comment-attachments,result-files"]
    } else {
        &["all"]
    };
    runs.iter()
        .map(|components| {
            thoradm_at(harness, binary)
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
                .output()
                .expect("failed to run the golden's thoradm")
        })
        .collect()
}

/// Writes one golden: seeds the golden fixture, backs it up with the binary the environment
/// names, and saves the namespace's snapshot and the golden's provenance
///
/// `THORADM_GOLDEN_NAME` names the golden (`legacy`, `format-1`, or `format-2`),
/// `THORADM_GOLDEN_BINARY` the thoradm that writes it, `THORADM_GOLDEN_COMMIT` the commit that
/// binary was built from, and `THORADM_GOLDEN_TOOLCHAIN` and `THORADM_GOLDEN_HARNESS` the toolchain
/// and the harness's commit, for its provenance.
#[tokio::test]
#[ignore = "writes a committed golden; run by tests/fixtures/golden/generate.sh"]
async fn generate_golden() {
    let name = required("THORADM_GOLDEN_NAME");
    let binary = PathBuf::from(required("THORADM_GOLDEN_BINARY"));
    let commit = required("THORADM_GOLDEN_COMMIT");
    let toolchain = required("THORADM_GOLDEN_TOOLCHAIN");
    let harness_commit = required("THORADM_GOLDEN_HARNESS");
    let legacy = name == "legacy";
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    // the golden fixture: no size cases, and the unset cells format 1 can't hold only in format 2
    let cases = FixtureCases {
        sizes: false,
        format_1_unset: name == "format-2",
    };
    let _ = Box::pin(fixture::value_classes_with(harness, cases)).await;
    let counted = census(harness);
    assert_eq!(counted.status.code(), Some(0), "{}", describe(&counted));
    let snapshot = Snapshot::take(harness).await;
    // a fresh golden directory with the backup, the snapshot, and the provenance
    let root = golden_root().join(&name);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("expected")).unwrap();
    let dir = root.join("backup");
    let runs = backup_with(harness, &binary, legacy, &dir);
    for run in &runs {
        assert_eq!(run.status.code(), Some(0), "{}", describe(run));
    }
    assert_eq!(
        dir.join("manifest.json").exists(),
        !legacy,
        "a legacy golden has no manifest and the others have one"
    );
    snapshot.save(&root.join("expected/snapshot.json"));
    let fixture = if cases.format_1_unset {
        "the value-class fixture without its size cases (large and near-limit values, collections \
         of many elements, objects at the part size or uploaded in parts, near-limit keys)"
    } else {
        "the value-class fixture without its size cases (large and near-limit values, collections \
         of many elements, objects at the part size or uploaded in parts, near-limit keys) and \
         without the unset cells of the columns format 1 can't hold unset"
    };
    let commands = runs
        .iter()
        .zip(if legacy {
            vec!["all", "comment-attachments,result-files"]
        } else {
            vec!["all"]
        })
        .map(|(run, components)| {
            format!(
                "thoradm --workers 4 backup new {components} -m 4 -o backup (exit {:?})",
                run.status.code()
            )
        })
        .collect::<Vec<_>>()
        .join("\n  ");
    let provenance = format!(
        "golden: {name}\n\
         written by: thoradm built from {commit}\n\
         toolchain: {toolchain}\n\
         harness: thoradm/tests/golden.rs::generate_golden at {harness_commit}\n\
         fixture: {fixture}; its values are generated afresh on each run (no seed), and \
         expected/snapshot.json records the ones this backup holds\n\
         commands:\n  {commands}\n\
         namespace: {}\n\
         secret_key (throwaway): {}\n",
        harness.conf.thorium.namespace, harness.conf.thorium.secret_key
    );
    std::fs::write(root.join("PROVENANCE"), provenance).unwrap();
}

/// Copies a directory tree
///
/// # Arguments
///
/// * `from` - The directory to copy
/// * `to` - Where to put the copy
fn copy_tree(from: &Path, to: &Path) {
    // recreate every directory and copy every file
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().filter_map(Result::ok) {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// Copies a golden's backup to a scratch directory, so a restore's reports stay out of the
/// committed files
///
/// # Arguments
///
/// * `harness` - The harness owning the scratch space
/// * `name` - The golden
/// * `scratch` - The scratch directory's name
fn golden_copy(harness: &Harness, name: &str, scratch: &str) -> PathBuf {
    // a fresh copy of the committed backup
    let dir = harness.scratch(scratch);
    let _ = std::fs::remove_dir_all(&dir);
    copy_tree(&golden_root().join(name).join("backup"), &dir);
    dir
}

/// Returns the problems with a restore that must be refused by one preflight check before it
/// writes anything: its exit code, its record, the text the refusal names, and any change
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `dir` - The backup
/// * `flags` - The restore's flags
/// * `check` - The preflight check that must refuse it
/// * `named` - Text the refusal must name
async fn refusal_problems(
    harness: &Harness,
    dir: &Path,
    flags: &[&str],
    check: &str,
    named: &str,
) -> Vec<String> {
    // the namespace before and after, without the TTLs that count down between them
    let mut before = Snapshot::take(harness).await;
    let refused = restore_with(harness, dir, flags);
    let mut after = Snapshot::take(harness).await;
    before.ttls.clear();
    after.ttls.clear();
    let changed = Oracle {
        rules: &[],
        format: None,
    }
    .compare(&before, &after);
    let records = failure_records(dir);
    let mut problems = Vec::new();
    if refused.status.code() != Some(1) {
        problems.push(format!(
            "exit {:?}: {}",
            refused.status.code(),
            describe(&refused)
        ));
    }
    if !records.iter().any(|record| {
        record.operation == Operation::Preflight && record.object.as_deref() == Some(check)
    }) {
        problems.push(format!("no preflight record of the {check} check"));
    }
    if !records.iter().any(|record| record.error.contains(named)) {
        problems.push(format!("no record names {named}"));
    }
    if !changed.is_empty() {
        let shown: Vec<String> = changed.iter().take(5).map(ToString::to_string).collect();
        problems.push(format!("{} changes:\n{}", changed.len(), shown.join("\n")));
    }
    problems
}

/// Returns the first data file of a backup's table
///
/// # Arguments
///
/// * `dir` - The backup
/// * `table` - The table
fn first_data_file(dir: &Path, table: &str) -> PathBuf {
    // the archives' data files, in a stable order
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir.join(table).join("data"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    files.sort();
    files.into_iter().next().expect("the table has an archive")
}

/// Reads the remaining TTL of every notification's message
///
/// # Arguments
///
/// * `scylla` - The Scylla session
/// * `ns` - The keyspace
async fn notification_ttls(scylla: &Session, ns: &str) -> Vec<Option<i32>> {
    // every notification's TTL
    scylla
        .query_unpaged(format!("SELECT TTL(msg) FROM {ns}.notifications"), ())
        .await
        .expect("failed to read notification TTLs")
        .into_rows_result()
        .unwrap()
        .rows::<(Option<i32>,)>()
        .unwrap()
        .map(|row| row.unwrap().0)
        .collect()
}

/// Every format version through today's has a committed golden, and each golden holds its
/// backup, its expected snapshot, and its provenance
#[test]
fn every_format_version_has_a_golden_with_provenance() {
    // a new format version lands with its golden
    let mut problems = Vec::new();
    for name in golden_names() {
        let root = golden_root().join(&name);
        for part in ["backup", "expected/snapshot.json", "PROVENANCE"] {
            if !root.join(part).exists() {
                problems.push(format!("the {name} golden has no {part}"));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Restores one golden into the emptied namespace and returns how the restored namespace differs
/// from the golden's expected snapshot, minus the golden's listed losses
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `name` - The golden
/// * `login` - The harness admin's login, written back after the restore
async fn golden_differences(
    harness: &Harness,
    name: &str,
    login: &BTreeMap<String, String>,
) -> (Snapshot, Vec<String>) {
    // an empty namespace, and the golden restored into it; a legacy backup needs --legacy
    wipe(harness).await;
    put_admin_login(harness, login).await;
    let dir = golden_copy(harness, name, &format!("{name}-restored"));
    let flags: &[&str] = if name == "legacy" { &["--legacy"] } else { &[] };
    let restored = restore_with(harness, &dir, flags);
    let found = Snapshot::take(harness).await;
    put_admin_login(harness, login).await;
    let mut problems = Vec::new();
    if restored.status.code() != Some(0) {
        problems.push(format!("{name}: the restore: {}", describe(&restored)));
    }
    // every column compared with what the golden's source held, minus the format's losses
    let mut expected = Snapshot::load(&golden_root().join(name).join("expected/snapshot.json"));
    // a view the golden's source held that the API's setup no longer creates is compared only
    // when the restore target holds it
    for view in UNCREATED_VIEWS {
        if !found.scylla.contains_key(*view) {
            expected.scylla.remove(*view);
        }
    }
    let oracle = Oracle {
        rules: ORACLE_RULES,
        format: Some(golden_format(name)),
    };
    let differences = oracle.compare(&expected, &found);
    let losses: Vec<&GoldenLoss> = GOLDEN_LOSSES
        .iter()
        .filter(|loss| loss.golden == name)
        .collect();
    problems.extend(
        differences
            .iter()
            .filter(|difference| !losses.iter().any(|loss| is_loss(loss, difference)))
            .map(|difference| format!("{name}: {difference}")),
    );
    // a listed loss that no longer shows is stale
    problems.extend(
        losses
            .iter()
            .filter(|loss| {
                !differences
                    .iter()
                    .any(|difference| is_loss(loss, difference))
            })
            .map(|loss| {
                format!(
                    "{name}: the listed loss of {} {} ({}) didn't show",
                    loss.entry, loss.holds, loss.reason
                )
            }),
    );
    (found, problems)
}

/// Returns the cells a snapshot holds unset in the columns format 1 can't hold unset, as
/// `table [key] column`, leaving out the columns an oracle rule covers (restore resets a node's
/// health, so an unset health comes back set)
///
/// # Arguments
///
/// * `snapshot` - The snapshot
fn format_1_required_unset(snapshot: &Snapshot) -> Vec<String> {
    // a cell the snapshot left out of a row is unset
    FORMAT_1_REQUIRED
        .iter()
        .filter(|(table, column)| {
            let location = format!("{table}.{column}");
            !ORACLE_RULES.iter().any(|rule| rule.location == location)
        })
        .flat_map(|(table, column)| {
            snapshot
                .scylla
                .get(*table)
                .into_iter()
                .flatten()
                .filter(|(_, row)| !row.contains_key(*column))
                .map(move |(key, _)| format!("{table} [{key}] {column}"))
        })
        .collect()
}

/// Every committed golden (`legacy` with `--legacy`, then one per format version) restores into
/// an empty namespace equal to its expected snapshot, every column compared, minus the losses its
/// format and its thoradm are known to have; the latest format's golden holds unset cells in the
/// columns format 1 can't hold unset, and they read back without a value
#[tokio::test]
async fn every_golden_restores_into_an_empty_namespace() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let login = harness_login(harness).await;
    let mut problems = Vec::new();
    for name in golden_names() {
        let (found, differences) = golden_differences(harness, &name, login).await;
        problems.extend(differences);
        // the latest golden's unset cells come back unset
        if name == format!("format-{FORMAT_VERSION}") {
            let expected =
                Snapshot::load(&golden_root().join(&name).join("expected/snapshot.json"));
            let unset = format_1_required_unset(&expected);
            if unset.is_empty() {
                problems.push(format!(
                    "{name}: the golden holds no unset cell in the columns format 1 can't hold unset"
                ));
            }
            let restored = format_1_required_unset(&found);
            problems.extend(
                unset
                    .iter()
                    .filter(|cell| !restored.contains(cell))
                    .map(|cell| format!("{name}: {cell} came back with a value")),
            );
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// The `legacy` golden (no manifest) is refused without `--legacy` before any write, naming the
/// flag; a copy with one archive byte flipped is refused by the archive checks before any write;
/// with `--legacy` it restores, warning that its completeness couldn't be checked, and its
/// notifications, which format 1 carries without a TTL, take the table's default TTL with one
/// warning
#[tokio::test]
async fn the_legacy_golden_restores_only_with_legacy() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let login = harness_login(harness).await;
    wipe(harness).await;
    put_admin_login(harness, login).await;
    // without --legacy it is refused, naming the flag
    let dir = golden_copy(harness, "legacy", "legacy-refused");
    let mut problems: Vec<String> = refusal_problems(harness, &dir, &[], "manifest", "--legacy")
        .await
        .into_iter()
        .map(|problem| format!("without --legacy: {problem}"))
        .collect();
    // a flipped archive byte fails the archive checks before anything is written
    let damaged = golden_copy(harness, "legacy", "legacy-damaged");
    let data = first_data_file(&damaged, "samples_list");
    let len = std::fs::metadata(&data).unwrap().len();
    flip_byte(&data, len / 2).unwrap();
    problems.extend(
        refusal_problems(
            harness,
            &damaged,
            &["--legacy"],
            "verify",
            "backup verify found",
        )
        .await
        .into_iter()
        .map(|problem| format!("damaged: {problem}")),
    );
    // with --legacy it restores, warning that completeness couldn't be checked
    let dir = golden_copy(harness, "legacy", "legacy-restored");
    let restored = restore_with(harness, &dir, &["--legacy"]);
    put_admin_login(harness, login).await;
    let stderr = String::from_utf8_lossy(&restored.stderr);
    if restored.status.code() != Some(0) {
        problems.push(format!("the --legacy restore: {}", describe(&restored)));
    }
    if !stderr.contains("its completeness can't be checked") {
        problems.push("the --legacy restore didn't warn that completeness can't be checked".into());
    }
    // its notifications take the table default, with one warning
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    let default = harness.conf.thorium.retention.notifications;
    let ttls = notification_ttls(&scylla, ns).await;
    if ttls.is_empty() {
        problems.push("the legacy golden restored no notifications".into());
    }
    for ttl in &ttls {
        let fresh = ttl.is_some_and(|ttl| i64::from(ttl) + 600 >= i64::try_from(default).unwrap());
        if !fresh {
            problems.push(format!(
                "a notification came back with TTL {ttl:?}, not the table default {default}"
            ));
        }
    }
    let warnings = stderr
        .matches("take the notifications table's default TTL")
        .count();
    if warnings != 1 {
        problems.push(format!("{warnings} default-TTL warnings, not 1:\n{stderr}"));
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// The columns format 2 added to the archives, each with a value to give a restored row before
/// a `--merge` of an older backup, which never held the column
const FORMAT_2_COLUMNS: &[(&str, &str, &str)] = &[
    ("results", "tool_version", r#""9.9""#),
    ("results", "entities", r#"{"Device": 3}"#),
    ("results_stream", "tool_version", r#""9.9""#),
    ("repos_list", "default_checkout", r#""kept-branch""#),
    ("repos_list", "earliest", r#""2020-01-02 03:04:05.000Z""#),
    (
        "associations",
        "submissions_source",
        r#""6c1f4a52-6f53-4c43-9d55-1d5d0c1f0a11""#,
    ),
    (
        "associations",
        "submissions_other",
        r#""6c1f4a52-6f53-4c43-9d55-1d5d0c1f0a12""#,
    ),
];

/// Reads every row of a table as JSON objects
///
/// # Arguments
///
/// * `scylla` - The Scylla session
/// * `ns` - The namespace
/// * `table` - The table
async fn json_rows(scylla: &Session, ns: &str, table: &str) -> Vec<serde_json::Value> {
    // each row as one JSON document
    scylla
        .query_unpaged(format!("SELECT JSON * FROM {ns}.{table}"), ())
        .await
        .unwrap_or_else(|err| panic!("failed to read {table}: {err}"))
        .into_rows_result()
        .unwrap()
        .rows::<(String,)>()
        .unwrap()
        .map(|row| serde_json::from_str(&row.unwrap().0).unwrap())
        .collect()
}

/// A `--merge` of a format-1 or legacy golden leaves the columns format 2 added as the target
/// holds them, since those backups never held a value for them
#[tokio::test]
async fn a_merge_of_an_older_golden_keeps_the_columns_it_never_held() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let login = harness_login(harness).await;
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    let mut problems = Vec::new();
    for (name, flags) in [("format-1", &[][..]), ("legacy", &["--legacy"][..])] {
        // the golden restored into an empty namespace
        wipe(harness).await;
        put_admin_login(harness, login).await;
        let dir = golden_copy(harness, name, &format!("merge-older-{name}"));
        let restored = restore_with(harness, &dir, flags);
        put_admin_login(harness, login).await;
        assert_eq!(
            restored.status.code(),
            Some(0),
            "{name}: {}",
            describe(&restored)
        );
        // one row of each table given a value in every column format 2 added; Scylla can't read
        // back the JSON of the fixture's extreme timestamps, so the first row it takes is used
        for table in ["results", "results_stream", "repos_list", "associations"] {
            let mut updated = false;
            for mut row in json_rows(&scylla, ns, table).await {
                for (_, column, value) in FORMAT_2_COLUMNS.iter().filter(|(of, ..)| *of == table) {
                    row[*column] = serde_json::from_str(value).unwrap();
                }
                let insert = format!("INSERT INTO {ns}.{table} JSON ?");
                if scylla
                    .query_unpaged(insert, (row.to_string(),))
                    .await
                    .is_ok()
                {
                    updated = true;
                    break;
                }
            }
            assert!(updated, "{name}: no {table} row could be updated");
        }
        // the same golden merged over it
        let dir = golden_copy(harness, name, &format!("merge-older-{name}-again"));
        let mut merge_flags = vec!["--merge"];
        merge_flags.extend_from_slice(flags);
        let merged = restore_with(harness, &dir, &merge_flags);
        put_admin_login(harness, login).await;
        assert_eq!(
            merged.status.code(),
            Some(0),
            "{name}: {}",
            describe(&merged)
        );
        // every value given survives the merge
        for (table, column, value) in FORMAT_2_COLUMNS {
            let wanted: serde_json::Value = serde_json::from_str(value).unwrap();
            let kept = json_rows(&scylla, ns, table)
                .await
                .iter()
                .any(|row| row[*column] == wanted);
            if !kept {
                problems.push(format!(
                    "a --merge of {name} wrote over {table}.{column} = {value}"
                ));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
