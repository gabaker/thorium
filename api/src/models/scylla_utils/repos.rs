//! The scylla utils and structs for repos

use chrono::prelude::*;
use uuid::Uuid;

use crate::models::schema::ScyllaProjection;
use crate::models::schema::scylla::{
    CommitishListTable, CommitishesTable, RepoDataTable, ReposListTable, ReposView,
};
use crate::models::{Commitish, CommitishKinds, CommitishRequest};

#[cfg(feature = "api")]
use crate::models::{BranchDetails, CommitDetails, CommitishDetails, GitTagDetails};

/// An internal struct containing a single repo row in Scylla
#[derive(ScyllaProjection)]
#[thorium_projection(of = ReposView)]
pub struct RepoRow {
    /// The group this repo submission is visible by
    pub group: String,
    /// Where this repo comes from (e.g. github.com)
    pub provider: String,
    /// The user that created this repo in the provider
    pub user: String,
    /// The name of this repo
    pub name: String,
    /// The full url for this repo
    pub url: String,
    /// The unique id for this repo
    pub id: Uuid,
    /// The user that added this repo to Thorium
    pub creator: String,
    /// When this repo was added to Thorium
    pub uploaded: DateTime<Utc>,
    /// The scheme to use when cloning this repo, which can hold credentials
    #[thorium(redact)]
    pub scheme: String,
    /// The default checkout behavior for this repo
    pub default_checkout: Option<String>,
    /// The earliest commit ever seen in this repo
    pub earliest: Option<DateTime<Utc>>,
}

/// The url of a repo some groups can see
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = ReposView)]
pub struct RepoAuthRow {
    /// The reduced url of the repo
    pub url: String,
}

/// The sha256 of one of a repo's data blobs
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = RepoDataTable)]
pub struct RepoDataHashRow {
    /// The sha256 of the data blob
    pub hash: String,
}

/// The serialized details of a commitish
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = CommitishesTable)]
pub struct CommitishDataRow {
    /// The serialized kind-specific commitish data
    pub data: String,
}

/// The kind of a commitish and the repo data blob holding it
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = CommitishesTable)]
pub struct CommitishRepoDataRow {
    /// The kind of commitish
    pub kind: CommitishKinds,
    /// The sha256 of the repo data blob containing this commitish
    pub repo_data: String,
}

/// An internal struct containing a single repo list row in Scylla
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = ReposListTable)]
pub struct RepoListRow {
    /// The group this commit is visible too
    pub group: String,
    /// The url for this repo
    pub url: String,
    /// The id for this repo submission
    #[thorium(column = "id")]
    pub submission: Uuid,
    /// When this repo was submitted
    pub uploaded: DateTime<Utc>,
}

/// An internal struct containing a single repo commitish list row in Scylla
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = CommitishListTable)]
pub struct CommitishListRow {
    /// The kind of commitish this is
    pub kind: CommitishKinds,
    /// The group this commitish is visible too
    pub group: String,
    /// The key for this commitish (hash, tag/branch name)
    pub key: String,
    /// When this commitish was added to this repo
    pub timestamp: DateTime<Utc>,
}

/// The commit specific data serialied in scylla
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CommitData {
    /// The author of this commit
    pub author: String,
    /// The topic of this commit
    pub topic: Option<String>,
    /// The description for this commit
    pub description: Option<String>,
    /// Whether this description was truncated or not
    pub truncated: bool,
}

/// The  branch specific data serialied in scylla
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BranchData {
    /// The commit this branch is tied too
    pub commit: String,
}
/// The git tag specific data serialied in scylla
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GitTagData {
    /// The commit this tag is tied too
    pub commit: String,
    /// The author for this tag
    pub author: String,
}

impl CommitishRequest {
    /// Serialize this commitishes data and get its kind
    #[cfg(feature = "api")]
    pub fn serialize_data(self) -> Result<(CommitishKinds, String), crate::utils::ApiError> {
        // serialize our data and get its kind
        match self {
            CommitishRequest::Commit(mut commit) => {
                // truncate this commits description if its too long
                commit.truncate_description(524_288);
                // cast our request to a commit data
                let data = CommitData {
                    author: commit.author,
                    topic: commit.topic,
                    description: commit.description,
                    truncated: commit.truncated,
                };
                // serialize our commit data
                Ok((CommitishKinds::Commit, serde_json::to_string(&data)?))
            }
            CommitishRequest::Branch(branch) => {
                // cast our request to a branch data
                let data = BranchData {
                    commit: branch.commit,
                };
                // serialize our branch data
                Ok((CommitishKinds::Branch, serde_json::to_string(&data)?))
            }
            CommitishRequest::Tag(tag) => {
                // cast our request to a git tag data
                let data = GitTagData {
                    commit: tag.commit,
                    author: tag.author,
                };
                // serialize our tag data
                Ok((CommitishKinds::Tag, serde_json::to_string(&data)?))
            }
        }
    }
}

impl Commitish {
    /// Add a group onto this [`Commitish`]'s groups
    ///
    /// # Arguments
    ///
    /// * `group` - The group to add
    pub fn add_group<G: Into<String>>(&mut self, group: G) {
        match self {
            Self::Commit(commit) => commit.groups.push(group.into()),
            Self::Branch(branch) => branch.groups.push(group.into()),
            Self::Tag(tag) => tag.groups.push(group.into()),
        }
    }

    /// Extend this commitish with details
    ///
    /// # Arguments
    ///
    /// * `data` - The details data to inject into our commitishes
    #[cfg(feature = "api")]
    pub fn to_details(self, data: &str) -> Result<CommitishDetails, crate::utils::ApiError> {
        match self {
            Self::Commit(commit) => {
                // deserialize our commit data
                let commit_data: CommitData = crate::deserialize!(data);
                // build our commit details object
                let details = CommitDetails {
                    hash: commit.hash,
                    groups: commit.groups,
                    timestamp: commit.timestamp,
                    author: commit_data.author,
                    topic: commit_data.topic,
                    description: commit_data.description,
                    truncated: commit_data.truncated,
                };
                // rewrap and return our commit details
                Ok(CommitishDetails::Commit(details))
            }
            Self::Branch(branch) => {
                // deserialize our branch data
                let branch_data: BranchData = crate::deserialize!(data);
                // build our branch details object
                let details = BranchDetails {
                    name: branch.name,
                    groups: branch.groups,
                    commit: branch_data.commit,
                    timestamp: branch.timestamp,
                };
                Ok(CommitishDetails::Branch(details))
            }
            Self::Tag(tag) => {
                // deserialize our tag data
                let tag_data: GitTagData = crate::deserialize!(data);
                // build our tag details object
                let details = GitTagDetails {
                    name: tag.name,
                    groups: tag.groups,
                    commit: tag_data.commit,
                    author: tag_data.author,
                    timestamp: tag.timestamp,
                };
                Ok(CommitishDetails::Tag(details))
            }
        }
    }
}
