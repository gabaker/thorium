// project imports
import { Group } from '@models/groups';

/**
 * Count the total number of members in a group across every role.
 *
 * Mirrors the backend `Group::member_count`, summing owners + managers + analysts + users +
 * monitors (each role's `combined` list, except analysts which is a flat list).
 *
 * @param group - The group to count members of.
 * @returns The total member count.
 */
export function getGroupMemberCount(group: Group): number {
  return (
    group.owners.combined.length +
    group.managers.combined.length +
    group.analysts.length +
    group.users.combined.length +
    group.monitors.combined.length
  );
}

/**
 * Collect the distinct group names referenced across a set of submissions.
 *
 * @param submissions - Submissions, each carrying a `groups` array.
 * @returns The de-duplicated list of group names, in first-seen order.
 */
export function getUniqueSubmissionGroups(submissions: { groups: string[] }[]): string[] {
  const uniqueGroupsList: string[] = [];
  for (const submission of submissions) {
    uniqueGroupsList.push(...submission.groups.filter((group: string) => !uniqueGroupsList.includes(group)));
  }
  return uniqueGroupsList;
}
