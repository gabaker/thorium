//! Generated rows of every archive struct round trip exactly through the archive writer and
//! reader, and damage to the written archives is always detected
//!
//! The `ci` matrix runs a fixed seed and a capped number of cases per struct; the `full` matrix
//! (`THORADM_TEST_MATRIX=full`) runs more cases under a random seed it prints.
//! `PROPTEST_RNG_SEED` replays a seed and `PROPTEST_CASES` overrides the case count. A failing
//! case is shrunk to a minimal one, printed, and its seed saved under `tests/proptest-regressions/`
//! so later runs replay it first.

mod common;

use chrono::{DateTime, TimeZone, Utc};
use common::classes::MatrixSize;
use common::corrupt::{flip_byte, truncate_by};
use common::enums;
use data_encoding::HEXLOWER;
use indicatif::ProgressBar;
use proptest::prelude::*;
use proptest::sample::{Index, select};
use proptest::test_runner::{
    Config, FileFailurePersistence, RngSeed, TestCaseError, TestError, TestRunner,
};
use rkyv::ser::serializers::AllocSerializer;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use thoradm::backup::coverage::{Registry, Store};
use thoradm::backup::manifest::ArchiveEntry;
use thoradm::backup::manifest::FORMAT_VERSION;
use thoradm::backup::sink::{SinkFactory, TokioFs};
use thoradm::backup::tables::{
    Association, AssociationV1, Comment, CommentV1, Commitish, CommitishList, CommitishListV1,
    CommitishV1, Entity, EntityV1, NetworkPolicy, NetworkPolicyV1, Node, NodeV1, Notification,
    NotificationV1, Output, OutputStream, OutputStreamV1, OutputV1, RepoData, RepoList, RepoListV1,
    S3Id, SamplesList, SamplesListV1, Tag, Versioned, read_rows,
};
use thoradm::backup::{ArchiveReader, ArchiveWriter, Backup, TableTally, Utils};
use thorium::models::{BurstableResources, Resources};
use uuid::Uuid;

/// The cases each struct runs in the `ci` matrix
const CI_CASES: u32 = 256;

/// The cases each struct runs in the `full` matrix
const FULL_CASES: u32 = 10_000;

/// The seed every `ci` run uses, so a failure there reproduces anywhere
const CI_SEED: u64 = 0x7468_6f72_6164_6d31;

/// The most partitions one case writes
const MAX_PARTITIONS: usize = 6;

/// The most rows one generated partition holds
const MAX_ROWS: usize = 5;

/// The smallest and largest rollover size a case writes with
const ROLLOVER: std::ops::RangeInclusive<usize> = 64..=65_536;

/// The most characters a large text value repeats, past the largest rollover size
const LARGE_TEXT: usize = 70_000;

/// The size in bytes of one archived map entry
const MAP_ENTRY_LEN: u64 = 96;

/// Text that is short but unusual: multi-byte UTF-8, combining marks, right-to-left text, NUL,
/// and a byte order mark
const UNICODE_EDGES: &[&str] = &[
    "é",
    "€",
    "😀",
    "e\u{301}",
    "\u{5d0}\u{5d1}\u{5d2}",
    "\0",
    "a\0b",
    "\u{feff}",
    "日本語のテキスト",
];

/// Counts scratch directories so concurrent cases never share one
static SCRATCH_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A scratch archive directory that is removed when the case ends, passing or failing
struct Scratch {
    /// The directory holding `data/` and `maps/`
    root: PathBuf,
}

impl Scratch {
    /// Creates a fresh directory with empty `data/` and `maps/` directories
    ///
    /// # Arguments
    ///
    /// * `table` - The table the case writes, to name the directory
    fn new(table: &str) -> Self {
        // name the directory by table, process, and case so no two cases share it
        let case = SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "thoradm-archive-property-{table}-{}-{case}",
            std::process::id()
        ));
        // start from nothing, then create both archive directories
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::create_dir_all(root.join("maps")).unwrap();
        Scratch { root }
    }

    /// Returns the data file of an archive
    ///
    /// # Arguments
    ///
    /// * `uuid` - The archive's uuid
    fn data(&self, uuid: Uuid) -> PathBuf {
        // data files are named by their uuid alone
        self.root.join("data").join(uuid.to_string())
    }

    /// Returns the map file of an archive
    ///
    /// # Arguments
    ///
    /// * `uuid` - The archive's uuid
    fn map(&self, uuid: Uuid) -> PathBuf {
        // map files carry the map extension
        self.root.join("maps").join(format!("{uuid}.thoriummap"))
    }
}

impl Drop for Scratch {
    /// Removes the scratch directory
    fn drop(&mut self) {
        // a directory that is already gone needs nothing
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Which file of an archive a case damages
#[derive(Debug, Clone, Copy)]
enum DamagedFile {
    /// The data file holding the partitions
    Data,
    /// The map file listing the partitions
    Map,
}

/// How a case damages a file
#[derive(Debug, Clone, Copy)]
enum DamageKind {
    /// Every bit of one byte is flipped
    Flip,
    /// Bytes are cut off the file's end
    Truncate,
}

/// The damage one case does to one of the archives it wrote
#[derive(Debug, Clone)]
struct Damage {
    /// Which file is damaged
    file: DamagedFile,
    /// How it is damaged
    kind: DamageKind,
    /// Picks the archive among those written
    archive: Index,
    /// Picks the byte to flip, or how many bytes to cut (one to the whole file)
    at: Index,
}

/// One generated case: the partitions to write, how to write them, and the damage to do after
#[derive(Debug)]
struct Case<T> {
    /// The partitions to write, in order; each holds at least one row
    partitions: Vec<Vec<T>>,
    /// The data file length at which the writer closes an archive and starts another
    rollover: usize,
    /// Whether pending partitions are written out after each partition, cycled over the
    /// partitions; the writer also writes once enough bytes are pending
    flushes: Vec<bool>,
    /// The damage done once the round trip is checked
    damage: Damage,
}

/// What reading a set of archives back noticed
#[derive(Debug, Default)]
struct Detected {
    /// The first error the archive reader or the row validation returned
    read_error: Option<String>,
    /// How many partitions' bytes didn't match their map entry's sha256
    checksum_mismatches: usize,
    /// Whether a map file's sha256 differs from the one the writer listed
    map_sha256_differs: bool,
    /// Whether a map holds fewer whole entries than the writer listed
    entries_missing: bool,
    /// Whether a data file's length differs from the one the writer listed
    data_len_differs: bool,
}

impl Detected {
    /// Returns whether anything was noticed
    fn any(&self) -> bool {
        // any one of the checks is enough
        self.read_error.is_some()
            || self.checksum_mismatches > 0
            || self.map_sha256_differs
            || self.entries_missing
            || self.data_len_differs
    }
}

/// Text weighted toward its value classes: empty, short unusual Unicode, arbitrary, and large
fn text() -> BoxedStrategy<String> {
    // the edges are picked as often as arbitrary text
    prop_oneof![
        2 => Just(String::new()),
        2 => select(UNICODE_EDGES).prop_map(str::to_owned),
        4 => any::<String>(),
        1 => (1..=LARGE_TEXT, any::<char>()).prop_map(|(len, ch)| ch.to_string().repeat(len)),
    ]
    .boxed()
}

/// Optional text: unset, or any text value
fn opt_text() -> BoxedStrategy<Option<String>> {
    // unset a quarter of the time
    prop::option::weighted(0.75, text()).boxed()
}

/// 32-bit integers weighted toward zero, minus one, and the type's limits
fn int() -> BoxedStrategy<i32> {
    // the edges are picked as often as arbitrary values
    prop_oneof![
        Just(0),
        Just(-1),
        Just(i32::MIN),
        Just(i32::MAX),
        any::<i32>(),
        any::<i32>(),
        any::<i32>(),
        any::<i32>(),
    ]
    .boxed()
}

/// Unsigned 64-bit integers weighted toward zero, the limit, and either side of `i64::MAX`
fn unsigned() -> BoxedStrategy<u64> {
    // i64::MAX and one past it straddle a conversion to a signed column
    let signed_max = u64::try_from(i64::MAX).unwrap();
    prop_oneof![
        Just(0),
        Just(u64::MAX),
        Just(signed_max),
        Just(signed_max + 1),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
    ]
    .boxed()
}

/// Timestamps weighted toward the epoch, a time before it, chrono's limits, and a leap day
fn timestamp() -> BoxedStrategy<DateTime<Utc>> {
    // arbitrary times anywhere chrono holds, to the nanosecond
    let (first, last) = (
        DateTime::<Utc>::MIN_UTC.timestamp(),
        DateTime::<Utc>::MAX_UTC.timestamp(),
    );
    let arbitrary = (first..=last, 0..1_000_000_000_u32).prop_map(|(secs, nanos)| {
        DateTime::from_timestamp(secs, nanos).unwrap_or(DateTime::<Utc>::MIN_UTC)
    });
    // the edges are picked about as often as arbitrary times
    prop_oneof![
        1 => Just(DateTime::<Utc>::UNIX_EPOCH),
        1 => Just(DateTime::from_timestamp_millis(-1).unwrap()),
        1 => Just(DateTime::<Utc>::MIN_UTC),
        1 => Just(DateTime::<Utc>::MAX_UTC),
        1 => Just(Utc.with_ymd_and_hms(2024, 2, 29, 12, 0, 0).unwrap()),
        4 => arbitrary,
    ]
    .boxed()
}

/// Uuids weighted toward the nil value and all bits set
fn uuid() -> BoxedStrategy<Uuid> {
    // the edges a quarter of the time each
    prop_oneof![
        1 => Just(Uuid::nil()),
        1 => Just(Uuid::max()),
        2 => any::<u128>().prop_map(Uuid::from_u128),
    ]
    .boxed()
}

/// Optional uuids: unset, or any uuid
fn opt_uuid() -> BoxedStrategy<Option<Uuid>> {
    // unset a quarter of the time
    prop::option::weighted(0.75, uuid()).boxed()
}

/// Generated `associations` rows as format 1 archives them
fn associations_v1() -> BoxedStrategy<AssociationV1> {
    // every column from its type's classes, every enum variant picked alike
    (
        text(),
        int(),
        int(),
        timestamp(),
        select(enums::directionalities()),
        select(enums::association_kinds()),
        text(),
        text(),
        text(),
        opt_text(),
        opt_text(),
    )
        .prop_map(
            |(
                group,
                year,
                bucket,
                created,
                direction,
                kind,
                source,
                other,
                submitter,
                extra_source,
                extra_other,
            )| AssociationV1 {
                group,
                year,
                bucket,
                created,
                direction,
                kind,
                source,
                other,
                submitter,
                extra_source,
                extra_other,
            },
        )
        .boxed()
}

/// Generated `comments` rows as format 1 archives them
fn comments_v1() -> BoxedStrategy<CommentV1> {
    // every column from its type's classes
    (text(), text(), timestamp(), uuid(), text(), text(), text())
        .prop_map(
            |(group, sha256, uploaded, id, author, comment, files)| CommentV1 {
                group,
                sha256,
                uploaded,
                id,
                author,
                comment,
                files,
            },
        )
        .boxed()
}

/// Generated `commitishes` rows as format 1 archives them
fn commitishes_v1() -> BoxedStrategy<CommitishV1> {
    // every column from its type's classes, every enum variant picked alike
    (
        select(enums::commitish_kinds()),
        text(),
        text(),
        text(),
        text(),
        text(),
        timestamp(),
    )
        .prop_map(
            |(kind, group, repo, key, data, repo_data, timestamp)| CommitishV1 {
                kind,
                group,
                repo,
                key,
                data,
                repo_data,
                timestamp,
            },
        )
        .boxed()
}

/// Generated `commitish_list` rows as format 1 archives them
fn commitish_lists_v1() -> BoxedStrategy<CommitishListV1> {
    // every column from its type's classes, every enum variant picked alike
    (
        select(enums::commitish_kinds()),
        text(),
        int(),
        int(),
        text(),
        timestamp(),
        text(),
        text(),
    )
        .prop_map(
            |(kind, group, year, bucket, repo, timestamp, key, repo_data)| CommitishListV1 {
                kind,
                group,
                year,
                bucket,
                repo,
                timestamp,
                key,
                repo_data,
            },
        )
        .boxed()
}

/// Generated `entities` rows as format 1 archives them
fn entities_v1() -> BoxedStrategy<EntityV1> {
    // every column from its type's classes, every enum variant picked alike
    (
        select(enums::entity_kinds()),
        text(),
        int(),
        int(),
        timestamp(),
        uuid(),
        text(),
        text(),
        text(),
        opt_text(),
        opt_text(),
    )
        .prop_map(
            |(
                kind,
                group,
                year,
                bucket,
                created,
                id,
                name,
                submitter,
                kind_data,
                description,
                image,
            )| EntityV1 {
                kind,
                group,
                year,
                bucket,
                created,
                id,
                name,
                submitter,
                kind_data,
                description,
                image,
            },
        )
        .boxed()
}

/// Generated `network_policies` rows as format 1 archives them
fn network_policies_v1() -> BoxedStrategy<NetworkPolicyV1> {
    // every column from its type's classes
    (
        text(),
        text(),
        uuid(),
        text(),
        timestamp(),
        text(),
        text(),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(
            |(
                group,
                name,
                id,
                k8s_name,
                created,
                ingress,
                egress,
                forced_policy,
                default_policy,
            )| {
                NetworkPolicyV1 {
                    group,
                    name,
                    id,
                    k8s_name,
                    created,
                    ingress,
                    egress,
                    forced_policy,
                    default_policy,
                }
            },
        )
        .boxed()
}

/// Generated `nodes` rows as format 1 archives them
fn nodes_v1() -> BoxedStrategy<NodeV1> {
    // the resources are seven unsigned counts
    let resources = (
        unsigned(),
        unsigned(),
        unsigned(),
        unsigned(),
        unsigned(),
        unsigned(),
        unsigned(),
        unsigned(),
    )
        .prop_map(
            |(
                cpu,
                memory,
                ephemeral_storage,
                worker_slots,
                nvidia_gpu,
                amd_gpu,
                burst_cpu,
                burst_memory,
            )| {
                Resources {
                    cpu,
                    memory,
                    ephemeral_storage,
                    worker_slots,
                    nvidia_gpu,
                    amd_gpu,
                    burstable: BurstableResources {
                        cpu: burst_cpu,
                        memory: burst_memory,
                    },
                }
            },
        );
    // every column from its type's classes, every health variant picked alike
    (
        text(),
        text(),
        select(enums::node_healths()),
        prop::option::weighted(0.75, timestamp()),
        resources,
    )
        .prop_map(|(cluster, node, health, heart_beat, resources)| NodeV1 {
            cluster,
            node,
            health,
            heart_beat,
            resources,
        })
        .boxed()
}

/// Generated `notifications` rows as format 1 archives them
fn notifications_v1() -> BoxedStrategy<NotificationV1> {
    // every column from its type's classes, every enum variant picked alike
    (
        select(enums::notification_types()),
        text(),
        timestamp(),
        uuid(),
        text(),
        select(enums::notification_levels()),
        opt_uuid(),
    )
        .prop_map(
            |(kind, key, created, id, msg, level, ban_id)| NotificationV1 {
                kind,
                key,
                created,
                id,
                msg,
                level,
                ban_id,
            },
        )
        .boxed()
}

/// Generated `repos_list` rows as format 1 archives them
fn repo_lists_v1() -> BoxedStrategy<RepoListV1> {
    // every column from its type's classes
    (
        text(),
        int(),
        int(),
        timestamp(),
        uuid(),
        text(),
        text(),
        text(),
        text(),
        text(),
        text(),
    )
        .prop_map(
            |(group, year, bucket, uploaded, id, creator, name, provider, scheme, url, user)| {
                RepoListV1 {
                    group,
                    year,
                    bucket,
                    uploaded,
                    id,
                    creator,
                    name,
                    provider,
                    scheme,
                    url,
                    user,
                }
            },
        )
        .boxed()
}

/// Generated `repo_data` rows
fn repo_data() -> BoxedStrategy<RepoData> {
    // both columns are text
    (text(), text())
        .prop_map(|(repo, hash)| RepoData { repo, hash })
        .boxed()
}

/// Generated `results` rows as format 1 archives them
fn outputs_v1() -> BoxedStrategy<OutputV1> {
    // the file list and children are unset, empty, or hold text keys of every class
    let files = prop::option::weighted(0.75, prop::collection::vec(text(), 0..4));
    let children = prop::option::weighted(0.75, prop::collection::hash_map(text(), uuid(), 0..4));
    // every column from its type's classes, every display type picked alike
    (
        uuid(),
        text(),
        opt_text(),
        timestamp(),
        text(),
        files,
        select(enums::output_display_types()),
        children,
    )
        .prop_map(
            |(id, tool, cmd, uploaded, result, files, display_type, children)| OutputV1 {
                id,
                tool,
                cmd,
                uploaded,
                result,
                files,
                display_type,
                children,
            },
        )
        .boxed()
}

/// Generated `results_stream` rows as format 1 archives them
fn output_streams_v1() -> BoxedStrategy<OutputStreamV1> {
    // every column from its type's classes, every display type picked alike
    (
        text(),
        text(),
        int(),
        int(),
        text(),
        text(),
        select(enums::output_display_types()),
        timestamp(),
        opt_text(),
        uuid(),
    )
        .prop_map(
            |(kind, group, year, bucket, key, tool, display_type, uploaded, cmd, id)| {
                OutputStreamV1 {
                    kind,
                    group,
                    year,
                    bucket,
                    key,
                    tool,
                    display_type,
                    uploaded,
                    cmd,
                    id,
                }
            },
        )
        .boxed()
}

/// Generated `s3_ids` rows
fn s3_ids() -> BoxedStrategy<S3Id> {
    // every column from its type's classes, every object type picked alike
    (select(enums::s3_objects()), uuid(), text())
        .prop_map(|(object_type, id, sha256)| S3Id {
            object_type,
            id,
            sha256,
        })
        .boxed()
}

/// Generated `samples_list` rows as format 1 archives them
fn samples_lists_v1() -> BoxedStrategy<SamplesListV1> {
    // every column from its type's classes
    (
        text(),
        int(),
        int(),
        text(),
        text(),
        text(),
        uuid(),
        opt_text(),
        opt_text(),
        text(),
        opt_text(),
        timestamp(),
    )
        .prop_map(
            |(
                group,
                year,
                bucket,
                sha256,
                sha1,
                md5,
                id,
                name,
                description,
                submitter,
                origin,
                uploaded,
            )| SamplesListV1 {
                group,
                year,
                bucket,
                sha256,
                sha1,
                md5,
                id,
                name,
                description,
                submitter,
                origin,
                uploaded,
            },
        )
        .boxed()
}

/// Generated `tags` rows
fn tags() -> BoxedStrategy<Tag> {
    // every column from its type's classes, every tag type picked alike
    (
        select(enums::tag_types()),
        text(),
        int(),
        int(),
        text(),
        text(),
        timestamp(),
        text(),
    )
        .prop_map(
            |(tag_type, group, year, bucket, key, value, uploaded, item)| Tag {
                tag_type,
                group,
                year,
                bucket,
                key,
                value,
                uploaded,
                item,
            },
        )
        .boxed()
}

/// Whether a generated format-2 field that format 1 always held is left unset
fn unset() -> BoxedStrategy<bool> {
    // a quarter of the time
    prop::bool::weighted(0.25).boxed()
}

/// Generated `associations` rows as format 2 archives them: format-1 rows with their required
/// fields unset at times and their submissions generated
fn associations() -> BoxedStrategy<Association> {
    // the format-1 row, which fields to unset, and the added columns
    (associations_v1(), unset(), unset(), opt_uuid(), opt_uuid())
        .prop_map(|(row, no_kind, no_submitter, source, other)| {
            let mut row = Association::from(row);
            row.kind = row.kind.filter(|_| !no_kind);
            row.submitter = row.submitter.filter(|_| !no_submitter);
            row.submissions_source = source;
            row.submissions_other = other;
            row
        })
        .boxed()
}

/// Generated `comments` rows as format 2 archives them: format-1 rows with their required fields
/// unset at times
fn comments() -> BoxedStrategy<Comment> {
    // the format-1 row and which fields to unset
    (comments_v1(), unset(), unset(), unset())
        .prop_map(|(row, no_author, no_comment, no_files)| {
            let mut row = Comment::from(row);
            row.author = row.author.filter(|_| !no_author);
            row.comment = row.comment.filter(|_| !no_comment);
            row.files = row.files.filter(|_| !no_files);
            row
        })
        .boxed()
}

/// Generated `commitishes` rows as format 2 archives them: format-1 rows with their required
/// fields unset at times
fn commitishes() -> BoxedStrategy<Commitish> {
    // the format-1 row and which fields to unset
    (commitishes_v1(), unset(), unset(), unset())
        .prop_map(|(row, no_data, no_repo_data, no_timestamp)| {
            let mut row = Commitish::from(row);
            row.data = row.data.filter(|_| !no_data);
            row.repo_data = row.repo_data.filter(|_| !no_repo_data);
            row.timestamp = row.timestamp.filter(|_| !no_timestamp);
            row
        })
        .boxed()
}

/// Generated `commitish_list` rows as format 2 archives them: format-1 rows with their required
/// fields unset at times
fn commitish_lists() -> BoxedStrategy<CommitishList> {
    // the format-1 row and whether to unset its repo data
    (commitish_lists_v1(), unset())
        .prop_map(|(row, no_repo_data)| {
            let mut row = CommitishList::from(row);
            row.repo_data = row.repo_data.filter(|_| !no_repo_data);
            row
        })
        .boxed()
}

/// Generated `entities` rows as format 2 archives them: format-1 rows with their required fields
/// unset at times
fn entities() -> BoxedStrategy<Entity> {
    // the format-1 row and which fields to unset
    (entities_v1(), unset(), unset(), unset())
        .prop_map(|(row, no_name, no_submitter, no_kind_data)| {
            let mut row = Entity::from(row);
            row.name = row.name.filter(|_| !no_name);
            row.submitter = row.submitter.filter(|_| !no_submitter);
            row.kind_data = row.kind_data.filter(|_| !no_kind_data);
            row
        })
        .boxed()
}

/// Generated `network_policies` rows as format 2 archives them: format-1 rows with their required
/// fields unset at times
fn network_policies() -> BoxedStrategy<NetworkPolicy> {
    // the format-1 row and which fields to unset
    let unsets = prop::collection::vec(unset(), 7);
    (network_policies_v1(), unsets)
        .prop_map(|(row, no)| {
            let mut row = NetworkPolicy::from(row);
            row.id = row.id.filter(|_| !no[0]);
            row.k8s_name = row.k8s_name.filter(|_| !no[1]);
            row.created = row.created.filter(|_| !no[2]);
            row.ingress = row.ingress.filter(|_| !no[3]);
            row.egress = row.egress.filter(|_| !no[4]);
            row.forced_policy = row.forced_policy.filter(|_| !no[5]);
            row.default_policy = row.default_policy.filter(|_| !no[6]);
            row
        })
        .boxed()
}

/// Generated `nodes` rows as format 2 archives them: format-1 rows with their required fields
/// unset at times
fn nodes() -> BoxedStrategy<Node> {
    // the format-1 row and which fields to unset
    (nodes_v1(), unset(), unset())
        .prop_map(|(row, no_health, no_resources)| {
            let mut row = Node::from(row);
            row.health = row.health.filter(|_| !no_health);
            row.resources = row.resources.filter(|_| !no_resources);
            row
        })
        .boxed()
}

/// Generated `notifications` rows as format 2 archives them: format-1 rows with their required
/// fields unset at times and their remaining TTL generated
fn notifications() -> BoxedStrategy<Notification> {
    // the format-1 row, which fields to unset, and the TTL
    let ttl = prop::option::weighted(0.75, prop_oneof![Just(1), Just(i32::MAX), 1..i32::MAX]);
    (notifications_v1(), unset(), unset(), ttl)
        .prop_map(|(row, no_msg, no_level, ttl)| {
            let mut row = Notification::from(row);
            row.msg = row.msg.filter(|_| !no_msg);
            row.level = row.level.filter(|_| !no_level);
            row.ttl = ttl;
            row
        })
        .boxed()
}

/// Generated `repos_list` rows as format 2 archives them: format-1 rows with their required
/// fields unset at times and their default checkout and earliest commit generated
fn repo_lists() -> BoxedStrategy<RepoList> {
    // the format-1 row, which fields to unset, and the added columns
    let unsets = prop::collection::vec(unset(), 6);
    let earliest = prop::option::weighted(0.75, timestamp());
    (repo_lists_v1(), unsets, opt_text(), earliest)
        .prop_map(|(row, no, default_checkout, earliest)| {
            let mut row = RepoList::from(row);
            row.creator = row.creator.filter(|_| !no[0]);
            row.name = row.name.filter(|_| !no[1]);
            row.provider = row.provider.filter(|_| !no[2]);
            row.scheme = row.scheme.filter(|_| !no[3]);
            row.url = row.url.filter(|_| !no[4]);
            row.user = row.user.filter(|_| !no[5]);
            row.default_checkout = default_checkout;
            row.earliest = earliest;
            row
        })
        .boxed()
}

/// Generated `results` rows as format 2 archives them: format-1 rows with their required fields
/// unset at times and their tool version and entity counts generated
fn outputs() -> BoxedStrategy<Output> {
    // the format-1 row, which fields to unset, and the added columns
    let entities =
        prop::option::weighted(0.75, prop::collection::hash_map(text(), any::<i64>(), 0..4));
    (
        outputs_v1(),
        unset(),
        unset(),
        unset(),
        opt_text(),
        entities,
    )
        .prop_map(
            |(row, no_tool, no_result, no_display, tool_version, entities)| {
                let mut row = Output::from(row);
                row.tool = row.tool.filter(|_| !no_tool);
                row.result = row.result.filter(|_| !no_result);
                row.display_type = row.display_type.filter(|_| !no_display);
                row.tool_version = tool_version;
                row.entities = entities;
                row
            },
        )
        .boxed()
}

/// Generated `results_stream` rows as format 2 archives them: format-1 rows with their required
/// fields unset at times and their tool version generated
fn output_streams() -> BoxedStrategy<OutputStream> {
    // the format-1 row, which fields to unset, and the added column
    (output_streams_v1(), unset(), unset(), opt_text())
        .prop_map(|(row, no_tool, no_display, tool_version)| {
            let mut row = OutputStream::from(row);
            row.tool = row.tool.filter(|_| !no_tool);
            row.display_type = row.display_type.filter(|_| !no_display);
            row.tool_version = tool_version;
            row
        })
        .boxed()
}

/// Generated `samples_list` rows as format 2 archives them: format-1 rows with their required
/// fields unset at times
fn samples_lists() -> BoxedStrategy<SamplesList> {
    // the format-1 row and which fields to unset
    (samples_lists_v1(), unset(), unset(), unset(), unset())
        .prop_map(|(row, no_sha256, no_sha1, no_md5, no_submitter)| {
            let mut row = SamplesList::from(row);
            row.sha256 = row.sha256.filter(|_| !no_sha256);
            row.sha1 = row.sha1.filter(|_| !no_sha1);
            row.md5 = row.md5.filter(|_| !no_md5);
            row.submitter = row.submitter.filter(|_| !no_submitter);
            row
        })
        .boxed()
}

/// Renders a row with every field for comparison
///
/// # Arguments
///
/// * `row` - The row to render
fn debug_form<T: Debug>(row: &T) -> String {
    // the derived debug form names every field and its value
    format!("{row:?}")
}

/// Renders a `results` row with every field for comparison, with its children in key order
///
/// # Arguments
///
/// * `row` - The row to render
fn output_form(row: &Output) -> String {
    // a hash map's debug order depends on its hasher, so sort the children and entities first
    let children = row
        .children
        .as_ref()
        .map(|children| children.iter().collect::<BTreeMap<_, _>>());
    let entities = row
        .entities
        .as_ref()
        .map(|entities| entities.iter().collect::<BTreeMap<_, _>>());
    format!(
        "Output {{ id: {:?}, tool: {:?}, tool_version: {:?}, cmd: {:?}, uploaded: {:?}, \
         result: {:?}, files: {:?}, display_type: {:?}, children: {children:?}, \
         entities: {entities:?} }}",
        row.id,
        row.tool,
        row.tool_version,
        row.cmd,
        row.uploaded,
        row.result,
        row.files,
        row.display_type
    )
}

/// Builds the strategy for one case of a struct
///
/// # Arguments
///
/// * `rows` - The strategy for one row
fn cases<T: Debug + 'static>(rows: BoxedStrategy<T>) -> impl Strategy<Value = Case<T>> {
    // each partition holds at least one row, as the backup worker never archives an empty one
    let partitions = prop::collection::vec(
        prop::collection::vec(rows, 1..=MAX_ROWS),
        1..=MAX_PARTITIONS,
    );
    // the damage names a file, a kind, an archive, and a position
    let damage = (
        prop_oneof![Just(DamagedFile::Data), Just(DamagedFile::Map)],
        prop_oneof![Just(DamageKind::Flip), Just(DamageKind::Truncate)],
        any::<Index>(),
        any::<Index>(),
    )
        .prop_map(|(file, kind, archive, at)| Damage {
            file,
            kind,
            archive,
            at,
        });
    // a rollover size and a flush pattern decide where archives split
    (
        partitions,
        ROLLOVER,
        prop::collection::vec(any::<bool>(), 1..=MAX_PARTITIONS),
        damage,
    )
        .prop_map(|(partitions, rollover, flushes, damage)| Case {
            partitions,
            rollover,
            flushes,
            damage,
        })
}

/// Writes a case's partitions through the archive writer as a backup worker does, returning
/// the archives the writer listed
///
/// # Arguments
///
/// * `scratch` - The directory to write into
/// * `case` - The case to write
/// * `hash` - Hashes a row's partition key, as the backup worker does
async fn write<W>(scratch: &Scratch, case: &Case<W>, hash: &dyn Fn(&W) -> u64) -> Vec<ArchiveEntry>
where
    W: rkyv::Serialize<AllocSerializer<1024>>,
{
    // a writer with the case's rollover size that lists its archives in a tally
    let sinks: Arc<dyn SinkFactory> = Arc::new(TokioFs);
    let tally = Arc::new(Mutex::new(TableTally::default()));
    let mut writer = ArchiveWriter::new(
        &scratch.root.join("data"),
        &scratch.root.join("maps"),
        ProgressBar::hidden(),
        &sinks,
    )
    .with_rollover(case.rollover)
    .with_tally(&tally);
    let (mut updates, _receiver) = kanal::unbounded_async();
    // each partition is archived, hashed, and added as the backup worker does
    for (index, partition) in case.partitions.iter().enumerate() {
        let bytes = rkyv::to_bytes::<_, 1024>(partition).unwrap();
        let sha256 = HEXLOWER.encode(&Sha256::digest(&bytes));
        let ready = writer.add(hash(&partition[0]), partition.len(), sha256, bytes);
        // pending partitions are written when enough bytes wait or the pattern says so, and a
        // full archive is closed so the next write starts another
        if ready || case.flushes[index % case.flushes.len()] {
            writer.archive(&mut updates).await.unwrap();
            if writer.full() {
                writer.close().await.unwrap();
            }
        }
    }
    // the rest is written and the last archive closed, as a worker does when it finishes
    writer.archive(&mut updates).await.unwrap();
    writer.close().await.unwrap();
    tally.lock().unwrap().archives.clone()
}

/// Returns the lowercase hex sha256 of a file
///
/// # Arguments
///
/// * `path` - The file to hash
fn sha256_of(path: &Path) -> String {
    // hash the whole file
    HEXLOWER.encode(&Sha256::digest(std::fs::read(path).unwrap()))
}

/// Reads every listed archive back, checking each against what the writer listed and each
/// partition against its checksum, and returns the rows of the partitions that validated
///
/// # Arguments
///
/// * `scratch` - The directory the archives are in
/// * `archives` - The archives the writer listed, in order
/// * `format` - The format version the rows were archived in, which the reader dispatches on
async fn read_back<T: Versioned>(
    scratch: &Scratch,
    archives: &[ArchiveEntry],
    format: u32,
) -> (Vec<Vec<T>>, Detected) {
    // nothing is noticed and no rows are read until the archives are walked
    let mut detected = Detected::default();
    let mut partitions = Vec::new();
    for entry in archives {
        // the files must still be what the writer listed
        let (data, map) = (scratch.data(entry.uuid), scratch.map(entry.uuid));
        let map_len = std::fs::metadata(&map).unwrap().len();
        detected.map_sha256_differs |= sha256_of(&map) != entry.map_sha256;
        detected.entries_missing |= map_len / MAP_ENTRY_LEN < entry.entries;
        detected.data_len_differs |= std::fs::metadata(&data).unwrap().len() != entry.data_len;
        // read each partition through the archive reader
        let mut reader = match ArchiveReader::new(map.clone()).await {
            Ok(reader) => reader,
            Err(error) => {
                detected.read_error.get_or_insert(error.to_string());
                continue;
            }
        };
        // walk the partitions until the map ends or the reader refuses one
        loop {
            let (bytes, partition) = match reader.next_partition().await {
                Ok(Some(next)) => next,
                Ok(None) => break,
                Err(error) => {
                    detected.read_error.get_or_insert(error.to_string());
                    break;
                }
            };
            // a partition whose bytes changed no longer matches its checksum
            if HEXLOWER.encode(&Sha256::digest(bytes)) != partition.sha256 {
                detected.checksum_mismatches += 1;
                continue;
            }
            // the bytes validate as the format's archived rows and read back as the current
            // struct, field for field
            match read_rows::<T>(bytes, format) {
                Ok(rows) => partitions.push(rows),
                Err(error) => {
                    detected.read_error.get_or_insert(error.to_string());
                }
            }
        }
    }
    (partitions, detected)
}

/// Writes one case, checks it reads back exactly, then damages it and checks the damage is
/// detected by the check that owns it
///
/// # Arguments
///
/// * `table` - The table the struct backs up
/// * `case` - The case to run
/// * `written` - How the case's rows were archived: their format version, how their partition
///   key hashes, and how each must read back
/// * `form` - Renders a read row with every field for comparison
async fn run_case<W, T: Versioned>(
    table: &str,
    case: &Case<W>,
    written: &Written<'_, W>,
    form: fn(&T) -> String,
) -> Result<(), TestCaseError>
where
    W: rkyv::Serialize<AllocSerializer<1024>>,
{
    // write the case into a fresh directory
    let scratch = Scratch::new(table);
    let archives = write(&scratch, case, written.hash).await;
    prop_assert!(!archives.is_empty(), "the writer listed no archive");
    // the listing accounts for every partition and row, and every archive holds data
    let partitions: u64 = archives.iter().map(|entry| entry.entries).sum();
    let rows: u64 = archives.iter().map(|entry| entry.rows).sum();
    let expected_rows: usize = case.partitions.iter().map(Vec::len).sum();
    prop_assert_eq!(partitions, case.partitions.len() as u64);
    prop_assert_eq!(rows, expected_rows as u64);
    prop_assert!(
        archives
            .iter()
            .all(|entry| entry.entries > 0 && entry.data_len > 0)
    );
    // every archive but the last was closed because it was full
    for entry in &archives[..archives.len() - 1] {
        prop_assert!(
            entry.data_len >= case.rollover as u64,
            "archive {} was closed at {} bytes, under the rollover size {}",
            entry.uuid,
            entry.data_len,
            case.rollover
        );
    }
    // the undamaged archives read back clean, in order, field for field
    let (read, detected) = read_back::<T>(&scratch, &archives, written.format).await;
    prop_assert!(
        !detected.any(),
        "undamaged archives read as damaged: {detected:?}"
    );
    let expected: Vec<Vec<String>> = case
        .partitions
        .iter()
        .map(|rows| rows.iter().map(|row| (written.expect)(row)).collect())
        .collect();
    let read: Vec<Vec<String>> = read
        .iter()
        .map(|rows| rows.iter().map(form).collect())
        .collect();
    prop_assert_eq!(read, expected);
    // damage one file of one archive
    let entry = &archives[case.damage.archive.index(archives.len())];
    let path = match case.damage.file {
        DamagedFile::Data => scratch.data(entry.uuid),
        DamagedFile::Map => scratch.map(entry.uuid),
    };
    let len = std::fs::metadata(&path).unwrap().len();
    let at = case.damage.at.index(usize::try_from(len).unwrap()) as u64;
    match case.damage.kind {
        DamageKind::Flip => flip_byte(&path, at).unwrap(),
        DamageKind::Truncate => truncate_by(&path, at + 1).unwrap(),
    }
    // the check that owns each kind of damage notices it
    let (_, detected) = read_back::<T>(&scratch, &archives, written.format).await;
    match (case.damage.file, case.damage.kind) {
        (DamagedFile::Data, DamageKind::Flip) => {
            // a changed data byte belongs to exactly one partition, whose checksum then differs
            prop_assert_eq!(detected.checksum_mismatches, 1, "{:?}", detected);
        }
        (DamagedFile::Data, DamageKind::Truncate) => {
            // the reader refuses a partition past the end of the data file
            let error = detected.read_error.clone().unwrap_or_default();
            prop_assert!(
                error.contains(&path.display().to_string()) && detected.data_len_differs,
                "{:?}",
                detected
            );
        }
        (DamagedFile::Map, DamageKind::Flip) => {
            // any changed map byte changes the map's listed sha256
            prop_assert!(detected.map_sha256_differs, "{:?}", detected);
        }
        (DamagedFile::Map, DamageKind::Truncate) => {
            // a cut inside an entry is refused by the reader; a cut at an entry boundary leaves
            // fewer entries than the writer listed
            let cut = at + 1;
            if cut.is_multiple_of(MAP_ENTRY_LEN) {
                prop_assert!(detected.entries_missing, "{:?}", detected);
            } else {
                let error = detected.read_error.clone().unwrap_or_default();
                prop_assert!(
                    error.contains(&path.display().to_string()),
                    "{:?}",
                    detected
                );
            }
            prop_assert!(detected.map_sha256_differs, "{:?}", detected);
        }
    }
    Ok(())
}

/// Builds the runner config: the case count and seed for the matrix size, and the file failing
/// seeds are saved to
///
/// # Arguments
///
/// * `table` - The table the struct backs up, naming its regressions file
fn config(table: &str) -> (Config, u64) {
    // the defaults carry any PROPTEST_* overrides from the environment
    let defaults = Config::default();
    let matrix = MatrixSize::from_env();
    // an explicit seed wins, then the matrix decides between the fixed seed and a random one
    let seed = match defaults.rng_seed {
        RngSeed::Fixed(seed) => seed,
        RngSeed::Random => match matrix {
            MatrixSize::Ci => CI_SEED,
            MatrixSize::Full => rand::random(),
        },
    };
    // an explicit case count wins, then the matrix's count
    let cases = if std::env::var_os("PROPTEST_CASES").is_some() {
        defaults.cases
    } else {
        match matrix {
            MatrixSize::Ci => CI_CASES,
            MatrixSize::Full => FULL_CASES,
        }
    };
    // failing seeds are saved per table, beside this test; cargo runs tests from the crate root
    let path: &'static str = Box::leak(
        format!("tests/proptest-regressions/archive_property-{table}.txt").into_boxed_str(),
    );
    let config = Config {
        cases,
        rng_seed: RngSeed::Fixed(seed),
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(path))),
        ..defaults
    };
    (config, seed)
}

/// How a case's rows were archived and how each must read back
struct Written<'a, W> {
    /// The format version the rows were archived in
    format: u32,
    /// Hashes a row's partition key, as the backup worker does
    hash: &'a dyn Fn(&W) -> u64,
    /// Renders a written row as it must read back
    expect: &'a dyn Fn(&W) -> String,
}

/// Runs a property over generated cases, panicking with the seed and the minimal failing case
/// when it fails
///
/// # Arguments
///
/// * `name` - The property's name, which names its regressions file
/// * `rows` - The strategy for one row
/// * `run` - Runs one case
fn run_property<W: Debug + 'static>(
    name: &str,
    rows: BoxedStrategy<W>,
    run: impl Fn(&Case<W>) -> Result<(), TestCaseError>,
) {
    // print the seed so any run can be repeated with PROPTEST_RNG_SEED
    let (config, seed) = config(name);
    eprintln!(
        "archive property for {name}: {} cases, seed {seed}",
        config.cases
    );
    let mut runner = TestRunner::new(config);
    let result = runner.run(&cases(rows), |case| run(&case));
    // a failure names the seed and prints the shrunk case
    match result {
        Ok(()) => {}
        Err(TestError::Fail(reason, case)) => {
            panic!(
                "the archive property for {name} failed under seed {seed}: {reason}\nminimal case: {case:#?}"
            )
        }
        Err(TestError::Abort(reason)) => {
            panic!("the archive property for {name} aborted under seed {seed}: {reason}")
        }
    }
}

/// Returns a runtime for one property's file work
fn runtime() -> tokio::runtime::Runtime {
    // each case runs its file work on one thread
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// Runs the property for one archive struct in the current format: generated rows read back
/// through the format dispatch as they were written
///
/// # Arguments
///
/// * `rows` - The strategy for one row
/// * `form` - Renders a row with every field for comparison
fn check<T: Backup + Versioned>(rows: BoxedStrategy<T>, form: fn(&T) -> String) {
    // rows read back exactly as written
    let table = T::name();
    let runtime = runtime();
    let hash = |row: &T| row.hash_partition();
    let expect = |row: &T| form(row);
    let written = Written {
        format: FORMAT_VERSION,
        hash: &hash,
        expect: &expect,
    };
    run_property(table, rows, |case| {
        runtime.block_on(run_case(table, case, &written, form))
    });
}

/// Runs the property for one table's format-1 archive struct: generated format-1 rows read back
/// through the format dispatch as the current struct, each equal to its conversion, which keeps
/// every value the format-1 row holds and leaves the columns format 2 added unset
///
/// # Arguments
///
/// * `rows` - The strategy for one format-1 row
/// * `converted` - Whether a current row holds a format-1 row's values, with the added columns
///   unset
/// * `form` - Renders a current row with every field for comparison
fn check_format_one<T>(
    rows: BoxedStrategy<T::V1>,
    converted: fn(&T::V1, &T) -> bool,
    form: fn(&T) -> String,
) where
    T: Backup + Versioned,
    T::V1: Clone + Debug + rkyv::Serialize<AllocSerializer<1024>> + 'static,
{
    // rows read back as their conversion, which holds what they held
    let table = format!("{}-format-1", T::name());
    let runtime = runtime();
    let hash = |row: &T::V1| -> u64 { row.clone().into().hash_partition() };
    let expect = |row: &T::V1| -> String { form(&row.clone().into()) };
    let written = Written {
        format: 1,
        hash: &hash,
        expect: &expect,
    };
    run_property(&table, rows, |case| {
        for row in case.partitions.iter().flatten() {
            let current: T = row.clone().into();
            prop_assert!(converted(row, &current), "{row:?} converts to {current:?}");
        }
        runtime.block_on(run_case(&table, case, &written, form))
    });
}

/// Every table that backs up through an archive struct has a property below
#[test]
fn every_archived_table_has_a_property() {
    // the tables the properties cover
    let covered: BTreeSet<&str> = [
        Association::name(),
        Comment::name(),
        Commitish::name(),
        CommitishList::name(),
        Entity::name(),
        NetworkPolicy::name(),
        Node::name(),
        Notification::name(),
        Output::name(),
        OutputStream::name(),
        RepoData::name(),
        RepoList::name(),
        S3Id::name(),
        SamplesList::name(),
        Tag::name(),
    ]
    .into_iter()
    .collect();
    // the registry's backed-up tables with archived columns
    let registered: BTreeSet<&str> = Registry::backed_up(Store::ScyllaTable)
        .filter(|entry| entry.columns.is_some())
        .map(|entry| entry.name)
        .collect();
    assert_eq!(covered, registered);
}

/// Generated `associations` rows round trip, and damage to their archives is detected
#[test]
fn associations_round_trip() {
    check(associations(), debug_form);
}

/// Generated `comments` rows round trip, and damage to their archives is detected
#[test]
fn comments_round_trip() {
    check(comments(), debug_form);
}

/// Generated `commitishes` rows round trip, and damage to their archives is detected
#[test]
fn commitishes_round_trip() {
    check(commitishes(), debug_form);
}

/// Generated `commitish_list` rows round trip, and damage to their archives is detected
#[test]
fn commitish_list_round_trip() {
    check(commitish_lists(), debug_form);
}

/// Generated `entities` rows round trip, and damage to their archives is detected
#[test]
fn entities_round_trip() {
    check(entities(), debug_form);
}

/// Generated `network_policies` rows round trip, and damage to their archives is detected
#[test]
fn network_policies_round_trip() {
    check(network_policies(), debug_form);
}

/// Generated `nodes` rows round trip, and damage to their archives is detected
#[test]
fn nodes_round_trip() {
    check(nodes(), debug_form);
}

/// Generated `notifications` rows round trip, and damage to their archives is detected
#[test]
fn notifications_round_trip() {
    check(notifications(), debug_form);
}

/// Generated `results` rows round trip, and damage to their archives is detected
#[test]
fn results_round_trip() {
    check(outputs(), output_form);
}

/// Generated `results_stream` rows round trip, and damage to their archives is detected
#[test]
fn results_stream_round_trip() {
    check(output_streams(), debug_form);
}

/// Generated `repo_data` rows round trip, and damage to their archives is detected
#[test]
fn repo_data_round_trip() {
    check(repo_data(), debug_form);
}

/// Generated `repos_list` rows round trip, and damage to their archives is detected
#[test]
fn repos_list_round_trip() {
    check(repo_lists(), debug_form);
}

/// Generated `s3_ids` rows round trip, and damage to their archives is detected
#[test]
fn s3_ids_round_trip() {
    check(s3_ids(), debug_form);
}

/// Generated `samples_list` rows round trip, and damage to their archives is detected
#[test]
fn samples_list_round_trip() {
    check(samples_lists(), debug_form);
}

/// Generated `tags` rows round trip, and damage to their archives is detected
#[test]
fn tags_round_trip() {
    check(tags(), debug_form);
}

/// Whether a current `associations` row holds a format-1 row's values, with its submissions unset
///
/// # Arguments
///
/// * `old` - The format-1 row
/// * `new` - Its conversion
fn association_converted(old: &AssociationV1, new: &Association) -> bool {
    // every format-1 value kept, the required ones set, and the added columns unset
    new.group == old.group
        && new.year == old.year
        && new.bucket == old.bucket
        && new.created == old.created
        && new.direction == old.direction
        && new.kind == Some(old.kind)
        && new.source == old.source
        && new.other == old.other
        && new.submitter.as_ref() == Some(&old.submitter)
        && new.extra_source == old.extra_source
        && new.extra_other == old.extra_other
        && new.submissions_source.is_none()
        && new.submissions_other.is_none()
}

/// Whether a current `comments` row holds a format-1 row's values
///
/// # Arguments
///
/// * `old` - The format-1 row
/// * `new` - Its conversion
fn comment_converted(old: &CommentV1, new: &Comment) -> bool {
    // every format-1 value kept and the required ones set
    new.group == old.group
        && new.sha256 == old.sha256
        && new.uploaded == old.uploaded
        && new.id == old.id
        && new.author.as_ref() == Some(&old.author)
        && new.comment.as_ref() == Some(&old.comment)
        && new.files.as_ref() == Some(&old.files)
}

/// Whether a current `commitishes` row holds a format-1 row's values
///
/// # Arguments
///
/// * `old` - The format-1 row
/// * `new` - Its conversion
fn commitish_converted(old: &CommitishV1, new: &Commitish) -> bool {
    // every format-1 value kept and the required ones set
    new.kind == old.kind
        && new.group == old.group
        && new.repo == old.repo
        && new.key == old.key
        && new.data.as_ref() == Some(&old.data)
        && new.repo_data.as_ref() == Some(&old.repo_data)
        && new.timestamp == Some(old.timestamp)
}

/// Whether a current `commitish_list` row holds a format-1 row's values
///
/// # Arguments
///
/// * `old` - The format-1 row
/// * `new` - Its conversion
fn commitish_list_converted(old: &CommitishListV1, new: &CommitishList) -> bool {
    // every format-1 value kept and the required ones set
    new.kind == old.kind
        && new.group == old.group
        && new.year == old.year
        && new.bucket == old.bucket
        && new.repo == old.repo
        && new.timestamp == old.timestamp
        && new.key == old.key
        && new.repo_data.as_ref() == Some(&old.repo_data)
}

/// Whether a current `entities` row holds a format-1 row's values
///
/// # Arguments
///
/// * `old` - The format-1 row
/// * `new` - Its conversion
fn entity_converted(old: &EntityV1, new: &Entity) -> bool {
    // every format-1 value kept and the required ones set
    new.kind == old.kind
        && new.group == old.group
        && new.year == old.year
        && new.bucket == old.bucket
        && new.created == old.created
        && new.id == old.id
        && new.name.as_ref() == Some(&old.name)
        && new.submitter.as_ref() == Some(&old.submitter)
        && new.kind_data.as_ref() == Some(&old.kind_data)
        && new.description == old.description
        && new.image == old.image
}

/// Whether a current `network_policies` row holds a format-1 row's values
///
/// # Arguments
///
/// * `old` - The format-1 row
/// * `new` - Its conversion
fn network_policy_converted(old: &NetworkPolicyV1, new: &NetworkPolicy) -> bool {
    // every format-1 value kept and the required ones set
    new.group == old.group
        && new.name == old.name
        && new.id == Some(old.id)
        && new.k8s_name.as_ref() == Some(&old.k8s_name)
        && new.created == Some(old.created)
        && new.ingress.as_ref() == Some(&old.ingress)
        && new.egress.as_ref() == Some(&old.egress)
        && new.forced_policy == Some(old.forced_policy)
        && new.default_policy == Some(old.default_policy)
}

/// Whether a current `nodes` row holds a format-1 row's values
///
/// # Arguments
///
/// * `old` - The format-1 row
/// * `new` - Its conversion
fn node_converted(old: &NodeV1, new: &Node) -> bool {
    // every format-1 value kept and the required ones set
    new.cluster == old.cluster
        && new.node == old.node
        && format!("{:?}", new.health) == format!("{:?}", Some(&old.health))
        && new.heart_beat == old.heart_beat
        && format!("{:?}", new.resources) == format!("{:?}", Some(&old.resources))
}

/// Whether a current `notifications` row holds a format-1 row's values, with no TTL
///
/// # Arguments
///
/// * `old` - The format-1 row
/// * `new` - Its conversion
fn notification_converted(old: &NotificationV1, new: &Notification) -> bool {
    // every format-1 value kept, the required ones set, and the added TTL unset
    new.kind == old.kind
        && new.key == old.key
        && new.created == old.created
        && new.id == old.id
        && new.msg.as_ref() == Some(&old.msg)
        && new.level == Some(old.level.clone())
        && new.ban_id == old.ban_id
        && new.ttl.is_none()
}

/// Whether a current `repos_list` row holds a format-1 row's values, with its default checkout
/// and earliest commit unset
///
/// # Arguments
///
/// * `old` - The format-1 row
/// * `new` - Its conversion
fn repo_list_converted(old: &RepoListV1, new: &RepoList) -> bool {
    // every format-1 value kept, the required ones set, and the added columns unset
    new.group == old.group
        && new.year == old.year
        && new.bucket == old.bucket
        && new.uploaded == old.uploaded
        && new.id == old.id
        && new.creator.as_ref() == Some(&old.creator)
        && new.name.as_ref() == Some(&old.name)
        && new.provider.as_ref() == Some(&old.provider)
        && new.scheme.as_ref() == Some(&old.scheme)
        && new.url.as_ref() == Some(&old.url)
        && new.user.as_ref() == Some(&old.user)
        && new.default_checkout.is_none()
        && new.earliest.is_none()
}

/// Whether a current `results` row holds a format-1 row's values, with its tool version and
/// entities unset
///
/// # Arguments
///
/// * `old` - The format-1 row
/// * `new` - Its conversion
fn output_converted(old: &OutputV1, new: &Output) -> bool {
    // every format-1 value kept, the required ones set, and the added columns unset
    new.id == old.id
        && new.tool.as_ref() == Some(&old.tool)
        && new.cmd == old.cmd
        && new.uploaded == old.uploaded
        && new.result.as_ref() == Some(&old.result)
        && new.files == old.files
        && new.display_type == Some(old.display_type)
        && new.children == old.children
        && new.tool_version.is_none()
        && new.entities.is_none()
}

/// Whether a current `results_stream` row holds a format-1 row's values, with its tool version
/// unset
///
/// # Arguments
///
/// * `old` - The format-1 row
/// * `new` - Its conversion
fn output_stream_converted(old: &OutputStreamV1, new: &OutputStream) -> bool {
    // every format-1 value kept, the required ones set, and the added column unset
    new.kind == old.kind
        && new.group == old.group
        && new.year == old.year
        && new.bucket == old.bucket
        && new.key == old.key
        && new.tool.as_ref() == Some(&old.tool)
        && new.display_type == Some(old.display_type)
        && new.uploaded == old.uploaded
        && new.cmd == old.cmd
        && new.id == old.id
        && new.tool_version.is_none()
}

/// Whether a current `samples_list` row holds a format-1 row's values
///
/// # Arguments
///
/// * `old` - The format-1 row
/// * `new` - Its conversion
fn samples_list_converted(old: &SamplesListV1, new: &SamplesList) -> bool {
    // every format-1 value kept and the required ones set
    new.group == old.group
        && new.year == old.year
        && new.bucket == old.bucket
        && new.sha256.as_ref() == Some(&old.sha256)
        && new.sha1.as_ref() == Some(&old.sha1)
        && new.md5.as_ref() == Some(&old.md5)
        && new.id == old.id
        && new.name == old.name
        && new.description == old.description
        && new.submitter.as_ref() == Some(&old.submitter)
        && new.origin == old.origin
        && new.uploaded == old.uploaded
}

/// Generated format-1 `associations` rows read back as their conversion
#[test]
fn associations_format_1_round_trip() {
    check_format_one(associations_v1(), association_converted, debug_form);
}

/// Generated format-1 `comments` rows read back as their conversion
#[test]
fn comments_format_1_round_trip() {
    check_format_one(comments_v1(), comment_converted, debug_form);
}

/// Generated format-1 `commitishes` rows read back as their conversion
#[test]
fn commitishes_format_1_round_trip() {
    check_format_one(commitishes_v1(), commitish_converted, debug_form);
}

/// Generated format-1 `commitish_list` rows read back as their conversion
#[test]
fn commitish_list_format_1_round_trip() {
    check_format_one(commitish_lists_v1(), commitish_list_converted, debug_form);
}

/// Generated format-1 `entities` rows read back as their conversion
#[test]
fn entities_format_1_round_trip() {
    check_format_one(entities_v1(), entity_converted, debug_form);
}

/// Generated format-1 `network_policies` rows read back as their conversion
#[test]
fn network_policies_format_1_round_trip() {
    check_format_one(network_policies_v1(), network_policy_converted, debug_form);
}

/// Generated format-1 `nodes` rows read back as their conversion
#[test]
fn nodes_format_1_round_trip() {
    check_format_one(nodes_v1(), node_converted, debug_form);
}

/// Generated format-1 `notifications` rows read back as their conversion
#[test]
fn notifications_format_1_round_trip() {
    check_format_one(notifications_v1(), notification_converted, debug_form);
}

/// Generated format-1 `results` rows read back as their conversion
#[test]
fn results_format_1_round_trip() {
    check_format_one(outputs_v1(), output_converted, output_form);
}

/// Generated format-1 `results_stream` rows read back as their conversion
#[test]
fn results_stream_format_1_round_trip() {
    check_format_one(output_streams_v1(), output_stream_converted, debug_form);
}

/// Generated format-1 `repos_list` rows read back as their conversion
#[test]
fn repos_list_format_1_round_trip() {
    check_format_one(repo_lists_v1(), repo_list_converted, debug_form);
}

/// Generated format-1 `samples_list` rows read back as their conversion
#[test]
fn samples_list_format_1_round_trip() {
    check_format_one(samples_lists_v1(), samples_list_converted, debug_form);
}
