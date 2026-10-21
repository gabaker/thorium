//! The generated statements Thorium runs against Scylla, one queries struct per table and view

mod entities;
mod repos;
mod results;
mod samples;
#[cfg(feature = "search-streamer")]
pub mod search;

pub use entities::{
    AssociationsListCursor, AssociationsQueries, EntitiesByIdQueries, EntitiesListCursor,
    EntitiesQueries, LogsQueries, NetworkPoliciesByNameQueries, NetworkPoliciesDefaultQueries,
    NetworkPoliciesListCursor, NetworkPoliciesQueries, NodesInsert, NodesListCursor,
    NodesListDetailsCursor, NodesQueries, NotificationsQueries,
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

use scylla::client::session::Session;
use scylla::errors::PrepareError;

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
}

impl ScyllaQueries {
    /// Prepare every generated statement
    ///
    /// # Arguments
    ///
    /// * `session` - The Scylla session to prepare on
    /// * `ns` - The keyspace the statements target
    // the driver's own error is returned as is, so callers convert it the way they already do
    #[allow(clippy::result_large_err)]
    pub async fn prepare(session: &Session, ns: &str) -> Result<Self, PrepareError> {
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
        let associations = AssociationsQueries::prepare(session, ns).await?;
        let network_policies = NetworkPoliciesQueries::prepare(session, ns).await?;
        let network_policies_by_name = NetworkPoliciesByNameQueries::prepare(session, ns).await?;
        let network_policies_default = NetworkPoliciesDefaultQueries::prepare(session, ns).await?;
        let nodes = NodesQueries::prepare(session, ns).await?;
        let notifications = NotificationsQueries::prepare(session, ns).await?;
        let logs = LogsQueries::prepare(session, ns).await?;
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
            associations,
            network_policies,
            network_policies_by_name,
            network_policies_default,
            nodes,
            notifications,
            logs,
        })
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
