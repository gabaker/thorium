//! Query hooks for groups: the details of each group, read once and shared by every page that decides what to offer.
import { useQueries } from '@tanstack/react-query';

// project imports
import { getGroup } from '@thorpi/groups';
import { hasGroupDetails } from '@utilities/agentic/access';
import { Group } from '@models/groups';

/** How long a group's details stay fresh; roles change rarely while a page is open. */
export const GROUP_STALE_MS = 60 * 1000;

/** Query keys for groups, so every page that reads a group shares one cache entry per name. */
export const groupKeys = {
  /** Every group details query */
  all: ['group'] as const,
  /**
   * One group's details.
   *
   * @param name - The group's name.
   * @returns The query key.
   */
  details: (name: string) => ['group', name] as const,
};

/**
 * Get the distinct names to read, in a stable order.
 *
 * @param names - The group names, in any order and possibly repeated.
 * @returns Each non-empty name once, sorted.
 */
export function groupNamesToRead(names: string[]): string[] {
  return [...new Set(names.filter((name) => name !== ''))].sort();
}

/**
 * Keep the groups whose details were read in full, in the order of their names.
 *
 * @param read - The result of each group's read so far: the group, `null` when it can't be read, or `undefined`
 *   while it is read.
 * @returns The groups read in full.
 */
export function readableGroups(read: (Group | null | undefined)[]): Group[] {
  return read.filter((group) => hasGroupDetails(group));
}

/**
 * Read the details of groups through the query cache.
 *
 * A group that can't be read is left out, so the actions it would gate stay available and the API's refusal is shown
 * instead.
 *
 * @param names - The group names to read.
 * @returns The details of the groups read so far.
 */
export function useGroupDetailsQuery(names: string[]): Group[] {
  return useQueries({
    queries: groupNamesToRead(names).map((name) => ({
      queryKey: groupKeys.details(name),
      queryFn: () => getGroup(name, () => undefined),
      staleTime: GROUP_STALE_MS,
    })),
    // combining keeps one array identity while the reads are unchanged
    combine: (results) => readableGroups(results.map((result) => result.data)),
  });
}
