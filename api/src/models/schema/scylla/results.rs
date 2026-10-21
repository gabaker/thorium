//! The Scylla tables and views for results and the result streams

use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

use crate::models::schema::{ScyllaTable, ScyllaView};
use crate::models::{EntityKinds, ImageVersion, OutputDisplayType, OutputKind};

/// The stored output of one tool run, keyed by result id
#[derive(ScyllaTable)]
#[thorium_table(name = "results")]
pub struct ResultsTable {
    /// The unique id of this result
    #[thorium(partition)]
    pub id: Uuid,
    /// When this result was uploaded
    #[thorium(cluster = "desc", timestamp)]
    pub uploaded: DateTime<Utc>,
    /// The tool (image or pipeline) that produced this result
    pub tool: Option<String>,
    /// The version of the tool that produced this result
    pub tool_version: Option<ImageVersion>,
    /// The command used to produce this result
    pub cmd: Option<String>,
    /// The result body: opaque tool output, parsed as JSON on a best-effort basis when read
    pub result: Option<String>,
    /// The names of result files stored in S3 under this result id
    pub files: Option<HashSet<String>>,
    /// How the UI renders this result
    pub display_type: Option<OutputDisplayType>,
    /// Child samples found by this result (sha256 → submission id)
    pub children: Option<HashMap<String, Uuid>>,
    /// Entity counts found in this result, by kind; values are non-negative counts
    #[thorium(unsigned = "bigint")]
    pub entities: Option<HashMap<EntityKinds, usize>>,
}
/// Per-group, time-bucketed index of results by (kind, key), the base of the result auth views
#[derive(ScyllaTable)]
#[thorium_table(name = "results_stream")]
pub struct ResultsStreamTable {
    /// Whether this result is for a file or a repo
    #[thorium(partition)]
    pub kind: OutputKind,
    /// The group that can see this result
    #[thorium(partition)]
    pub group: String,
    /// The year this result was uploaded in
    #[thorium(partition, year)]
    pub year: i32,
    /// The time bucket within the year (`results.partition_size`)
    #[thorium(partition, bucket(partition_size = "results.partition_size"))]
    pub bucket: i32,
    /// When this result was uploaded
    #[thorium(cluster = "desc", timestamp)]
    pub uploaded: DateTime<Utc>,
    /// The result id
    #[thorium(cluster = "asc")]
    pub id: Uuid,
    /// The sample sha256 or repo url this result is for
    #[thorium(cluster = "asc")]
    pub key: String,
    /// The tool that produced this result
    pub tool: Option<String>,
    /// The version of the tool that produced this result
    pub tool_version: Option<ImageVersion>,
    /// How the UI renders this result
    pub display_type: Option<OutputDisplayType>,
    /// The command used to produce this result
    pub cmd: Option<String>,
}
/// Results by key, for auth and listing a key's results per group and tool
#[derive(ScyllaView)]
#[thorium_view(name = "results_auth", base = ResultsStreamTable)]
pub struct ResultsAuthView {
    /// The sample sha256 or repo url
    #[thorium(partition)]
    pub key: String,
    /// Whether this result is for a file or a repo
    #[thorium(cluster = "asc")]
    pub kind: OutputKind,
    /// The group that can see this result
    #[thorium(cluster = "asc")]
    pub group: String,
    /// The tool that produced this result
    #[thorium(cluster = "asc")]
    pub tool: String,
    /// The result id
    #[thorium(cluster = "asc")]
    pub id: Uuid,
    /// The year the result was uploaded in
    #[thorium(cluster = "asc")]
    pub year: i32,
    /// The time bucket the result was uploaded in
    #[thorium(cluster = "asc")]
    pub bucket: i32,
    /// When the result was uploaded
    #[thorium(cluster = "asc")]
    pub uploaded: DateTime<Utc>,
    /// The version of the tool that produced this result
    pub tool_version: Option<ImageVersion>,
    /// How the UI renders this result
    pub display_type: Option<OutputDisplayType>,
    /// The command used to produce this result
    pub cmd: Option<String>,
}
/// Results by (key, id), used to check whether a result is still reachable and to enumerate keys
#[derive(ScyllaView)]
#[thorium_view(name = "results_ids", base = ResultsStreamTable)]
pub struct ResultsIdsView {
    /// The sample sha256 or repo url
    #[thorium(partition)]
    pub key: String,
    /// The result id
    #[thorium(cluster = "asc")]
    pub id: Uuid,
    /// Whether this result is for a file or a repo
    #[thorium(cluster = "asc")]
    pub kind: OutputKind,
    /// The group that can see this result
    #[thorium(cluster = "asc")]
    pub group: String,
    /// The year the result was uploaded in
    #[thorium(cluster = "asc")]
    pub year: i32,
    /// The time bucket the result was uploaded in
    #[thorium(cluster = "asc")]
    pub bucket: i32,
    /// When the result was uploaded
    #[thorium(cluster = "asc")]
    pub uploaded: DateTime<Utc>,
    /// The tool that produced this result
    pub tool: Option<String>,
}
