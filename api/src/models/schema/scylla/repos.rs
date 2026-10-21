//! The Scylla tables and views for repos, their data blobs, and their commitishes

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::models::schema::{ScyllaTable, ScyllaView};
use crate::models::{CommitishKinds, RepoCheckout, RepoScheme};

/// The repo data blobs (zipped repo snapshots in S3) that exist for each repo
#[derive(ScyllaTable)]
#[thorium_table(name = "repo_data")]
pub struct RepoDataTable {
    /// The url of the repo this data blob belongs to
    #[thorium(partition)]
    pub repo: String,
    /// The sha256 of this repo data blob in S3
    #[thorium(cluster = "asc")]
    pub hash: String,
}
/// The ground truth table for every repo submission, bucketed by upload time per group
#[derive(ScyllaTable)]
#[thorium_table(name = "repos_list")]
pub struct ReposListTable {
    /// The group this repo submission is visible to
    #[thorium(partition)]
    pub group: String,
    /// The year this repo was submitted
    #[thorium(partition, year)]
    pub year: i32,
    /// The time bucket within the year this repo was submitted in
    #[thorium(partition, bucket(partition_size = "repos.partition_size"))]
    pub bucket: i32,
    /// When this repo was submitted
    #[thorium(cluster = "desc", timestamp)]
    pub uploaded: DateTime<Utc>,
    /// The unique id for this repo submission
    #[thorium(cluster = "desc")]
    pub id: Uuid,
    /// The reduced url for this repo ("provider/user/name")
    pub url: String,
    /// Where this repo comes from (e.g. github.com)
    pub provider: String,
    /// The user that owns this repo at its provider
    pub user: String,
    /// The name of this repo
    pub name: String,
    /// The Thorium user that submitted this repo
    pub creator: String,
    /// The scheme to use when cloning this repo (JSON text through `ScyllaStoreJson`); can
    /// hold credentials, so every generated `Debug` prints `<redacted>` for it
    #[thorium(redact)]
    pub scheme: RepoScheme,
    /// The default checkout behavior for this repo (JSON text through `ScyllaStoreJson`; `None` is
    /// CQL null)
    pub default_checkout: Option<RepoCheckout>,
    /// The timestamp of the earliest commitish seen for this repo
    pub earliest: Option<DateTime<Utc>>,
}
/// Repo submissions by url, for getting and authorizing a repo across groups
#[derive(ScyllaView)]
#[thorium_view(name = "repos", base = ReposListTable)]
pub struct ReposView {
    /// The reduced url for this repo
    #[thorium(partition)]
    pub url: String,
    /// The group this repo submission is visible to
    #[thorium(cluster = "asc")]
    pub group: String,
    /// When this repo was submitted
    #[thorium(cluster = "desc")]
    pub uploaded: DateTime<Utc>,
    /// The year this repo was submitted
    #[thorium(cluster = "desc")]
    pub year: i32,
    /// The time bucket this repo was submitted in
    #[thorium(cluster = "desc")]
    pub bucket: i32,
    /// The unique id for this repo submission
    #[thorium(cluster = "desc")]
    pub id: Uuid,
    /// Where this repo comes from
    pub provider: String,
    /// The user that owns this repo at its provider
    pub user: String,
    /// The name of this repo
    pub name: String,
    /// The Thorium user that submitted this repo
    pub creator: String,
    /// The clone scheme (JSON text); redacted in every generated `Debug`
    #[thorium(redact)]
    pub scheme: RepoScheme,
    /// The default checkout behavior (JSON text; `None` is CQL null)
    pub default_checkout: Option<RepoCheckout>,
    /// The earliest commitish timestamp for this repo
    pub earliest: Option<DateTime<Utc>>,
}
/// The details of every commitish (commit, branch, tag) of a repo, one partition per commitish per group
#[derive(ScyllaTable)]
#[thorium_table(name = "commitishes")]
pub struct CommitishesTable {
    /// The kind of commitish (commit, branch, tag)
    #[thorium(partition)]
    pub kind: CommitishKinds,
    /// The group this commitish is visible to
    #[thorium(partition)]
    pub group: String,
    /// The url of the repo this commitish belongs to
    #[thorium(partition)]
    pub repo: String,
    /// The commitish key (commit hash, branch or tag name)
    #[thorium(partition)]
    pub key: String,
    /// When this commitish was created
    pub timestamp: DateTime<Utc>,
    /// The serialized kind-specific commitish data (JSON text)
    pub data: String,
    /// The sha256 of the repo data blob containing this commitish
    pub repo_data: String,
}
/// Commitishes of a repo listed by time, bucketed per kind, group, and repo
#[derive(ScyllaTable)]
#[thorium_table(name = "commitish_list")]
pub struct CommitishListTable {
    /// The kind of commitish
    #[thorium(partition)]
    pub kind: CommitishKinds,
    /// The group this commitish is visible to
    #[thorium(partition)]
    pub group: String,
    /// The year of this commitish's timestamp
    #[thorium(partition, year)]
    pub year: i32,
    /// The time bucket within the year of this commitish's timestamp
    #[thorium(partition, bucket(partition_size = "repos.partition_size"))]
    pub bucket: i32,
    /// The url of the repo this commitish belongs to
    #[thorium(partition)]
    pub repo: String,
    /// When this commitish was created
    #[thorium(cluster = "desc", timestamp)]
    pub timestamp: DateTime<Utc>,
    /// The commitish key (commit hash, branch or tag name)
    #[thorium(cluster = "desc")]
    pub key: String,
    /// The sha256 of the repo data blob containing this commitish
    pub repo_data: String,
}
/// Commitishes by the repo data blob that contains them, for pruning orphaned blobs
#[derive(ScyllaView)]
#[thorium_view(name = "committed_repo_data", base = CommitishListTable)]
pub struct CommittedRepoDataView {
    /// The sha256 of the repo data blob
    #[thorium(partition)]
    pub repo_data: String,
    /// The year of the commitish's timestamp
    #[thorium(partition, year)]
    pub year: i32,
    /// The time bucket of the commitish's timestamp
    #[thorium(partition, bucket(partition_size = "repos.partition_size"))]
    pub bucket: i32,
    /// The kind of commitish
    #[thorium(cluster = "asc")]
    pub kind: CommitishKinds,
    /// The group the commitish is visible to
    #[thorium(cluster = "asc")]
    pub group: String,
    /// The repo url
    #[thorium(cluster = "asc")]
    pub repo: String,
    /// The commitish key
    #[thorium(cluster = "asc")]
    pub key: String,
    /// When the commitish was created
    #[thorium(cluster = "asc")]
    pub timestamp: DateTime<Utc>,
}
