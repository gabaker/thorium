//! Resource kinds and the per-kind checks of a round trip
//!
//! A round trip records every difference the oracle finds, every object the runs skipped or
//! failed, and the components two backups counted differently. Each test checks one resource
//! kind's share of those, so a kind that doesn't restore yet is ignored on its own.

use super::cycle::{
    backup, census, component_entries, describe, failure_records, record_entries, restore, wipe,
};
use super::harness::Harness;
use super::oracle::{Difference, Snapshot};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Output;
use thoradm::backup::coverage::{Coverage, Registry, Store};
use thoradm::backup::failures::{FAILURES_FILE, FailureRecord, Operation, read_records};
use thoradm::backup::manifest::Manifest;

/// The registry entries each resource kind covers, named as differences name them
pub const KINDS: &[(&str, &[&str])] = &[
    ("samples", &["samples_list", "samples"]),
    ("s3_ids", &["s3_ids", "s3_sha256s"]),
    ("comments", &["comments", "comments_by_id"]),
    ("tags", &["tags", "tags_by_item", "tags_case_insensitive"]),
    ("repos", &["repo_data", "repos_list", "repos"]),
    (
        "commitishes",
        &["commitishes", "commitish_list", "committed_repo_data"],
    ),
    (
        "network_policies",
        &[
            "network_policies",
            "network_policies_by_name",
            "network_policies_default",
        ],
    ),
    ("results", &["results"]),
    (
        "results_stream",
        &["results_stream", "results_auth", "results_ids"],
    ),
    ("nodes", &["nodes"]),
    (
        "entities",
        &["entities", "entities_by_id", "entities_by_name"],
    ),
    ("associations", &["associations"]),
    ("notifications", &["notifications"]),
    ("objects", &["s3:files", "s3:repos"]),
    ("attachments", &["s3:attachments", "s3:results"]),
    (
        "users",
        &[
            "users",
            "user_data:*",
            "user_groups:*",
            "users_token_map",
            "users_email_map",
            "oauth:alias_to_user:*",
            "analysts",
            "scoped_token_map",
            "scoped_tokens:*",
        ],
    ),
    (
        "groups",
        &[
            "groups",
            "groups_data:*",
            "groups_direct:*",
            "groups_metagroups:*",
            "groups_combined:*",
            "netpols:*",
            "netpol_used_by:*",
        ],
    ),
    (
        "images_and_pipelines",
        &[
            "images:*",
            "image_data:*",
            "image_used_by:*",
            "pipelines:*",
            "pipeline_data:*",
        ],
    ),
    ("settings", &["system_settings"]),
    ("graphics", &["s3:graphics"]),
    ("census", &["census:*"]),
    (
        "excluded",
        &["logs", "events", "s3:ephemeral", "s3:reaction_cache"],
    ),
];

/// A column a resource kind is known to lose in a round trip, with the finding that explains it
#[derive(Debug)]
pub struct KnownLoss {
    /// The resource kind
    pub kind: &'static str,
    /// The table the column is in
    pub table: &'static str,
    /// The lost column
    pub column: &'static str,
    /// The title of the finding that explains the loss
    pub finding: &'static str,
}

/// Every column a kind's check skips, so the rest of the kind's tables are still compared
pub const KNOWN_LOSSES: &[KnownLoss] = &[];

/// A record a backup of the fixture makes, with the finding that explains it
#[derive(Debug)]
pub struct ExpectedRecord {
    /// The backup that makes it, or `None` for every backup
    pub backup: Option<&'static str>,
    /// The component that records it
    pub component: &'static str,
    /// The operation that fails
    pub operation: Operation,
    /// The title of the finding that explains it
    pub finding: &'static str,
}

/// The records a backup of the fixture is known to make; any other record is a loss
pub const EXPECTED_BACKUP_RECORDS: &[ExpectedRecord] = &[];

/// What a round trip found
#[derive(Debug, Default)]
pub struct Outcome {
    /// The differences the oracle found after each restore, by cycle
    pub cycles: Vec<(&'static str, Vec<Difference>)>,
    /// Every failed or skipped object the backups and restores recorded, with its cycle
    pub records: Vec<(&'static str, FailureRecord)>,
    /// Every failed or skipped object the backups alone recorded, with the backup's name
    pub backup_records: Vec<(String, FailureRecord)>,
    /// The components whose counts two backups of the same data disagree on, with both counts
    pub counts: Vec<(String, String, String)>,
    /// Problems with the runs themselves: runs that stopped and a census that failed
    pub runs: Vec<String>,
    /// Group members in the seeded source that aren't users, which the API never allows
    pub non_user_members: Vec<String>,
}

/// Runs a round trip once, keeping a panic as its failure so later tests report it instead of
/// running the round trip again
///
/// # Arguments
///
/// * `run` - The round trip
pub async fn run_once<F>(run: F) -> Result<Outcome, String>
where
    F: std::future::Future<Output = Outcome> + Send + 'static,
{
    // a spawned task turns a panic into an error
    tokio::spawn(run)
        .await
        .map_err(|err| match err.try_into_panic() {
            Ok(panic) => panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
                .unwrap_or_else(|| "the round trip panicked".to_owned()),
            Err(err) => err.to_string(),
        })
}

/// Reads the per-component counts a run logs when each component finishes
///
/// # Arguments
///
/// * `output` - The run's output
#[must_use]
pub fn component_counts(output: &Output) -> BTreeMap<String, String> {
    // each summary line reads `<component>: N handled, M skipped, K failed`
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter_map(|line| {
            let start = line.find(|c: char| c.is_ascii_lowercase())?;
            let (component, counts) = line[start..].split_once(": ")?;
            counts.contains(" handled, ").then(|| {
                let counts = counts.split(" component=").next().unwrap_or(counts);
                (component.to_owned(), counts.to_owned())
            })
        })
        .collect()
}

impl Outcome {
    /// Records the components two backups counted differently
    ///
    /// # Arguments
    ///
    /// * `first` - The first backup's output
    /// * `second` - The second backup's output
    pub fn compare_counts(&mut self, first: &Output, second: &Output) {
        // every component either run counted
        let (first, second) = (component_counts(first), component_counts(second));
        let components: BTreeSet<&String> = first.keys().chain(second.keys()).collect();
        for component in components {
            if first.get(component) != second.get(component) {
                self.counts.push((
                    component.clone(),
                    first.get(component).cloned().unwrap_or_default(),
                    second.get(component).cloned().unwrap_or_default(),
                ));
            }
        }
    }

    /// Records every component whose manifest entries differ between two backups of the same
    /// data: a table's rows in each token range, or an S3 component's object count and bytes
    ///
    /// Times, archive names, and the run's own paths differ between any two backups, so only
    /// counts are compared.
    ///
    /// # Arguments
    ///
    /// * `first` - The first backup's directory
    /// * `second` - The second backup's directory
    pub async fn compare_manifests(&mut self, first: &Path, second: &Path) {
        // a backup without a manifest stopped early, which the runs already report
        let (Ok(Some(first)), Ok(Some(second))) =
            (Manifest::read(first).await, Manifest::read(second).await)
        else {
            self.runs
                .push("a backup of the round trip has no readable manifest".to_owned());
            return;
        };
        // each table's rows per range, attributed to the component that backs it up
        let tables: BTreeSet<&String> = first.tables.keys().chain(second.tables.keys()).collect();
        for table in tables {
            let rows = |manifest: &Manifest| {
                manifest.tables.get(table).map(|entry| {
                    let ranges: Vec<u64> = entry.ranges.iter().map(|range| range.rows).collect();
                    format!("{} rows in ranges {ranges:?}", entry.rows)
                })
            };
            let (in_first, in_second) = (rows(&first), rows(&second));
            if in_first != in_second {
                let component = Registry::classify(Store::ScyllaTable, table)
                    .and_then(|entry| match entry.coverage {
                        Coverage::BackedUp(component) => Some(component.to_string()),
                        Coverage::Excluded { .. } => None,
                    })
                    .unwrap_or_else(|| table.clone());
                self.counts.push((
                    component,
                    in_first.unwrap_or_default(),
                    in_second.unwrap_or_default(),
                ));
            }
        }
        // each S3 component's objects and bytes
        let components: BTreeSet<&String> = first.s3.keys().chain(second.s3.keys()).collect();
        for component in components {
            let objects = |manifest: &Manifest| {
                manifest
                    .s3
                    .get(component)
                    .map(|entry| format!("{} objects, {} bytes", entry.objects, entry.bytes))
            };
            let (in_first, in_second) = (objects(&first), objects(&second));
            if in_first != in_second {
                self.counts.push((
                    component.clone(),
                    in_first.unwrap_or_default(),
                    in_second.unwrap_or_default(),
                ));
            }
        }
    }

    /// Runs a census, recording a failure
    ///
    /// # Arguments
    ///
    /// * `harness` - The harness naming the namespace
    /// * `when` - When the census runs, for the failure message
    pub fn census(&mut self, harness: &Harness, when: &str) {
        // rebuild every census kind thoradm counts
        let counted = census(harness);
        if !counted.status.success() {
            self.runs
                .push(format!("census {when}: {}", describe(&counted)));
        }
    }

    /// Backs up the namespace, recording a run that stopped
    ///
    /// # Arguments
    ///
    /// * `harness` - The harness naming the namespace
    /// * `dir` - Where to write the backup
    /// * `name` - The cycle's name
    pub fn backup(&mut self, harness: &Harness, dir: &Path, name: &str) -> Output {
        // a run that finished with recorded objects exits 2, and the records are checked per kind
        let backed_up = backup(harness, dir);
        if !matches!(backed_up.status.code(), Some(0 | 2)) {
            self.runs
                .push(format!("backup {name}: {}", describe(&backed_up)));
        }
        // keep the backup's own records apart from the restores' to check them on their own
        let records = read_records(&dir.join(FAILURES_FILE)).unwrap_or_default();
        self.backup_records
            .extend(records.into_iter().map(|record| (name.to_owned(), record)));
        backed_up
    }

    /// Wipes the namespace, restores a backup, and compares the result with the source snapshot
    ///
    /// # Arguments
    ///
    /// * `harness` - The harness naming the namespace
    /// * `dir` - The backup to restore
    /// * `name` - The cycle's name
    /// * `source` - The snapshot the restore should reproduce
    pub async fn restore(
        &mut self,
        harness: &Harness,
        dir: &Path,
        name: &'static str,
        source: &Snapshot,
    ) {
        // wipe and restore
        wipe(harness).await;
        let restored = restore(harness, dir);
        if !matches!(restored.status.code(), Some(0 | 2)) {
            self.runs
                .push(format!("restore {name}: {}", describe(&restored)));
        }
        self.records.extend(
            failure_records(dir)
                .into_iter()
                .map(|record| (name, record)),
        );
        // compare with the source
        let found = Snapshot::take(harness).await;
        self.cycles.push((
            name,
            super::oracle::Oracle::default().compare(source, &found),
        ));
    }

    /// Checks that one resource kind came back unchanged in every cycle
    ///
    /// # Arguments
    ///
    /// * `kind` - The resource kind
    ///
    /// # Panics
    ///
    /// Panics, listing what differs, if any run stopped or the kind lost or changed anything.
    pub fn assert_kind(&self, kind: &str) {
        // no run stopped
        assert!(
            self.runs.is_empty(),
            "the round trip's runs failed:\n{}",
            self.runs.join("\n")
        );
        let entries = KINDS
            .iter()
            .find(|(name, _)| *name == kind)
            .map_or_else(|| panic!("no kind {kind}"), |(_, entries)| *entries);
        let mine = |names: &[String]| names.iter().any(|entry| entries.contains(&entry.as_str()));
        // no object of the kind was skipped or failed
        let recorded: Vec<String> = self
            .records
            .iter()
            .filter(|(_, record)| mine(&record_entries(record)))
            .map(|(cycle, record)| {
                format!(
                    "{cycle}: {} {:?} {:?}: {}",
                    record.component, record.table, record.key, record.error
                )
            })
            .collect();
        assert!(
            recorded.is_empty(),
            "{} objects of {kind} were recorded:\n{}",
            recorded.len(),
            recorded.join("\n")
        );
        // both backups counted the kind's objects alike
        let counted: Vec<String> = self
            .counts
            .iter()
            .filter(|(component, ..)| mine(&component_entries(component)))
            .map(|(component, first, second)| {
                format!("{component}: first {first}, second {second}")
            })
            .collect();
        assert!(
            counted.is_empty(),
            "the backups counted {kind} differently:\n{}",
            counted.join("\n")
        );
        // the oracle found nothing in the kind's entries but the losses findings explain
        let known = |difference: &Difference| {
            KNOWN_LOSSES.iter().any(|loss| {
                loss.kind == kind
                    && difference.entry == loss.table
                    && difference
                        .location
                        .starts_with(&format!("{} [", loss.table))
                    && difference.location.ends_with(&format!("] {}", loss.column))
            })
        };
        for (cycle, differences) in &self.cycles {
            let found: Vec<String> = differences
                .iter()
                .filter(|difference| entries.contains(&difference.entry.as_str()))
                .filter(|difference| !known(difference))
                .map(ToString::to_string)
                .collect();
            let shown: Vec<&str> = found.iter().take(50).map(String::as_str).collect();
            assert!(
                found.is_empty(),
                "{} differences in {kind} after the {cycle} cycle:\n{}",
                found.len(),
                shown.join("\n")
            );
        }
    }

    /// Returns every record a backup made that `EXPECTED_BACKUP_RECORDS` doesn't explain,
    /// whichever kind it belongs to
    #[must_use]
    pub fn unexpected_backup_records(&self) -> Vec<String> {
        // a record of an ignored kind is still a loss the backup made
        self.backup_records
            .iter()
            .filter(|(backup, record)| {
                !EXPECTED_BACKUP_RECORDS.iter().any(|expected| {
                    expected.backup.is_none_or(|name| name == backup)
                        && record.component == expected.component
                        && record.operation == expected.operation
                })
            })
            .map(|(backup, record)| {
                format!(
                    "{backup}: {:?} {} {:?} {:?} {:?}: {}",
                    record.kind,
                    record.component,
                    record.operation,
                    record.table,
                    record.key,
                    record.error
                )
            })
            .collect()
    }

    /// Records every group member a source snapshot holds that isn't a user
    ///
    /// # Arguments
    ///
    /// * `source` - The snapshot of the seeded source
    pub fn check_members(&mut self, source: &Snapshot) {
        // every member of every role list must have a user record
        for (key, entry) in &source.redis {
            let is_members =
                key.starts_with("groups_combined:") || key.starts_with("groups_direct:");
            let super::oracle::RedisValueRepr::Set(members) = &entry.value else {
                continue;
            };
            if !is_members {
                continue;
            }
            for member in members {
                if !source.redis.contains_key(&format!("user_data:{member}")) {
                    self.non_user_members.push(format!("{key}: {member}"));
                }
            }
        }
    }

    /// Returns every difference that belongs to no kind
    #[must_use]
    pub fn orphans(&self) -> Vec<String> {
        // differences outside every kind would go unchecked
        self.cycles
            .iter()
            .flat_map(|(_, differences)| differences)
            .filter(|difference| {
                !KINDS
                    .iter()
                    .any(|(_, entries)| entries.contains(&difference.entry.as_str()))
            })
            .map(ToString::to_string)
            .collect()
    }
}
