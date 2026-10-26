// project imports
import { canDeleteEntityInGroup, canDevelopAnyInGroup, canEditEntityInGroup, isAdmin, isDeveloperAny } from '@utilities/permissions';
import { Group } from '@models/groups';
import { UserInfo } from '@models/users';

/**
 * Tell whether a value read from the groups API carries the membership lists the checks need.
 *
 * @param group - A group read from the API; it may be missing or partial.
 * @returns `true` when the group has its name, membership lists, and allow flags.
 */
export function hasGroupDetails(group: Group | null | undefined): group is Group {
  return Boolean(group?.name && group.owners?.combined && group.managers?.combined && group.users?.combined && group.allowed);
}

/**
 * Name the groups that block an action as a readable list.
 *
 * @param blocking - The groups that block the action.
 * @returns The group names, joined with commas.
 */
function nameGroups(blocking: Group[]): string {
  return blocking.map((group) => group.name).join(', ');
}

/**
 * Explain why a user cannot edit an entity, naming the visible groups that block it.
 *
 * A group whose details are unknown (not loaded or not readable) never blocks, so the page offers
 * the action and shows the API's refusal if it comes.
 *
 * @param groups - The details of the entity's groups.
 * @param userInfo - The signed in user.
 * @returns The reason the edit is blocked, or an empty string when it is allowed.
 */
export function editBlockReason(groups: Group[], userInfo: UserInfo | null): string {
  if (!userInfo) return '';
  const blocking = groups.filter((group) => hasGroupDetails(group) && !canEditEntityInGroup(group, userInfo));
  if (blocking.length === 0) return '';
  return `You can't edit this in ${nameGroups(blocking)}: editing needs the user, analyst, manager, or owner role in a group that allows entities.`;
}

/**
 * Explain why a user cannot delete an entity, naming the visible groups that block it.
 *
 * The submitter and admins may always delete; anyone else must manage or own every group.
 *
 * @param groups - The details of the entity's groups.
 * @param userInfo - The signed in user.
 * @param submitter - The user that submitted the entity.
 * @returns The reason the delete is blocked, or an empty string when it is allowed.
 */
export function deleteBlockReason(groups: Group[], userInfo: UserInfo | null, submitter: string): string {
  if (!userInfo || isAdmin(userInfo) || userInfo.username === submitter) return '';
  const blocking = groups.filter((group) => hasGroupDetails(group) && !canDeleteEntityInGroup(group, userInfo));
  if (blocking.length === 0) return '';
  return `Only the submitter or a manager or owner of ${nameGroups(blocking)} can delete this`;
}

/**
 * Explain why a user cannot create an entity at all.
 *
 * Creating needs the edit role in at least one group that allows entities.
 *
 * @param groups - The details of the user's groups; empty while they load.
 * @param userInfo - The signed in user.
 * @returns The reason creating is blocked, or an empty string when it is allowed or not yet known.
 */
export function createBlockReason(groups: Group[], userInfo: UserInfo | null): string {
  if (!userInfo) return '';
  const known = groups.filter((group) => hasGroupDetails(group));
  // with no group details yet (loading, or unreadable) the page offers the action
  if (known.length === 0 || known.length < userInfo.groups.length) return '';
  if (known.some((group) => canEditEntityInGroup(group, userInfo))) return '';
  return "You can't create here: creating needs the user, analyst, manager, or owner role in a group that allows entities";
}

/**
 * Explain why a user cannot create a pipeline at all.
 *
 * The API checks every stage's tool for a developer role for the tool's scaler and for the user,
 * manager, or owner role in the pipeline's group; a group-level analyst role does not count. The
 * scaler is not known before the stages are chosen, so the role check is the coarse "develops for some
 * scaler" level.
 *
 * @param groups - The details of the user's groups; empty while they load.
 * @param userInfo - The signed in user.
 * @returns The reason creating is blocked, or an empty string when it is allowed or not yet known.
 */
export function pipelineCreateBlockReason(groups: Group[], userInfo: UserInfo | null): string {
  if (!userInfo || isAdmin(userInfo)) return '';
  // the role needs no group details, so it is checked first
  if (!isDeveloperAny(userInfo)) {
    return "You can't create a pipeline: creating needs the Analyst, Admin, or Developer role for the scaler its tools run on.";
  }
  const known = groups.filter((group) => hasGroupDetails(group));
  // with no group details yet (loading, or unreadable) the page offers the action
  if (known.length === 0 || known.length < userInfo.groups.length) return '';
  if (known.some((group) => canDevelopAnyInGroup(group, userInfo))) return '';
  return "You can't create a pipeline here: creating needs the user, manager, or owner role in a group (the group analyst role does not count).";
}

/**
 * Tell whether a user may create a pipeline in a group.
 *
 * A group whose details are unknown (not loaded or not readable) is allowed, so the API's refusal is
 * shown if it comes.
 *
 * @param groups - The details of the user's groups.
 * @param name - The name of the group to create the pipeline in.
 * @param userInfo - The signed in user.
 * @returns `false` only when the group is known and the user may not create a pipeline in it.
 */
export function canCreatePipelineInGroupName(groups: Group[], name: string, userInfo: UserInfo | null): boolean {
  if (!userInfo) return true;
  const found = groups.find((group) => group.name === name);
  return !found || canDevelopAnyInGroup(found, userInfo);
}

/**
 * Explain why a user cannot delete one version of a tool, naming the visible groups that block it.
 *
 * A version shares its tool's groups but has its own submitter. While that submitter is still being
 * read, the delete is offered only when the user could delete another user's version anyway. A
 * version whose submitter can't be read is offered, and the API's refusal is shown if it comes.
 *
 * @param groups - The details of the tool's groups.
 * @param userInfo - The signed in user.
 * @param submitter - The user that submitted the version, `undefined` while it is being read, or `null` when it can't be read.
 * @returns The reason the delete is blocked, or an empty string when it is allowed.
 */
export function versionDeleteBlockReason(groups: Group[], userInfo: UserInfo | null, submitter: string | null | undefined): string {
  if (submitter === null) return '';
  if (submitter !== undefined) return deleteBlockReason(groups, userInfo, submitter);
  // the reason that would apply to a version submitted by someone else
  const otherReason = deleteBlockReason(groups, userInfo, '');
  return otherReason === '' ? '' : `${otherReason} (checking who submitted this version)`;
}

/**
 * Explain why a user cannot delete a whole tool, naming the visible groups that block it.
 *
 * Deleting a tool deletes every version first, so it is blocked by the tool's own delete check and by
 * the first version whose delete check blocks (including one still waiting on its submitter).
 *
 * @param groups - The details of the tool's groups.
 * @param userInfo - The signed in user.
 * @param submitter - The user that submitted the tool.
 * @param versionSubmitters - Each version's submitter: `undefined` while being read, or `null` when it can't be read.
 * @returns The reason the delete is blocked, or an empty string when it is allowed.
 */
export function toolDeleteBlockReason(
  groups: Group[],
  userInfo: UserInfo | null,
  submitter: string,
  versionSubmitters: (string | null | undefined)[],
): string {
  const toolReason = deleteBlockReason(groups, userInfo, submitter);
  if (toolReason !== '') return toolReason;
  // the first version that the user could not delete decides, so no version is deleted before a refusal
  for (const versionSubmitter of versionSubmitters) {
    const reason = versionDeleteBlockReason(groups, userInfo, versionSubmitter);
    if (reason !== '') return `A version of this tool blocks deleting it: ${reason}`;
  }
  return '';
}

/**
 * Describe what adding a group to a tool does, shown before the change is saved.
 *
 * @param group - The group being added.
 * @param image - Whether the tool is an Image tool, whose versions run with network policies.
 * @returns The note naming the group.
 */
export function toolGroupWideningNote(group: string, image: boolean): string {
  const note = `Every version of this tool gains \`${group}\`, so members of \`${group}\` can see and run it.`;
  return image ? `${note} Its versions' network policies must also exist in \`${group}\`.` : note;
}
