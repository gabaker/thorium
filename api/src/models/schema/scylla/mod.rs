//! Every Scylla table and view Thorium declares

mod associations;
mod entities;
mod logs;
mod network_policies;
mod nodes;
mod notifications;
mod repos;
mod results;
mod samples;
mod tags;

pub use associations::{AssociationsTable, AssociationsTableRef};
pub use entities::{EntitiesByIdView, EntitiesTable, EntitiesTableRef};
pub use logs::{LogsTable, LogsTableRef};
pub use network_policies::{
    NetworkPoliciesByNameView, NetworkPoliciesDefaultView, NetworkPoliciesTable,
    NetworkPoliciesTableRef,
};
pub use nodes::{NodesTable, NodesTableRef};
pub use notifications::{NotificationsTable, NotificationsTableRef};
pub use repos::{
    CommitishListTable, CommitishListTableRef, CommitishesTable, CommitishesTableRef,
    CommittedRepoDataView, RepoDataTable, RepoDataTableRef, ReposListTable, ReposListTableRef,
    ReposView,
};
pub use results::{
    ResultsAuthView, ResultsIdsView, ResultsStreamTable, ResultsStreamTableRef, ResultsTable,
    ResultsTableRef,
};
pub use samples::{
    CommentsByIdView, CommentsTable, CommentsTableRef, S3IdsTable, S3IdsTableRef, S3Sha256sView,
    SamplesListTable, SamplesListTableRef, SamplesView,
};
pub use tags::{TagsByItemView, TagsCaseInsensitiveView, TagsTable, TagsTableRef};

/// Every Scylla object Thorium declares, in creation order
#[thorium::keyspace]
pub struct ThoriumKeyspace {
    /// The associations table
    associations: AssociationsTable,
    /// The entities table
    entities: EntitiesTable,
    /// The entities by id view
    entities_by_id: EntitiesByIdView,
    /// The comments table
    comments: CommentsTable,
    /// The comments by id view
    comments_by_id: CommentsByIdView,
    /// The commitishes table
    commitishes: CommitishesTable,
    /// The commitish list table
    commitish_list: CommitishListTable,
    /// The committed repo data view
    committed_repo_data: CommittedRepoDataView,
    /// The logs table
    logs: LogsTable,
    /// The network policies table
    network_policies: NetworkPoliciesTable,
    /// The default network policies view
    network_policies_default: NetworkPoliciesDefaultView,
    /// The network policies by name view
    network_policies_by_name: NetworkPoliciesByNameView,
    /// The nodes table
    nodes: NodesTable,
    /// The notifications table
    notifications: NotificationsTable,
    /// The repo data table
    repo_data: RepoDataTable,
    /// The repos list table
    repos_list: ReposListTable,
    /// The repos view
    repos: ReposView,
    /// The results table
    results: ResultsTable,
    /// The results stream table
    results_stream: ResultsStreamTable,
    /// The results auth view
    results_auth: ResultsAuthView,
    /// The results ids view
    results_ids: ResultsIdsView,
    /// The s3 ids table
    s3_ids: S3IdsTable,
    /// The s3 ids by sha256 view
    s3_sha256s: S3Sha256sView,
    /// The samples list table
    samples_list: SamplesListTable,
    /// The samples view
    samples: SamplesView,
    /// The tags table
    tags: TagsTable,
    /// The tags by item view
    tags_by_item: TagsByItemView,
    /// The case-insensitive tags view
    tags_case_insensitive: TagsCaseInsensitiveView,
}
