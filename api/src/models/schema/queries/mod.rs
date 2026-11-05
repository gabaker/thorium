//! The generated statements Thorium runs against Scylla, one queries struct per table and view

mod entities;
pub mod migrations;
mod repos;
mod results;
mod samples;
mod scan_stats;
#[cfg(feature = "search-streamer")]
pub mod search;

pub use entities::{
    AssociationsListCursor, AssociationsQueries, EntitiesByIdQueries, EntitiesByNameQueries,
    EntitiesListCursor, EntitiesQueries, LogsQueries, NetworkPoliciesByNameQueries,
    NetworkPoliciesDefaultQueries, NetworkPoliciesListCursor, NetworkPoliciesQueries, NodesInsert,
    NodesListCursor, NodesListDetailsCursor, NodesQueries, NotificationsQueries,
};
pub use repos::{
    CommitishListListCursor, CommitishListQueries, CommitishesQueries,
    CommittedRepoDataGetRepoDataCountCursor, CommittedRepoDataQueries, RepoDataQueries,
    ReposListInsert, ReposListListCursor, ReposListQueries, ReposQueries,
};
pub use results::{
    ResultsAuthQueries, ResultsIdsQueries, ResultsInsert, ResultsQueries, ResultsStreamQueries,
    TagsByItemQueries, TagsCaseInsensitiveListCursor, TagsCaseInsensitiveQueries, TagsListCursor,
    TagsQueries,
};
pub use samples::{
    CommentsByIdQueries, CommentsQueries, S3IdsQueries, S3Sha256sQueries, SamplesListListCursor,
    SamplesListQueries, SamplesQueries,
};
pub use scan_stats::ScanStatsQueries;

#[cfg(feature = "api")]
use axum::http::StatusCode;
use scylla::client::session::Session;
use scylla::errors::PrepareError;
#[cfg(feature = "api")]
use std::collections::BTreeSet;

#[cfg(feature = "api")]
use super::apply::read_texts;
#[cfg(feature = "api")]
use super::introduced_by_later_revision;
#[cfg(feature = "api")]
use crate::models::upgrades::SchemaBackend;
#[cfg(feature = "api")]
use crate::utils::ApiError;

/// The declared Scylla objects a later revision added, whose statements an offline writer leaves
/// unprepared while the keyspace doesn't hold them yet
///
/// An upgrade runs an earlier revision's conversion steps before the operator applies the schema
/// of the image it deploys, so a writer running inside one of those steps can find the schema the
/// upgrade started from. Each object here has an optional field in [`ScyllaQueries`], and every
/// Scylla object `LATER_REVISION_OBJECTS` lists must be here. Which objects are missing is read
/// through `LATER_REVISION_OBJECTS` itself; a unit test holds this list to it, so each one has an
/// optional field.
pub const ADDED_LATER: [&str; 2] = ["entities_by_name", "scan_stats"];

/// Every queries struct, prepared against one keyspace
pub struct ScyllaQueries {
    /// Statements on the `samples_list` table
    pub samples_list: SamplesListQueries,
    /// Statements on the samples view
    pub samples: SamplesQueries,
    /// Statements on the `s3_ids` table
    pub s3_ids: S3IdsQueries,
    /// Statements on the `s3_sha256s` view
    pub s3_sha256s: S3Sha256sQueries,
    /// Statements on the comments table
    pub comments: CommentsQueries,
    /// Statements on the `comments_by_id` view
    pub comments_by_id: CommentsByIdQueries,
    /// Statements on the `repos_list` table
    pub repos_list: ReposListQueries,
    /// Statements on the repos view
    pub repos: ReposQueries,
    /// Statements on the `repo_data` table
    pub repo_data: RepoDataQueries,
    /// Statements on the commitishes table
    pub commitishes: CommitishesQueries,
    /// Statements on the `commitish_list` table
    pub commitish_list: CommitishListQueries,
    /// Statements on the `committed_repo_data` view
    pub committed_repo_data: CommittedRepoDataQueries,
    /// Statements on the results table
    pub results: ResultsQueries,
    /// Statements on the `results_stream` table
    pub results_stream: ResultsStreamQueries,
    /// Statements on the `results_auth` view
    pub results_auth: ResultsAuthQueries,
    /// Statements on the `results_ids` view
    pub results_ids: ResultsIdsQueries,
    /// Statements on the tags table
    pub tags: TagsQueries,
    /// Statements on the `tags_by_item` view
    pub tags_by_item: TagsByItemQueries,
    /// Statements on the `tags_case_insensitive` view
    pub tags_case_insensitive: TagsCaseInsensitiveQueries,
    /// Statements on the entities table
    pub entities: EntitiesQueries,
    /// Statements on the `entities_by_id` view
    pub entities_by_id: EntitiesByIdQueries,
    /// Statements on the `entities_by_name` view; absent only for an offline writer on a keyspace
    /// that doesn't hold the view yet (read them through [`ScyllaQueries::entities_by_name`])
    pub entities_by_name: Option<EntitiesByNameQueries>,
    /// Statements on the associations table
    pub associations: AssociationsQueries,
    /// Statements on the `network_policies` table
    pub network_policies: NetworkPoliciesQueries,
    /// Statements on the `network_policies_by_name` view
    pub network_policies_by_name: NetworkPoliciesByNameQueries,
    /// Statements on the `network_policies_default` view
    pub network_policies_default: NetworkPoliciesDefaultQueries,
    /// Statements on the nodes table
    pub nodes: NodesQueries,
    /// Statements on the notifications table
    pub notifications: NotificationsQueries,
    /// Statements on the logs table
    pub logs: LogsQueries,
    /// Statements on the `scan_stats` table; absent only for an offline writer on a keyspace that
    /// doesn't hold the table yet (read them through [`ScyllaQueries::scan_stats`])
    pub scan_stats: Option<ScanStatsQueries>,
}

impl ScyllaQueries {
    /// Prepare every generated statement, failing when any object is missing from the keyspace
    ///
    /// # Arguments
    ///
    /// * `session` - The Scylla session to prepare on
    /// * `ns` - The keyspace the statements target
    // the driver's own error is returned as is, so callers convert it the way they already do
    #[allow(clippy::result_large_err)]
    pub async fn prepare(session: &Session, ns: &str) -> Result<Self, PrepareError> {
        // the API runs on the whole declared schema, so every object's statements are prepared
        Self::prepare_with(session, ns, &|_| true).await
    }

    /// Prepare every generated statement except those of the objects a later revision added that
    /// the keyspace doesn't hold yet, for a writer that runs inside an upgrade
    ///
    /// # Arguments
    ///
    /// * `session` - The Scylla session to prepare on
    /// * `ns` - The keyspace the statements target
    #[cfg(feature = "api")]
    pub async fn prepare_present(session: &Session, ns: &str) -> Result<Self, ApiError> {
        // find which of the later objects the keyspace holds
        let present = present_objects(session, ns).await?;
        // prepare the rest as the API does, leaving out the later objects that are missing
        Self::prepare_with(session, ns, &|name| present.contains(name))
            .await
            .map_err(|err| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Some(format!("the Scylla statements didn't prepare: {err}")),
                )
            })
    }

    /// Prepare the generated statements, leaving out a later object the keyspace doesn't hold
    ///
    /// # Arguments
    ///
    /// * `session` - The Scylla session to prepare on
    /// * `ns` - The keyspace the statements target
    /// * `holds` - Whether the keyspace holds an object from [`ADDED_LATER`]
    // the driver's own error is returned as is, as `prepare` returns it
    #[allow(clippy::result_large_err)]
    async fn prepare_with(
        session: &Session,
        ns: &str,
        holds: &(dyn Fn(&str) -> bool + Sync),
    ) -> Result<Self, PrepareError> {
        // prepare each object's statements in turn
        let samples_list = SamplesListQueries::prepare(session, ns).await?;
        let samples = SamplesQueries::prepare(session, ns).await?;
        let s3_ids = S3IdsQueries::prepare(session, ns).await?;
        let s3_sha256s = S3Sha256sQueries::prepare(session, ns).await?;
        let comments = CommentsQueries::prepare(session, ns).await?;
        let comments_by_id = CommentsByIdQueries::prepare(session, ns).await?;
        let repos_list = ReposListQueries::prepare(session, ns).await?;
        let repos = ReposQueries::prepare(session, ns).await?;
        let repo_data = RepoDataQueries::prepare(session, ns).await?;
        let commitishes = CommitishesQueries::prepare(session, ns).await?;
        let commitish_list = CommitishListQueries::prepare(session, ns).await?;
        let committed_repo_data = CommittedRepoDataQueries::prepare(session, ns).await?;
        let results = ResultsQueries::prepare(session, ns).await?;
        let results_stream = ResultsStreamQueries::prepare(session, ns).await?;
        let results_auth = ResultsAuthQueries::prepare(session, ns).await?;
        let results_ids = ResultsIdsQueries::prepare(session, ns).await?;
        let tags = TagsQueries::prepare(session, ns).await?;
        let tags_by_item = TagsByItemQueries::prepare(session, ns).await?;
        let tags_case_insensitive = TagsCaseInsensitiveQueries::prepare(session, ns).await?;
        let entities = EntitiesQueries::prepare(session, ns).await?;
        let entities_by_id = EntitiesByIdQueries::prepare(session, ns).await?;
        let entities_by_name = if holds("entities_by_name") {
            Some(EntitiesByNameQueries::prepare(session, ns).await?)
        } else {
            None
        };
        let associations = AssociationsQueries::prepare(session, ns).await?;
        let network_policies = NetworkPoliciesQueries::prepare(session, ns).await?;
        let network_policies_by_name = NetworkPoliciesByNameQueries::prepare(session, ns).await?;
        let network_policies_default = NetworkPoliciesDefaultQueries::prepare(session, ns).await?;
        let nodes = NodesQueries::prepare(session, ns).await?;
        let notifications = NotificationsQueries::prepare(session, ns).await?;
        let logs = LogsQueries::prepare(session, ns).await?;
        let scan_stats = if holds("scan_stats") {
            Some(ScanStatsQueries::prepare(session, ns).await?)
        } else {
            None
        };
        Ok(ScyllaQueries {
            samples_list,
            samples,
            s3_ids,
            s3_sha256s,
            comments,
            comments_by_id,
            repos_list,
            repos,
            repo_data,
            commitishes,
            commitish_list,
            committed_repo_data,
            results,
            results_stream,
            results_auth,
            results_ids,
            tags,
            tags_by_item,
            tags_case_insensitive,
            entities,
            entities_by_id,
            entities_by_name,
            associations,
            network_policies,
            network_policies_by_name,
            network_policies_default,
            nodes,
            notifications,
            logs,
            scan_stats,
        })
    }

    /// Get the statements on the `entities_by_name` view
    #[cfg(feature = "api")]
    pub fn entities_by_name(&self) -> Result<&EntitiesByNameQueries, ApiError> {
        // only an offline writer runs without them, and nothing it writes reads the view
        self.entities_by_name
            .as_ref()
            .ok_or_else(|| not_prepared("entities_by_name"))
    }

    /// Get the statements on the `scan_stats` table
    #[cfg(feature = "api")]
    pub fn scan_stats(&self) -> Result<&ScanStatsQueries, ApiError> {
        // only an offline writer runs without them, and nothing it writes touches the table
        self.scan_stats
            .as_ref()
            .ok_or_else(|| not_prepared("scan_stats"))
    }

    /// Every generated statement's text, named `<object>.<statement>`, in declaration order
    ///
    /// # Arguments
    ///
    /// * `ns` - The keyspace the statements target
    #[must_use]
    pub fn statements(ns: &str) -> Vec<(String, String)> {
        // list each queries struct's statements under its object's name
        let groups = [
            ("samples_list", SamplesListQueries::statements(ns)),
            ("samples", SamplesQueries::statements(ns)),
            ("s3_ids", S3IdsQueries::statements(ns)),
            ("s3_sha256s", S3Sha256sQueries::statements(ns)),
            ("comments", CommentsQueries::statements(ns)),
            ("comments_by_id", CommentsByIdQueries::statements(ns)),
            ("repos_list", ReposListQueries::statements(ns)),
            ("repos", ReposQueries::statements(ns)),
            ("repo_data", RepoDataQueries::statements(ns)),
            ("commitishes", CommitishesQueries::statements(ns)),
            ("commitish_list", CommitishListQueries::statements(ns)),
            (
                "committed_repo_data",
                CommittedRepoDataQueries::statements(ns),
            ),
            ("results", ResultsQueries::statements(ns)),
            ("results_stream", ResultsStreamQueries::statements(ns)),
            ("results_auth", ResultsAuthQueries::statements(ns)),
            ("results_ids", ResultsIdsQueries::statements(ns)),
            ("tags", TagsQueries::statements(ns)),
            ("tags_by_item", TagsByItemQueries::statements(ns)),
            (
                "tags_case_insensitive",
                TagsCaseInsensitiveQueries::statements(ns),
            ),
            ("entities", EntitiesQueries::statements(ns)),
            ("entities_by_id", EntitiesByIdQueries::statements(ns)),
            ("entities_by_name", EntitiesByNameQueries::statements(ns)),
            ("associations", AssociationsQueries::statements(ns)),
            ("network_policies", NetworkPoliciesQueries::statements(ns)),
            (
                "network_policies_by_name",
                NetworkPoliciesByNameQueries::statements(ns),
            ),
            (
                "network_policies_default",
                NetworkPoliciesDefaultQueries::statements(ns),
            ),
            ("nodes", NodesQueries::statements(ns)),
            ("notifications", NotificationsQueries::statements(ns)),
            ("logs", LogsQueries::statements(ns)),
            ("scan_stats", ScanStatsQueries::statements(ns)),
        ];
        groups
            .into_iter()
            .flat_map(|(object, statements)| {
                statements
                    .into_iter()
                    .map(move |(name, cql)| (format!("{object}.{name}"), cql))
            })
            .collect()
    }
}

/// Read which of the Scylla objects a later revision added (see [`ADDED_LATER`]) a keyspace holds
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `ns` - The keyspace to look in
#[cfg(feature = "api")]
async fn present_objects(session: &Session, ns: &str) -> Result<BTreeSet<String>, ApiError> {
    // Scylla stores unquoted keyspace names in lowercase
    let keyspace = ns.to_lowercase();
    // start with none found, since a missing object simply has no row
    let mut present = BTreeSet::new();
    // look among the keyspace's tables, then its views
    for cql in [
        "SELECT table_name FROM system_schema.tables WHERE keyspace_name = ?",
        "SELECT view_name FROM system_schema.views WHERE keyspace_name = ?",
    ] {
        // read every object's name, answering 503 when the schema tables can't be read
        let rows = read_texts::<1>(session, cql, &[&keyspace])
            .await
            .map_err(|err| {
                ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    Some(format!("Scylla's schema tables can't be read: {err}")),
                )
            })?;
        // keep only the later objects, the only ones that may be missing
        present.extend(
            rows.into_iter()
                .map(|[name]| name)
                .filter(|name| introduced_by_later_revision(SchemaBackend::Scylla, name)),
        );
    }
    Ok(present)
}

/// The error for statements an offline writer left unprepared because their object is missing
///
/// # Arguments
///
/// * `object` - The table or view the statements target
#[cfg(feature = "api")]
fn not_prepared(object: &str) -> ApiError {
    // the schema of the deployed image hasn't been applied yet, which a retry after it can fix
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        Some(format!(
            "the {object} statements aren't prepared because the keyspace doesn't hold {object} \
             yet; apply the schema first"
        )),
    )
}
