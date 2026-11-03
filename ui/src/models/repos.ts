// project imports
import { Tags } from './tags';

export type RepoCheckout = {
  /// Checkout a commit in detached mode
  Commit?: string;
  /// Checkout a branch
  Branch?: string;
  /// Checkout a tag
  Tag: string;
};

export type RepoScheme = {
  /// Use https when pulling this repo
  Https?: 'Https';
  /// Use http when pulling this repo
  Http?: 'Http';
  /// Use https when pulling this repo with auth info
  HttpsAuthed?: { username: string; password: string };
  /// Use http when pulling this repo with auth info
  HttpAuthed?: { username: string; password: string };
  /// An authenticated scheme was used but not for this user
  ScrubbedAuth?: 'ScrubbedAuth';
};

export type RepoSubmissionChunk = {
  /// The group this repo submission is visible by
  groups: string[];
  /// The unique id for this repo submission
  id: string;
  /// The user that added this repo to Thorium
  creator: string;
  /// When this repo was added to Thorium
  uploaded: string;
  // The scheme to use when cloning this repo
  scheme: RepoScheme;
  /// The earliest commit ever seen in this repo
  earliest?: string;
};

export type Repo = {
  /// Where this repo comes from (e.g. github.com)
  provider: string;
  /// The user that created this repo in the provider
  user: string;
  /// The name of this repo
  name: string;
  /// The url for this repo
  url: string;
  /// The tags for this repo
  tags: Tags;
  /// The default checkout behavior for this repo
  default_checkout?: RepoCheckout;
  /// The submissions for this repo
  submissions: RepoSubmissionChunk[];
  /// The earliest commit ever seen in this repo
  earliest?: string;
};

/// A commit of a repo with its details
export type CommitDetails = {
  /// The hash of this commit
  hash: string;
  /// The groups this commit is visible to
  groups: string[];
  /// When this commit was added to the repo
  timestamp: string;
  /// The author of this commit
  author: string;
  /// The topic (first line) of this commit
  topic?: string | null;
  /// The description of this commit
  description?: string | null;
  /// Whether the description was truncated
  truncated: boolean;
};

/// A branch of a repo with its details
export type BranchDetails = {
  /// The name of this branch
  name: string;
  /// The groups this branch is visible to
  groups: string[];
  /// The commit this branch is on
  commit: string;
  /// When this branch was last updated
  timestamp: string;
};

/// A git tag of a repo with its details
export type GitTagDetails = {
  /// The name of this tag
  name: string;
  /// The groups this tag is visible to
  groups: string[];
  /// The commit this tag is on
  commit: string;
  /// The author of this tag
  author: string;
  /// When this tag was last updated
  timestamp: string;
};

/// A commit, branch, or tag of a repo with its details, as the API returns it (one key set)
export type CommitishDetails = { Commit: CommitDetails } | { Branch: BranchDetails } | { Tag: GitTagDetails };

/// A page of a repo's commitishes
export type CommitishDetailsPage = {
  /// The cursor of the next page, when there is one
  cursor?: string | null;
  /// The commitishes on this page
  data: CommitishDetails[];
};
