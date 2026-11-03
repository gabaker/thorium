// project imports
import type { BranchDetails, CommitDetails, CommitishDetails, GitTagDetails } from '@models/repos';

/** A repo's commitishes split by kind, each newest first. */
export interface SplitCommitishes {
  /** The branches. */
  branches: BranchDetails[];
  /** The git tags. */
  tags: GitTagDetails[];
  /** The commits. */
  commits: CommitDetails[];
}

/**
 * Split a repo's commitishes by kind and sort each kind newest first.
 *
 * @param list - The commitishes, each holding one of `Commit`, `Branch`, or `Tag`.
 * @returns The branches, tags, and commits.
 */
export function splitCommitishes(list: CommitishDetails[]): SplitCommitishes {
  const split: SplitCommitishes = { branches: [], tags: [], commits: [] };
  for (const item of list) {
    if ('Branch' in item) split.branches.push(item.Branch);
    else if ('Tag' in item) split.tags.push(item.Tag);
    else split.commits.push(item.Commit);
  }
  // compare instants: timestamps vary in fractional precision, so they don't sort as text
  const newestFirst = (a: { timestamp: string }, b: { timestamp: string }) => Date.parse(b.timestamp) - Date.parse(a.timestamp);
  split.branches.sort(newestFirst);
  split.tags.sort(newestFirst);
  split.commits.sort(newestFirst);
  return split;
}

/**
 * Shorten a commit hash for display.
 *
 * @param hash - The full commit hash.
 * @returns The first 10 characters.
 */
export function shortHash(hash: string): string {
  return hash.slice(0, 10);
}
