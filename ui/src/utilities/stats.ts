// project imports
import { GroupsStats, UserStageStats } from '@models/system';
import { ToolInstance } from '@models/tools';

/** One row of the group stats table: a user's jobs for one tool instance in one group. */
export interface GroupStatsRow extends UserStageStats {
  /// The group the jobs are in
  group: string;
  /// The tool instance whose shared workload queue holds the jobs
  toolInstance: string;
  /// The user the jobs belong to
  user: string;
}

/**
 * Flatten the per-group workload stats into one row per group, tool instance, and user.
 *
 * @param stats - The `groups` map of the system stats.
 * @returns The rows, in the map's order; a group without workloads adds none.
 */
export function flattenGroupStats(stats: GroupsStats): GroupStatsRow[] {
  return Object.entries(stats).flatMap(([group, groupStats]) =>
    Object.entries(groupStats.workloads ?? {}).flatMap(([toolInstance, users]) =>
      Object.entries(users).map(([user, counts]) => ({ group, toolInstance, user, ...counts })),
    ),
  );
}

/** The tool a workload's tool instance is a version of. */
export interface WorkloadInstance {
  /** The instance's name, `<tool>@<version>` */
  name: string;
  /** The id of the tool the instance belongs to */
  toolId: string;
}

/**
 * Index tool instances by id so workload rows can name the instance they run.
 *
 * @param instances - The tool instances the user can see.
 * @returns The instances' names and tool ids by instance id.
 */
export function indexWorkloadInstances(instances: ToolInstance[]): Map<string, WorkloadInstance> {
  return new Map(instances.map((instance) => [instance.id, { name: instance.name, toolId: instance.metadata.ToolInstance.tool }]));
}

/**
 * Check whether a group stats row matches the search text.
 *
 * The text is trimmed and compared case-insensitively, so a search of only whitespace matches every row
 * (the same way `emptyGroupStatsText` treats it as no search).
 *
 * @param row - The row to check.
 * @param searchQuery - The current search text.
 * @param instanceLabel - The name the row's tool instance is shown by.
 * @returns Whether the row's group, tool instance (id or name), or user contains the search text.
 */
export function matchesGroupStatsSearch(row: GroupStatsRow, searchQuery: string, instanceLabel: string): boolean {
  const query = searchQuery.trim().toLowerCase();
  return [row.group, row.toolInstance, instanceLabel, row.user].some((value) => value.toLowerCase().includes(query));
}

/**
 * Pick the message to show when the group stats table has no rows.
 *
 * @param totalRows - How many rows exist before the search filter is applied.
 * @param searchQuery - The current search text.
 * @returns The message, or an empty string when there is nothing to explain.
 */
export function emptyGroupStatsText(totalRows: number, searchQuery: string): string {
  // nothing exists at all: no group has any queued or finished work
  if (totalRows === 0) return 'No workload to show: no group has jobs queued or run.';
  // rows exist but the search hides them all
  return searchQuery.trim() === '' ? '' : 'No workload matches the search.';
}
