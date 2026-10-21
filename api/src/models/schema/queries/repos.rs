//! The statements on the repos, repo data, and commitish tables and their views

use crate::models::schema::scylla::{
    CommitishListTable, CommitishesTable, CommittedRepoDataView, RepoDataTable, ReposListTable,
    ReposView,
};
use crate::models::{
    CommitishDataRow, CommitishListRow, CommitishRepoDataRow, RepoAuthRow, RepoDataHashRow,
    RepoListRow, RepoRow,
};

/// Statements on the `repos_list` table
#[thorium::table_queries(table = ReposListTable)]
pub struct ReposListQueries {
    /// Insert a new repo submission for a group, leaving its earliest commitish unset
    #[insert(
        columns = [group, year, bucket, uploaded, id, url, provider, user, name, creator, scheme,
                   default_checkout],
        ttl = none
    )]
    insert: Insert,
    /// Set the default checkout behavior of one repo submission in one group
    #[update(set(default_checkout), by_key, eq(group, year, bucket, uploaded, id))]
    update_default_checkout: Update,
    /// Set the earliest commitish timestamp of one repo submission in one group
    #[update(set(earliest), by_key, eq(group, year, bucket, uploaded, id))]
    update_earliest: Update,
    /// The cursor listing repo submissions newest first across groups
    #[cursor(kind = time, group_by = group, year = year, bucket = bucket, range = uploaded,
             tie = id, project = RepoListRow, limit = per_partition)]
    list: Cursor,
    /// The number of repo submissions in each partition of a token range, for the census
    #[census(project = crate::models::git::RepoCensusRow)]
    census: Census,
}

/// Statements on the repos view, which finds repo submissions by url
#[thorium::view_queries(view = ReposView)]
pub struct ReposQueries {
    /// Every submission of a repo in a set of groups
    #[select(project = RepoRow, eq(url), r#in(group))]
    get: Select,
    /// Every submission of a set of repos in a set of groups
    #[select(project = RepoRow, r#in(url, group), chunk = 50)]
    get_many: Select,
    /// Which of a set of repos at least one of a set of groups can see
    #[select(
        project = RepoAuthRow,
        r#in(url, group),
        group_by(url),
        per_partition_limit = 1,
        chunk = 50
    )]
    auth: Select,
}

/// Statements on the `repo_data` table
#[thorium::table_queries(table = RepoDataTable)]
pub struct RepoDataQueries {
    /// Record a data blob of a repo
    #[insert(columns = all, ttl = none)]
    insert_data: Insert,
    /// The hashes of every data blob of a repo
    #[select(project = RepoDataHashRow, eq(repo))]
    get_data: Select,
    /// Whether a repo has a data blob
    #[select(eq(repo, hash), exists)]
    data_exists: Select,
    /// Remove a data blob from a repo
    #[delete(by_key, eq(repo, hash))]
    delete_data: Delete,
}

/// Statements on the commitishes table
#[thorium::table_queries(table = CommitishesTable)]
pub struct CommitishesQueries {
    /// Insert or overwrite a commitish's details for a group
    #[insert(columns = all, ttl = none)]
    insert: Insert,
    /// The details of a commitish in any of a set of groups
    #[select(project = CommitishDataRow, eq(kind), r#in(group), eq(repo, key), exists)]
    get_data: Select,
    /// The kind and repo data blob of a commitish key across kinds and groups
    #[select(project = CommitishRepoDataRow, r#in(kind, group), eq(repo, key), chunk = 32)]
    get_repo_data: Select,
}

/// Statements on the `commitish_list` table
#[thorium::table_queries(table = CommitishListTable)]
pub struct CommitishListQueries {
    /// Insert or overwrite a commitish's list row for a group
    #[insert(columns = all, ttl = none)]
    insert_list: Insert,
    /// The cursor listing a repo's commitishes of some kinds newest first across groups
    #[cursor(kind = time, group_by = group, r#in(kind), eq(repo), year = year, bucket = bucket,
             range = timestamp, tie = key, project = CommitishListRow, limit = per_partition)]
    list: Cursor,
    /// The number of commitishes in each partition of a token range, for the census
    #[census(project = crate::models::git::CommitishCensusRow)]
    census: Census,
}

/// Statements on the `committed_repo_data` view, which finds commitishes by their repo data
#[thorium::view_queries(view = CommittedRepoDataView)]
pub struct CommittedRepoDataQueries {
    /// Whether any commitish in some buckets of a year is backed by a repo data blob
    #[cursor(kind = exists, key = repo_data, year = year, bucket = bucket)]
    get_repo_data_count: Cursor,
}
