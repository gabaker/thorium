//! Backups written by older thoradm builds restore with this build
//!
//! Run by hand at the backup compatibility review, with the older binaries named in
//! `THORADM_OLDER_BINARIES` as `label=path` pairs separated by commas (for example
//! `4e405bb=/tmp/thoradm-4e405bb,ae918c9=/tmp/thoradm-ae918c9`). The fixture is seeded once and
//! backed up by this build and by every older binary. Each older backup is compared with this
//! build's backup of the same source: every table's archived rows, every object's bytes, and the
//! Redis state. Each is then restored into the wiped namespace with this build and compared
//! through the oracle with this build's own backup restored the same way, and with the source.
//! Only differences a named finding explains for that build pass. The verdicts are written to
//! `cross-version.md` in the report directory.
//!
//! The older builds the expected differences name are `4e405bb` and `ae918c9`, which write no
//! manifest, are backed up the way their admins had to (an `all` run, then the S3 components
//! again into the same directory), and restore with `--legacy`; and `c224f0c`, `d991b3a`,
//! `84373c7`, and `66d79fe`, which write a manifest, take a single `all` backup, and restore
//! without `--legacy` in the format their manifest records. The Redis backup of `4e405bb`,
//! `ae918c9`, `c224f0c`, and `d991b3a` holds no scoped tokens or network-policy sets, and no
//! build before `56885fc` backs up the graphics bucket or result entity blobs.

mod common;

use common::archives;
use common::classes::{KNOWN_BROKEN, report_dir};
use common::cycle::{backup, census, describe, failure_records, restore_with, wipe};
use common::fixture::{self, FixtureCases};
use common::harness::Harness;
use common::kinds::{KINDS, KNOWN_LOSSES};
use common::oracle::{
    Difference, ORACLE_RULES, Oracle, OracleRule, OracleRuleKind, Snapshot, render, text_of,
};
use data_encoding::HEXLOWER;
use scylla::value::CqlValue;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use thoradm::backup::Utils;
use thoradm::backup::manifest::FORMAT_VERSION;
use thoradm::backup::tables::Versioned;
use thoradm::backup::tables::{
    Association, Comment, Commitish, CommitishList, Entity, NetworkPolicy, Node, Notification,
    Output as ResultOutput, OutputStream, RepoData, RepoList, S3Id, SamplesList, Tag,
};

/// A difference between an older build's backup and this build's that a known finding explains
/// for that build only
struct ExpectedDifference {
    /// The labels of the older builds that make it
    builds: &'static [&'static str],
    /// The comparison it shows up in, as `CompatDifference::place` names it
    place: &'static str,
    /// The start of the difference's rendering, which names the backup that alone holds the row
    side: &'static str,
    /// Text the difference's rendering holds, which picks out the explained rows
    holds: &'static str,
    /// The most differences it explains: the number of such rows the fixture holds, or every one
    /// for a Redis family the build's API never backed up
    at_most: usize,
    /// Whether the difference must name the census stream of a partition that holds a
    /// bidirectional association's row
    in_bidirectional_partition: bool,
    /// The title of the finding that explains it
    finding: &'static str,
}

/// The oracle's rules for comparing two restored namespaces: every rule but those that judge a
/// restored value against the source (a reset health, a set rebuilt from the found data)
static RESTORED_RULES: std::sync::LazyLock<Vec<OracleRule>> = std::sync::LazyLock::new(|| {
    // keep the rules that hold the same way on both sides
    ORACLE_RULES
        .iter()
        .filter(|rule| {
            !matches!(
                rule.kind,
                OracleRuleKind::Reset(_) | OracleRuleKind::Recomputed
            )
        })
        .cloned()
        .collect()
});

/// The older builds whose API predates the scoped tokens and network-policy sets in the Redis backup
const BEFORE_NEWER_REDIS_FIELDS: &[&str] = &["4e405bb", "ae918c9", "c224f0c", "d991b3a"];

/// Why a backup taken by a build in [`BEFORE_NEWER_REDIS_FIELDS`] restores without scoped tokens or
/// the network-policy sets no image's policies make
const NEWER_REDIS_FIELDS: &str =
    "The API's Redis backup held no scoped tokens or network-policy sets before this build";

/// The older builds whose backups hold neither the graphics bucket nor the results bucket's entity
/// blobs
const BEFORE_GRAPHICS: &[&str] = &[
    "4e405bb", "ae918c9", "c224f0c", "d991b3a", "84373c7", "66d79fe",
];

/// Why a backup taken by a build in [`BEFORE_GRAPHICS`] restores without graphics or entity blobs
const NO_GRAPHICS: &str = "The graphics bucket and result entity blobs aren't backed up";

/// Every difference an older build's backup is known to have from this build's
const EXPECTED_DIFFERENCES: &[ExpectedDifference] = &[
    ExpectedDifference {
        // its results objects hold no entity blob, which this build's do
        builds: BEFORE_GRAPHICS,
        place: "results objects",
        side: "only in this build's backup: ",
        holds: "/__thorium_entities/",
        at_most: usize::MAX,
        in_bidirectional_partition: false,
        finding: NO_GRAPHICS,
    },
    ExpectedDifference {
        // nor any graphic, which this build's backup holds
        builds: BEFORE_GRAPHICS,
        place: "graphics objects",
        side: "only in this build's backup: ",
        holds: "",
        at_most: usize::MAX,
        in_bidirectional_partition: false,
        finding: NO_GRAPHICS,
    },
    ExpectedDifference {
        // so its restore lacks the entity blobs, which this build's restore has
        builds: BEFORE_GRAPHICS,
        place: "restored s3:results",
        side: "[s3:results] s3 results/",
        holds: "/__thorium_entities/",
        at_most: usize::MAX,
        in_bidirectional_partition: false,
        finding: NO_GRAPHICS,
    },
    ExpectedDifference {
        // and the source has them
        builds: BEFORE_GRAPHICS,
        place: "source s3:results",
        side: "[s3:results] s3 results/",
        holds: "/__thorium_entities/",
        at_most: usize::MAX,
        in_bidirectional_partition: false,
        finding: NO_GRAPHICS,
    },
    ExpectedDifference {
        // and its restore lacks every graphic, which this build's restore has
        builds: BEFORE_GRAPHICS,
        place: "restored s3:graphics",
        side: "[s3:graphics] s3 graphics/",
        holds: "found <missing>",
        at_most: usize::MAX,
        in_bidirectional_partition: false,
        finding: NO_GRAPHICS,
    },
    ExpectedDifference {
        // and the source has them
        builds: BEFORE_GRAPHICS,
        place: "source s3:graphics",
        side: "[s3:graphics] s3 graphics/",
        holds: "found <missing>",
        at_most: usize::MAX,
        in_bidirectional_partition: false,
        finding: NO_GRAPHICS,
    },
    ExpectedDifference {
        // 4e405bb predates the fix that reads bidirectional associations back, so it skips them
        builds: &["4e405bb"],
        place: "associations archives",
        side: "only in this build's backup: ",
        holds: "direction: Bidirectional",
        // the fixture holds one bidirectional association
        at_most: 1,
        in_bidirectional_partition: false,
        finding: "Bidirectional associations fail to read back from Scylla",
    },
    ExpectedDifference {
        // so its restore lacks the association's rows, which this build's restore has
        builds: &["4e405bb"],
        place: "restored associations",
        side: "[associations] associations [",
        holds: "found <missing>",
        // a bidirectional association is stored as a row from each side
        at_most: 2,
        in_bidirectional_partition: false,
        finding: "Bidirectional associations fail to read back from Scylla",
    },
    ExpectedDifference {
        // and the source has them
        builds: &["4e405bb"],
        place: "source associations",
        side: "[associations] associations [",
        holds: "found <missing>",
        at_most: 2,
        in_bidirectional_partition: false,
        finding: "Bidirectional associations fail to read back from Scylla",
    },
    ExpectedDifference {
        // the association's partition holds no other row, so the census its restore rebuilds
        // has no stream for it, which this build's restore has
        builds: &["4e405bb"],
        place: "restored associations",
        side: "[associations] census census:associations:stream:",
        holds: "found <missing>",
        at_most: 1,
        in_bidirectional_partition: true,
        finding: "Bidirectional associations fail to read back from Scylla",
    },
    ExpectedDifference {
        // and the source has
        builds: &["4e405bb"],
        place: "source associations",
        side: "[associations] census census:associations:stream:",
        holds: "found <missing>",
        at_most: 1,
        in_bidirectional_partition: true,
        finding: "Bidirectional associations fail to read back from Scylla",
    },
    ExpectedDifference {
        // its redis.json holds no network-policy sets, which this build's does
        builds: BEFORE_NEWER_REDIS_FIELDS,
        place: "redis.json",
        side: ".network_policy_sets: the older backup holds nothing,",
        holds: "",
        at_most: 1,
        in_bidirectional_partition: false,
        finding: NEWER_REDIS_FIELDS,
    },
    ExpectedDifference {
        // nor scoped tokens
        builds: BEFORE_NEWER_REDIS_FIELDS,
        place: "redis.json",
        side: ".scoped_tokens: the older backup holds nothing,",
        holds: "",
        at_most: 1,
        in_bidirectional_partition: false,
        finding: NEWER_REDIS_FIELDS,
    },
    ExpectedDifference {
        // so its restore rebuilds each group's set from the group's images, and a set no image's
        // policies make is missing, which this build's restore has
        builds: BEFORE_NEWER_REDIS_FIELDS,
        place: "restored netpols:*",
        side: "[netpols:*] redis netpols:",
        holds: "found <missing>",
        at_most: usize::MAX,
        in_bidirectional_partition: false,
        finding: NEWER_REDIS_FIELDS,
    },
    ExpectedDifference {
        // and the source has them
        builds: BEFORE_NEWER_REDIS_FIELDS,
        place: "source netpols:*",
        side: "[netpols:*] redis netpols:",
        holds: "found <missing>",
        at_most: usize::MAX,
        in_bidirectional_partition: false,
        finding: NEWER_REDIS_FIELDS,
    },
    ExpectedDifference {
        // and its restore writes no user's scoped tokens, which this build's restore has
        builds: BEFORE_NEWER_REDIS_FIELDS,
        place: "restored scoped_tokens:*",
        side: "[scoped_tokens:*] redis scoped_tokens:",
        holds: "found <missing>",
        at_most: usize::MAX,
        in_bidirectional_partition: false,
        finding: NEWER_REDIS_FIELDS,
    },
    ExpectedDifference {
        // and the source has them
        builds: BEFORE_NEWER_REDIS_FIELDS,
        place: "source scoped_tokens:*",
        side: "[scoped_tokens:*] redis scoped_tokens:",
        holds: "found <missing>",
        at_most: usize::MAX,
        in_bidirectional_partition: false,
        finding: NEWER_REDIS_FIELDS,
    },
    ExpectedDifference {
        // nor the map from scoped token to owner
        builds: BEFORE_NEWER_REDIS_FIELDS,
        place: "restored scoped_token_map",
        side: "[scoped_token_map] redis scoped_token_map",
        holds: "found <missing>",
        at_most: usize::MAX,
        in_bidirectional_partition: false,
        finding: NEWER_REDIS_FIELDS,
    },
    ExpectedDifference {
        // and the source has them
        builds: BEFORE_NEWER_REDIS_FIELDS,
        place: "source scoped_token_map",
        side: "[scoped_token_map] redis scoped_token_map",
        holds: "found <missing>",
        at_most: usize::MAX,
        in_bidirectional_partition: false,
        finding: NEWER_REDIS_FIELDS,
    },
];

/// One way an older build's backup differs from this build's backup of the same source
#[derive(Debug)]
struct CompatDifference {
    /// The comparison: `<table> archives`, `<table> objects`, `redis.json`, or `restored <entry>`
    place: String,
    /// The difference, rendered
    text: String,
}

/// Returns the entry of [`EXPECTED_DIFFERENCES`] whose rows a difference of an older build
/// matches, if any
///
/// # Arguments
///
/// * `build` - The older build's label
/// * `difference` - The difference
/// * `bidirectional` - The census streams of the partitions holding a bidirectional association
fn expected(build: &str, difference: &CompatDifference, bidirectional: &[String]) -> Option<usize> {
    // only an entry for this build, this comparison, this side, this row, and, for a census
    // stream, a bidirectional association's partition matches it
    EXPECTED_DIFFERENCES.iter().position(|known| {
        known.builds.contains(&build)
            && known.place == difference.place
            && difference.text.starts_with(known.side)
            && difference.text.contains(known.holds)
            && (!known.in_bidirectional_partition
                || bidirectional
                    .iter()
                    .any(|stream| difference.text.contains(stream.as_str())))
    })
}

/// Returns the census stream of each partition that holds a bidirectional association's row, as
/// the census comparison names it (`census:associations:stream:<group>:<source>:<year>:`)
///
/// # Arguments
///
/// * `snapshot` - The snapshot holding the associations
fn bidirectional_streams(snapshot: &Snapshot) -> Vec<String> {
    // each bidirectional row's group, source, and year
    snapshot
        .scylla
        .get("associations")
        .into_iter()
        .flat_map(BTreeMap::values)
        .filter(|row| {
            row.get("direction")
                .is_some_and(|direction| direction.contains("Bidirectional"))
        })
        .filter_map(|row| {
            let group = text_of(row.get("group")?)?;
            let source = text_of(row.get("source")?)?;
            let year = row.get("year")?.strip_prefix("Int(")?.strip_suffix(')')?;
            Some(format!(
                "census:associations:stream:{group}:{source}:{year}:"
            ))
        })
        .collect()
}

/// Splits an older build's differences into the findings that explain some of them and the
/// differences no finding explains
///
/// Each entry of [`EXPECTED_DIFFERENCES`] explains at most its `at_most` differences; any more
/// it matches stay unexplained.
///
/// # Arguments
///
/// * `build` - The older build's label
/// * `differences` - Every difference of the older build's backup from this build's
/// * `bidirectional` - The census streams of the partitions holding a bidirectional association
fn explain(
    build: &str,
    differences: Vec<CompatDifference>,
    bidirectional: &[String],
) -> (Vec<&'static str>, Vec<CompatDifference>) {
    // count the differences each entry explains, keeping each explaining finding once
    let mut used = vec![0_usize; EXPECTED_DIFFERENCES.len()];
    let mut findings = Vec::new();
    let mut unexplained = Vec::new();
    for difference in differences {
        match expected(build, &difference, bidirectional) {
            Some(entry) if used[entry] < EXPECTED_DIFFERENCES[entry].at_most => {
                used[entry] += 1;
                let finding = EXPECTED_DIFFERENCES[entry].finding;
                if !findings.contains(&finding) {
                    findings.push(finding);
                }
            }
            _ => unexplained.push(difference),
        }
    }
    (findings, unexplained)
}

/// Returns the rows only one of two lists holds, counting repeats, as differences
///
/// # Arguments
///
/// * `place` - The comparison's name
/// * `older` - The older build's rows
/// * `newer` - This build's rows
fn row_differences(place: &str, older: Vec<String>, newer: Vec<String>) -> Vec<CompatDifference> {
    // count each rendered row on both sides
    let mut counts: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for row in older {
        counts.entry(row).or_default().0 += 1;
    }
    for row in newer {
        counts.entry(row).or_default().1 += 1;
    }
    // a row held more times by one side is a difference for each extra copy
    let mut differences = Vec::new();
    for (row, (in_older, in_newer)) in counts {
        for _ in in_newer..in_older {
            differences.push(CompatDifference {
                place: place.to_owned(),
                text: format!("only in the older backup: {row}"),
            });
        }
        for _ in in_older..in_newer {
            differences.push(CompatDifference {
                place: place.to_owned(),
                text: format!("only in this build's backup: {row}"),
            });
        }
    }
    differences
}

/// Renders an archived row as format 1 holds it, so rows of every format compare: without the
/// columns format 2 added, and with maps in key order
trait FormatOne {
    /// Renders the row without the columns format 2 added
    fn format_one(self) -> String;
}

/// Implements [`FormatOne`] for tables whose rows format 2 left alone or only made optional
macro_rules! format_one_as_debug {
    ($($table:ty),* $(,)?) => {$(
        impl FormatOne for $table {
            fn format_one(self) -> String {
                // every field format 1 holds, as its Debug renders it
                format!("{self:?}")
            }
        }
    )*};
}

format_one_as_debug!(
    SamplesList,
    S3Id,
    Comment,
    Tag,
    RepoData,
    Commitish,
    CommitishList,
    Node,
    NetworkPolicy,
    Entity,
);

impl FormatOne for ResultOutput {
    fn format_one(self) -> String {
        // the children in key order, without the tool version and entities format 2 added
        let children = self
            .children
            .map(|children| children.into_iter().collect::<BTreeMap<_, _>>());
        format!(
            "{:?} {:?} {:?} {:?} {:?} {:?} {:?} {children:?}",
            self.id, self.tool, self.cmd, self.uploaded, self.result, self.files, self.display_type
        )
    }
}

impl FormatOne for OutputStream {
    fn format_one(mut self) -> String {
        // without the tool version format 2 added
        self.tool_version = None;
        format!("{self:?}")
    }
}

impl FormatOne for RepoList {
    fn format_one(mut self) -> String {
        // without the default checkout and earliest commit format 2 added
        self.default_checkout = None;
        self.earliest = None;
        format!("{self:?}")
    }
}

impl FormatOne for Association {
    fn format_one(mut self) -> String {
        // without the submissions format 2 added
        self.submissions_source = None;
        self.submissions_other = None;
        format!("{self:?}")
    }
}

impl FormatOne for Notification {
    fn format_one(mut self) -> String {
        // without the TTL format 2 added
        self.ttl = None;
        format!("{self:?}")
    }
}

/// Returns the rows of one table's archives, read in the backup's format and rendered as format
/// 1 holds them
///
/// # Arguments
///
/// * `backup` - The backup directory
/// * `format` - The backup's format version; a legacy backup's is 1
async fn table_rows<T: Versioned + Utils + FormatOne>(backup: &Path, format: u32) -> Vec<String> {
    // every row of every archive, as format 1 holds it
    archives::versioned_rows::<T>(&backup.join(T::name()), format)
        .await
        .into_iter()
        .map(FormatOne::format_one)
        .collect()
}

/// Compares every table's archived rows of an older build's backup with this build's
///
/// # Arguments
///
/// * `older` - The older build's backup
/// * `format` - The older backup's format version; a legacy backup's is 1
/// * `newer` - This build's backup of the same source
async fn archive_differences(older: &Path, format: u32, newer: &Path) -> Vec<CompatDifference> {
    // one comparison per table, named for its directory
    macro_rules! compare {
        ($($table:ty),* $(,)?) => {{
            let mut differences = Vec::new();
            $(
                let place = format!("{} archives", <$table>::name());
                differences.extend(row_differences(
                    &place,
                    table_rows::<$table>(older, format).await,
                    table_rows::<$table>(newer, FORMAT_VERSION).await,
                ));
            )*
            differences
        }};
    }
    compare!(
        SamplesList,
        S3Id,
        Comment,
        ResultOutput,
        OutputStream,
        Tag,
        RepoData,
        RepoList,
        Commitish,
        CommitishList,
        Node,
        NetworkPolicy,
        Entity,
        Association,
        Notification,
    )
}

/// Returns every file under a directory by its path relative to it, with its sha256
///
/// # Arguments
///
/// * `root` - The directory to walk
fn file_hashes(root: &Path) -> BTreeMap<String, String> {
    // depth first; a directory that doesn't exist holds nothing
    let mut found = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(listing) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in listing.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let relative = path.strip_prefix(root).unwrap().display().to_string();
                let sha256 = HEXLOWER.encode(&Sha256::digest(std::fs::read(&path).unwrap()));
                found.insert(relative, sha256);
            }
        }
    }
    found
}

/// Compares the objects and the Redis state of an older build's backup with this build's
///
/// # Arguments
///
/// * `older` - The older build's backup
/// * `newer` - This build's backup of the same source
fn file_differences(older: &Path, newer: &Path) -> Vec<CompatDifference> {
    // every object of every S3 component, by path and sha256
    let mut differences = Vec::new();
    for table in [
        S3Id::name(),
        Comment::name(),
        ResultOutput::name(),
        "graphics",
    ] {
        let place = format!("{table} objects");
        let render = |hashes: BTreeMap<String, String>| {
            hashes
                .into_iter()
                .map(|(path, sha256)| format!("{path} {sha256}"))
                .collect::<Vec<_>>()
        };
        differences.extend(row_differences(
            &place,
            render(file_hashes(&older.join(table).join("objects"))),
            render(file_hashes(&newer.join(table).join("objects"))),
        ));
    }
    // the Redis state, compared as JSON with lists in a stable order
    let state = |dir: &Path| {
        std::fs::read(dir.join("redis.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .map(canonical)
    };
    let mut paths = Vec::new();
    json_differences("", state(older).as_ref(), state(newer).as_ref(), &mut paths);
    differences.extend(paths.into_iter().map(|text| CompatDifference {
        place: "redis.json".to_owned(),
        text,
    }));
    differences
}

/// Returns a JSON value with every list sorted by its items' rendering, so two states that hold
/// the same records in another order are equal
///
/// # Arguments
///
/// * `value` - The value to sort
fn canonical(value: serde_json::Value) -> serde_json::Value {
    // sort lists after sorting what they hold; objects already keep their keys sorted
    match value {
        serde_json::Value::Array(items) => {
            let mut items: Vec<serde_json::Value> = items.into_iter().map(canonical).collect();
            items.sort_by_key(ToString::to_string);
            serde_json::Value::Array(items)
        }
        serde_json::Value::Object(object) => serde_json::Value::Object(
            object
                .into_iter()
                .map(|(key, value)| (key, canonical(value)))
                .collect(),
        ),
        other => other,
    }
}

/// Collects the paths at which two JSON values differ, with both values
///
/// # Arguments
///
/// * `path` - The path of the values being compared
/// * `older` - The older backup's value, if it has one
/// * `newer` - This build's value, if it has one
/// * `found` - Where to collect the differences
fn json_differences(
    path: &str,
    older: Option<&serde_json::Value>,
    newer: Option<&serde_json::Value>,
    found: &mut Vec<String>,
) {
    // equal values hold no differences
    if older == newer {
        return;
    }
    // objects and lists are compared member by member, anything else as a whole
    match (older, newer) {
        (Some(serde_json::Value::Object(a)), Some(serde_json::Value::Object(b))) => {
            let keys: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
            for key in keys {
                json_differences(&format!("{path}.{key}"), a.get(key), b.get(key), found);
            }
        }
        (Some(serde_json::Value::Array(a)), Some(serde_json::Value::Array(b))) => {
            for index in 0..a.len().max(b.len()) {
                json_differences(
                    &format!("{path}[{index}]"),
                    a.get(index),
                    b.get(index),
                    found,
                );
            }
        }
        _ => found.push(format!(
            "{path}: the older backup holds {}, this build's {}",
            older.map_or_else(|| "nothing".to_owned(), ToString::to_string),
            newer.map_or_else(|| "nothing".to_owned(), ToString::to_string)
        )),
    }
}

/// One older thoradm binary, labelled with the commit it was built from
#[derive(Debug, Clone)]
struct Older {
    /// The label, usually the commit
    label: String,
    /// The binary
    path: PathBuf,
}

/// Reads the older binaries named in `THORADM_OLDER_BINARIES`
fn older_binaries() -> Vec<Older> {
    // `label=path` pairs separated by commas
    std::env::var("THORADM_OLDER_BINARIES")
        .unwrap_or_default()
        .split(',')
        .filter(|pair| !pair.trim().is_empty())
        .map(|pair| {
            let (label, path) = pair
                .split_once('=')
                .unwrap_or_else(|| panic!("{pair} isn't label=path"));
            Older {
                label: label.trim().to_owned(),
                path: PathBuf::from(path.trim()),
            }
        })
        .collect()
}

/// Builds a command running an older thoradm binary against the harness's namespace
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `binary` - The older binary
fn older_thoradm(harness: &Harness, binary: &Path) -> Command {
    // the same configs and the same cleared environment the harness gives this build
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

/// Backs up the namespace with an older binary, the way its admins had to: every component, then,
/// for a build that writes no manifest, the S3 components that copy objects again, since those
/// builds copied attachments and result files only on a second run into the same directory
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `binary` - The older binary
/// * `dir` - Where to write the backup
fn older_backup(harness: &Harness, binary: &Path, dir: &Path) -> Vec<Output> {
    // one run of a component list into the directory, with a small worker count
    let run = |components: &str| {
        older_thoradm(harness, binary)
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
            .expect("failed to run an older thoradm")
    };
    // every component first
    let mut runs = vec![run("all")];
    // a build that writes a manifest backs up its objects in that one run, and would refuse a
    // second run into a finished backup
    if !dir.join("manifest.json").exists() {
        runs.push(run("comment-attachments,result-files"));
    }
    runs
}

/// Returns the format version a backup's manifest records, or `None` for a legacy backup without
/// a manifest
///
/// # Arguments
///
/// * `dir` - The backup
fn manifest_format(dir: &Path) -> Option<u32> {
    // read only the version, since older manifests may lack fields this build's has
    let bytes = std::fs::read(dir.join("manifest.json")).ok()?;
    let manifest: serde_json::Value =
        serde_json::from_slice(&bytes).expect("an older manifest isn't JSON");
    let format = manifest["format_version"]
        .as_u64()
        .expect("an older manifest has no format version");
    Some(u32::try_from(format).expect("an older manifest's format version is too large"))
}

/// Returns whether a difference is one the oracle may report for an older backup: an entry of a
/// kind that doesn't restore yet, or a column a known finding loses
///
/// # Arguments
///
/// * `difference` - The difference
fn explained(difference: &Difference) -> Option<&'static str> {
    // every entry of a kind that doesn't restore yet, its views included
    let broken = KNOWN_BROKEN.iter().find(|broken| {
        broken.entries.contains(&difference.entry.as_str())
            || KINDS.iter().any(|(kind, entries)| {
                *kind == broken.kind && entries.contains(&difference.entry.as_str())
            })
    });
    if let Some(broken) = broken {
        return Some(broken.finding);
    }
    // columns a known finding loses
    KNOWN_LOSSES
        .iter()
        .find(|loss| {
            difference.entry == loss.table
                && difference
                    .location
                    .starts_with(&format!("{} [", loss.table))
                && difference.location.ends_with(&format!("] {}", loss.column))
        })
        .map(|loss| loss.finding)
}

/// What restoring one backup with this build gave
struct Restored {
    /// The restore run
    output: Output,
    /// What the oracle found against the source that no known finding explains
    unexplained: Vec<Difference>,
    /// The findings that explained the rest
    findings: Vec<&'static str>,
    /// The namespace after the restore
    snapshot: Snapshot,
}

/// Restores a backup into the wiped namespace with this build and compares it with the source
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `dir` - The backup to restore
/// * `source` - The snapshot of the namespace the backup was taken from
/// * `flags` - The `backup restore` flags the backup needs (`--legacy` without a manifest)
/// * `format` - The backup's format version; a legacy backup's is 1
async fn restore_and_compare(
    harness: &Harness,
    dir: &Path,
    source: &Snapshot,
    flags: &[&str],
    format: u32,
) -> Restored {
    // wipe and restore as an admin would after a disaster
    wipe(harness).await;
    let restored = restore_with(harness, dir, flags);
    // compare everything as the backup's format carries it
    let found = Snapshot::take(harness).await;
    let oracle = Oracle {
        format: Some(format),
        ..Oracle::default()
    };
    let mut unexplained = Vec::new();
    let mut findings = Vec::new();
    for difference in oracle.compare(source, &found) {
        match explained(&difference) {
            Some(finding) => {
                if !findings.contains(&finding) {
                    findings.push(finding);
                }
            }
            None => unexplained.push(difference),
        }
    }
    Restored {
        output: restored,
        unexplained,
        findings,
        snapshot: found,
    }
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

/// Returns a snapshot without its row TTLs, which count down between two snapshots of a target
/// nothing changed
///
/// # Arguments
///
/// * `snapshot` - The snapshot
fn without_ttls(mut snapshot: Snapshot) -> Snapshot {
    // the rows themselves are still compared
    snapshot.ttls.clear();
    snapshot
}

/// Returns whether a restore of a backup left a preflight record of a check in its report
///
/// # Arguments
///
/// * `dir` - The backup
/// * `check` - The preflight check that must have refused
fn refused_by(dir: &Path, check: &str) -> bool {
    // the refusal's record names its check as its object
    failure_records(dir).iter().any(|record| {
        record.operation == thoradm::backup::failures::Operation::Preflight
            && record.object.as_deref() == Some(check)
    })
}

/// Restores a backup, checking the restore is refused by one preflight check before anything is
/// written; returns what went wrong, if anything
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `dir` - The backup to restore
/// * `flags` - The `backup restore` flags to pass
/// * `check` - The preflight check that must refuse
/// * `named` - Text the refusal must name (its override flag)
async fn check_refused(
    harness: &Harness,
    dir: &Path,
    flags: &[&str],
    check: &str,
    named: &str,
) -> Vec<String> {
    // the restore refuses and changes nothing anywhere
    let before = Snapshot::take(harness).await;
    let refused = restore_with(harness, dir, flags);
    let after = Snapshot::take(harness).await;
    // compare the snapshots without the TTLs, which count down
    let (before, after) = (without_ttls(before), without_ttls(after));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    let changed = Oracle {
        rules: &[],
        format: None,
    }
    .compare(&before, &after);
    // it exits 1 with a record of the check, naming the override, and nothing changed
    let mut problems = Vec::new();
    if refused.status.code() != Some(1) {
        problems.push(format!("exit {:?}", refused.status.code()));
    }
    if !refused_by(dir, check) {
        problems.push(format!("no preflight record of the {check} check"));
    }
    if !failure_records(dir)
        .iter()
        .any(|record| record.error.contains(named))
    {
        problems.push(format!("the refusal doesn't name {named}: {stderr}"));
    }
    if !changed.is_empty() {
        let shown: Vec<String> = changed.iter().take(5).map(ToString::to_string).collect();
        problems.push(format!("{} changes:\n{}", changed.len(), shown.join("\n")));
    }
    problems
}

/// Restores a copy of a backup marked one format version newer than this build reads, checking
/// it is refused before anything is written; returns the verdict row and any failure
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `label` - The older binary that took the backup
/// * `dir` - The backup to copy
async fn check_newer_refused(
    harness: &Harness,
    label: &str,
    dir: &Path,
) -> (String, Option<String>) {
    // copy the backup and give it a manifest from a newer thoradm
    let newer = harness.scratch("newer-format");
    copy_tree(dir, &newer);
    // the reports of the earlier restore of this backup aren't part of the copy
    let _ = std::fs::remove_dir_all(newer.join("reports"));
    let manifest = serde_json::json!({
        "format_version": FORMAT_VERSION + 1,
        "thoradm_version": "9.9.9",
    });
    std::fs::write(newer.join("manifest.json"), manifest.to_string()).unwrap();
    // the restore refuses, naming the version, and changes nothing anywhere
    let before = Snapshot::take(harness).await;
    let refused = restore_with(harness, &newer, &[]);
    let after = Snapshot::take(harness).await;
    let (before, after) = (without_ttls(before), without_ttls(after));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    // compared as they are, with none of the rules a restore's result is judged by
    let changed = Oracle {
        rules: &[],
        format: None,
    }
    .compare(&before, &after);
    let mut problems = Vec::new();
    if refused.status.code() != Some(1) {
        problems.push(format!("exit {:?}", refused.status.code()));
    }
    if !stderr.contains(&format!("format version {}", FORMAT_VERSION + 1)) {
        problems.push("the refusal doesn't name the version".to_owned());
    }
    if !changed.is_empty() {
        let shown: Vec<String> = changed.iter().take(5).map(ToString::to_string).collect();
        problems.push(format!("{} changes:\n{}", changed.len(), shown.join("\n")));
    }
    if !refused_by(&newer, "manifest") {
        problems.push("no preflight record of the manifest check".to_owned());
    }
    let refused_ok = problems.is_empty();
    let row = format!(
        "| copy of {label} marked format {} | newer | {} | none overrides it | | |\n",
        FORMAT_VERSION + 1,
        if refused_ok { "refused" } else { "FAILS" }
    );
    let failure = (!refused_ok).then(|| format!("newer format: {}", problems.join("; ")));
    (row, failure)
}

/// Backups of the fixture taken by every older binary equal this build's backup of the same
/// source, row for row, object for object, and in their Redis state, and restore with this build
/// to the namespace this build's own backup restores to; the only differences allowed are the
/// rows a finding names for that build, and against the source the kinds and columns restore
/// doesn't bring back for any backup. An older backup without a manifest restores only with
/// `--legacy`, and one with a manifest restores without it; a copy marked one format version
/// newer, a legacy backup without `--legacy`, and a copy that records a failure (without
/// `--accept-failures`) are refused before anything is written
#[tokio::test]
#[ignore = "run at the backup compatibility review, with older thoradm binaries named in THORADM_OLDER_BINARIES"]
#[allow(clippy::too_many_lines)]
async fn backups_from_older_builds_restore() {
    let binaries = older_binaries();
    assert!(
        !binaries.is_empty(),
        "THORADM_OLDER_BINARIES names no older thoradm binaries"
    );
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    // seed the fixture and snapshot it with its census
    fixture::broad(harness).await;
    let cases = FixtureCases {
        format_1_unset: false,
        ..FixtureCases::ALL
    };
    let _ = Box::pin(fixture::value_classes_with(harness, cases)).await;
    let _ = census(harness);
    let source = Snapshot::take(harness).await;
    let bidirectional = bidirectional_streams(&source);
    // this build backs up the source, to compare every older backup with
    let newer = harness.scratch("this-build");
    let taken = backup(harness, &newer);
    assert!(
        matches!(taken.status.code(), Some(0 | 2)),
        "this build's backup: {}",
        describe(&taken)
    );
    // every older binary backs up the same source
    let mut backups = Vec::new();
    for older in &binaries {
        let dir = harness.scratch(&format!("older-{}", older.label));
        let runs = older_backup(harness, &older.path, &dir);
        for run in &runs {
            assert!(
                matches!(run.status.code(), Some(0 | 2)),
                "{} backup: {}",
                older.label,
                describe(run)
            );
        }
        backups.push((older.clone(), dir));
    }
    // this build's backup restores, giving what every older backup's restore must equal
    let mut failures = Vec::new();
    let this_build = restore_and_compare(
        harness,
        &newer,
        &source,
        &["--accept-failures"],
        FORMAT_VERSION,
    )
    .await;
    if !matches!(this_build.output.status.code(), Some(0 | 2)) {
        failures.push(format!(
            "this build's restore: {}",
            describe(&this_build.output)
        ));
    }
    // each older backup equals this build's, and restores with this build to the same namespace
    let mut verdicts = String::from(
        "| Backup | Format | Verdict | Flags | Explained differences | Unexplained differences |\n|---|---|---|---|---|---|\n",
    );
    for (older, dir) in &backups {
        // every archived row, object, and Redis field compared with this build's backup
        // a backup with a manifest restores in the format it records, accepting the failures it
        // recorded; one without is legacy, read as format 1 and restored with --legacy
        let format = manifest_format(dir);
        let (flags, format_name, format) = match format {
            Some(format) => (["--accept-failures"], format!("format {format}"), format),
            None => (["--legacy"], "legacy (no manifest)".to_owned(), 1),
        };
        let mut compat = archive_differences(dir, format, &newer).await;
        compat.extend(file_differences(dir, &newer));
        let restored = restore_and_compare(harness, dir, &source, &flags, format).await;
        let stderr = String::from_utf8_lossy(&restored.output.stderr);
        if flags == ["--legacy"] && !stderr.contains("as a legacy backup (--legacy)") {
            failures.push(format!(
                "{}: the restore didn't warn that it is legacy",
                older.label
            ));
        }
        // the restored namespace compared with this build's restored backup, every kind included
        let oracle = Oracle {
            rules: RESTORED_RULES.as_slice(),
            format: Some(format),
        };
        compat.extend(
            oracle
                .compare(&this_build.snapshot, &restored.snapshot)
                .into_iter()
                .map(|difference| CompatDifference {
                    place: format!("restored {}", difference.entry),
                    text: difference.to_string(),
                }),
        );
        // only the rows a finding names for this build are explained
        let mut findings = restored.findings.clone();
        let (explaining, unexplained_compat) = explain(&older.label, compat, &bidirectional);
        for finding in explaining {
            if !findings.contains(&finding) {
                findings.push(finding);
            }
        }
        // differences from the source that a finding names for this build are explained too
        let (explaining, unexplained) = explain(
            &older.label,
            restored
                .unexplained
                .iter()
                .map(|difference| CompatDifference {
                    place: format!("source {}", difference.entry),
                    text: difference.to_string(),
                })
                .collect(),
            &bidirectional,
        );
        for finding in explaining {
            if !findings.contains(&finding) {
                findings.push(finding);
            }
        }
        let restored_ok = matches!(restored.output.status.code(), Some(0 | 2));
        let verdict = if restored_ok && unexplained.is_empty() && unexplained_compat.is_empty() {
            "restores"
        } else {
            "FAILS"
        };
        let _ = writeln!(
            verdicts,
            "| {} | {format_name} | {verdict} | `--confirm {}` | {} | {} |",
            older.label,
            flags[0],
            if findings.is_empty() {
                "none".to_owned()
            } else {
                findings.join("; ")
            },
            unexplained.len() + unexplained_compat.len()
        );
        if !restored_ok {
            failures.push(format!(
                "{} restore: {}",
                older.label,
                describe(&restored.output)
            ));
        }
        if !unexplained_compat.is_empty() {
            // how many per comparison, then a few of each
            let mut per_place: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
            for difference in &unexplained_compat {
                per_place
                    .entry(difference.place.as_str())
                    .or_default()
                    .push(difference.text.as_str());
            }
            let mut text = format!(
                "{}: {} differences from this build's backup:",
                older.label,
                unexplained_compat.len()
            );
            for (place, found) in &per_place {
                let shown: Vec<&str> = found.iter().take(3).copied().collect();
                let _ = write!(text, "\n{place}: {}\n  {}", found.len(), shown.join("\n  "));
            }
            failures.push(text);
        }
        if !unexplained.is_empty() {
            // how many per entry, then a few of each
            let mut per_entry: std::collections::BTreeMap<&str, Vec<String>> =
                std::collections::BTreeMap::new();
            for difference in &unexplained {
                per_entry
                    .entry(difference.place.as_str())
                    .or_default()
                    .push(difference.text.clone());
            }
            let mut text = format!(
                "{}: {} unexplained differences:",
                older.label,
                unexplained.len()
            );
            for (entry, found) in &per_entry {
                let _ = write!(
                    text,
                    "\n{entry}: {}\n  {}",
                    found.len(),
                    found
                        .iter()
                        .take(3)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("\n  ")
                );
            }
            failures.push(text);
        }
    }
    // a copy marked one format version newer is refused before anything is written
    let (older, dir) = &backups[0];
    let (row, refusal) = check_newer_refused(harness, &older.label, dir).await;
    verdicts.push_str(&row);
    failures.extend(refusal);
    // a backup without a manifest is refused without --legacy, naming the flag
    if let Some((older, dir)) = backups
        .iter()
        .find(|(_, dir)| manifest_format(dir).is_none())
    {
        let legacy_copy = harness.scratch("legacy-refused");
        copy_tree(dir, &legacy_copy);
        let _ = std::fs::remove_dir_all(legacy_copy.join("reports"));
        let problems = check_refused(harness, &legacy_copy, &[], "manifest", "--legacy").await;
        let _ = writeln!(
            verdicts,
            "| {} without `--legacy` | legacy | {} | `--legacy` overrides it | | |",
            older.label,
            if problems.is_empty() {
                "refused"
            } else {
                "FAILS"
            }
        );
        if !problems.is_empty() {
            failures.push(format!("legacy without --legacy: {}", problems.join("; ")));
        }
    }
    // a copy of this build's backup that records a failure is refused without --accept-failures
    let recorded = harness.scratch("recorded-refused");
    copy_tree(&newer, &recorded);
    let _ = std::fs::remove_dir_all(recorded.join("reports"));
    record_a_failure(&recorded).await;
    let problems = check_refused(
        harness,
        &recorded,
        &[],
        "recorded-failures",
        "--accept-failures",
    )
    .await;
    let _ = writeln!(
        verdicts,
        "| a copy of this build's backup with a recorded failure | format {FORMAT_VERSION} | {} | `--accept-failures` overrides it | | |",
        if problems.is_empty() {
            "refused"
        } else {
            "FAILS"
        }
    );
    if !problems.is_empty() {
        failures.push(format!("recorded failure: {}", problems.join("; ")));
    }
    let dir = report_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("cross-version.md"), &verdicts).unwrap();
    println!("{verdicts}");
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// Makes a finished backup record one failed object, as a backup that couldn't download it would:
/// a line in `failures.jsonl` and the manifest's count and checksum of it
///
/// # Arguments
///
/// * `dir` - The backup
async fn record_a_failure(dir: &Path) {
    // one failed download, listed and pinned by the manifest
    let record = thoradm::backup::failures::FailureRecord::failed(
        "result-files",
        thoradm::backup::failures::Operation::Download,
        "the object went missing",
    )
    .bucket("testing-results")
    .key("a-result/out.bin");
    let failures = dir.join(thoradm::backup::failures::FAILURES_FILE);
    std::fs::write(
        &failures,
        format!("{}\n", serde_json::to_string(&record).unwrap()),
    )
    .unwrap();
    // the manifest counts it and pins the file's sha256
    let sha256 = thoradm::backup::manifest::file_sha256(&failures)
        .await
        .unwrap();
    let manifest = dir.join("manifest.json");
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    doc["failures"] = serde_json::Value::from(1);
    doc["failures_sha256"] = serde_json::Value::from(sha256);
    std::fs::write(&manifest, serde_json::to_vec(&doc).unwrap()).unwrap();
}

/// Builds a difference in 4e405bb's associations archives
///
/// # Arguments
///
/// * `side` - Which backup alone holds the row, as the rendering names it
/// * `direction` - The rendered direction of the row
fn association_difference(side: &str, direction: &str) -> CompatDifference {
    // render the row the way the archive comparison does
    CompatDifference {
        place: "associations archives".to_owned(),
        text: format!(
            "only in {side}: ArchivedAssociation {{ kind: FileFor, direction: {direction} }}"
        ),
    }
}

/// 4e405bb's missing bidirectional association is explained only as a row only this build's
/// backup holds, and only as many times as the fixture holds such associations: a reversed or
/// extra difference, and a missing census stream of another partition, stays unexplained
#[test]
fn only_the_missing_bidirectional_association_is_explained() {
    // rows only this build's backup holds name it this way
    let this_build = "this build's backup";
    // the one row 4e405bb skips is explained by its finding
    let (findings, unexplained) = explain(
        "4e405bb",
        vec![association_difference(this_build, "Bidirectional")],
        &[],
    );
    assert_eq!(
        findings,
        vec!["Bidirectional associations fail to read back from Scylla"]
    );
    assert!(unexplained.is_empty(), "{unexplained:?}");
    // a row only the older backup holds isn't what the finding explains
    let (_, unexplained) = explain(
        "4e405bb",
        vec![association_difference("the older backup", "Bidirectional")],
        &[],
    );
    assert_eq!(unexplained.len(), 1, "a reversed difference was explained");
    // a second missing row is more than the fixture holds
    let (_, unexplained) = explain(
        "4e405bb",
        vec![
            association_difference(this_build, "Bidirectional"),
            association_difference(this_build, "Bidirectional"),
        ],
        &[],
    );
    assert_eq!(unexplained.len(), 1, "an extra difference was explained");
    // another build or another direction isn't explained
    let (_, unexplained) = explain(
        "ae918c9",
        vec![association_difference(this_build, "Bidirectional")],
        &[],
    );
    assert_eq!(
        unexplained.len(),
        1,
        "another build's difference was explained"
    );
    let (_, unexplained) = explain(
        "4e405bb",
        vec![association_difference(this_build, "To")],
        &[],
    );
    assert_eq!(unexplained.len(), 1, "another direction was explained");
    // the census stream of the association's partition, missing after 4e405bb's restore, is
    // explained once against this build's restore and once against the source
    let partition = vec!["census:associations:stream:group-1:{\"fixture\":\"2\"}:2025:".to_owned()];
    let stream = |place: &str| CompatDifference {
        place: place.to_owned(),
        text: "[associations] census census:associations:stream:group-1:{\"fixture\":\"2\"}:2025: \
               expected 1 buckets [\"79502\"], found <missing>"
            .to_owned(),
    };
    let (findings, unexplained) = explain(
        "4e405bb",
        vec![
            stream("restored associations"),
            stream("source associations"),
        ],
        &partition,
    );
    assert!(unexplained.is_empty(), "{unexplained:?}");
    assert_eq!(
        findings,
        vec!["Bidirectional associations fail to read back from Scylla"]
    );
    // a second missing stream is more than the fixture holds
    let (_, unexplained) = explain(
        "4e405bb",
        vec![
            stream("restored associations"),
            stream("restored associations"),
        ],
        &partition,
    );
    assert_eq!(unexplained.len(), 1, "an extra stream was explained");
    // a missing stream of another partition isn't the association's
    let other = CompatDifference {
        place: "restored associations".to_owned(),
        text: "[associations] census census:associations:stream:group-7:{\"fixture\":\"9\"}:2025: \
               expected 1 buckets [\"79502\"], found <missing>"
            .to_owned(),
    };
    let (_, unexplained) = explain("4e405bb", vec![other], &partition);
    assert_eq!(
        unexplained.len(),
        1,
        "another partition's missing stream was explained"
    );
}

/// The census stream of a partition holding a bidirectional association's row is named from the
/// row's group, source, and year, and a row of another direction names none
#[test]
fn a_bidirectional_rows_partition_names_its_census_stream() {
    // two rows rendered the way snapshots read them, one of each direction
    let row = |direction: &str, group: &str| -> common::oracle::Row {
        let text = |value: &str| render(Some(&CqlValue::Text(value.to_owned()))).unwrap();
        BTreeMap::from([
            ("direction".to_owned(), text(direction)),
            ("group".to_owned(), text(group)),
            ("source".to_owned(), text("{\"fixture\":\"2\"}")),
            (
                "year".to_owned(),
                render(Some(&CqlValue::Int(2025))).unwrap(),
            ),
        ])
    };
    let mut snapshot = Snapshot::default();
    snapshot.scylla.insert(
        "associations".to_owned(),
        BTreeMap::from([
            ("a".to_owned(), row("Bidirectional", "group-1")),
            ("b".to_owned(), row("To", "group-2")),
        ]),
    );
    // only the bidirectional row's partition is named
    assert_eq!(
        bidirectional_streams(&snapshot),
        vec!["census:associations:stream:group-1:{\"fixture\":\"2\"}:2025:".to_owned()]
    );
}

/// The scoped tokens and network-policy sets an older build's API never backed up are explained
/// only for those builds, only as keys missing after the restore and as the two fields its
/// `redis.json` lacks; another build, another Redis family, a set that differs instead of being
/// missing, or a field the older backup holds differently stays unexplained
#[test]
fn only_the_redis_families_older_apis_never_backed_up_are_explained() {
    // a key missing after the older backup's restore, rendered the way the oracle does
    let missing = |place: &str, family: &str, key: &str| CompatDifference {
        place: format!("{place} {family}"),
        text: format!("[{family}] redis {key}: expected Set({{\"policy-1\"}}), found <missing>"),
    };
    // a field only this build's redis.json holds
    let field = |name: &str| CompatDifference {
        place: "redis.json".to_owned(),
        text: format!(
            ".{name}: the older backup holds nothing, this build's [{{\"group\":\"g\"}}]"
        ),
    };
    // every such difference of an older build is explained by the one finding
    for build in BEFORE_NEWER_REDIS_FIELDS {
        let (findings, unexplained) = explain(
            build,
            vec![
                field("network_policy_sets"),
                field("scoped_tokens"),
                missing("restored", "netpols:*", "netpols:group-1"),
                missing("source", "netpols:*", "netpols:group-1"),
                missing("restored", "scoped_tokens:*", "scoped_tokens:user-1"),
                missing("source", "scoped_tokens:*", "scoped_tokens:user-1"),
                missing("restored", "scoped_token_map", "scoped_token_map"),
                missing("source", "scoped_token_map", "scoped_token_map"),
            ],
            &[],
        );
        assert_eq!(findings, vec![NEWER_REDIS_FIELDS], "{build}");
        assert!(unexplained.is_empty(), "{build}: {unexplained:?}");
    }
    // a build that isn't listed gets no explanation
    let (_, unexplained) = explain(
        "c0ffee0",
        vec![
            field("scoped_tokens"),
            missing("restored", "netpols:*", "netpols:group-1"),
        ],
        &[],
    );
    assert_eq!(
        unexplained.len(),
        2,
        "an unlisted build's difference was explained"
    );
    // another family missing after the restore isn't explained
    let (_, unexplained) = explain(
        "4e405bb",
        vec![
            missing("restored", "user_data:*", "user_data:user-1"),
            missing("source", "groups_data:*", "groups_data:group-1"),
        ],
        &[],
    );
    assert_eq!(
        unexplained.len(),
        2,
        "another family's difference was explained"
    );
    // a set the restore wrote differently, and a field the older backup holds, stay unexplained
    let differing = CompatDifference {
        place: "restored netpols:*".to_owned(),
        text: "[netpols:*] redis netpols:group-1: expected Set({\"policy-1\", \"policy-2\"}), \
               found Set({\"policy-1\"})"
            .to_owned(),
    };
    let held = CompatDifference {
        place: "redis.json".to_owned(),
        text: ".scoped_tokens[0].name: the older backup holds \"a\", this build's \"b\"".to_owned(),
    };
    let (_, unexplained) = explain("4e405bb", vec![differing, held], &[]);
    assert_eq!(
        unexplained.len(),
        2,
        "a differing set or field was explained"
    );
    // the two redis.json fields are explained once each
    let (_, unexplained) = explain(
        "4e405bb",
        vec![field("scoped_tokens"), field("scoped_tokens")],
        &[],
    );
    assert_eq!(unexplained.len(), 1, "a repeated field was explained twice");
}
