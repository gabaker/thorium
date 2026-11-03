import { describe, expect, it } from 'vitest';

// project imports
import {
  canCreatePipelineInGroupName,
  createBlockReason,
  deleteBlockReason,
  editBlockReason,
  hasGroupDetails,
  pipelineCreateBlockReason,
  toolDeleteBlockReason,
  toolGroupWideningNote,
  versionDeleteBlockReason,
} from './access';
import { Group } from '@models/groups';
import { UserInfo } from '@models/users';

/** Build group details with the given members. */
function group(
  name: string,
  roles: { users?: string[]; managers?: string[]; owners?: string[]; monitors?: string[] },
  entities = true,
): Group {
  const users = (combined: string[] = []) => ({ combined, direct: combined, metagroups: [] });
  return {
    name,
    owners: users(roles.owners),
    managers: users(roles.managers),
    analysts: [],
    users: users(roles.users),
    monitors: users(roles.monitors),
    allowed: {
      files: true,
      repos: true,
      tags: true,
      images: true,
      pipelines: true,
      reactions: true,
      results: true,
      comments: true,
      entities,
    },
  };
}

/** Build a signed in user in the given groups. */
function user(username: string, groups: string[], role = 'User'): UserInfo {
  return { username, role, groups } as unknown as UserInfo;
}

describe('hasGroupDetails', () => {
  it('rejects empty or partial group payloads', () => {
    expect(hasGroupDetails(null)).toBe(false);
    expect(hasGroupDetails({} as Group)).toBe(false);
    expect(hasGroupDetails(group('minerva', {}))).toBe(true);
  });
});

describe('editBlockReason', () => {
  const minerva = group('minerva', { users: ['ganymede'] });
  const juno = group('juno', { monitors: ['ganymede'], managers: ['europa'] });
  it('allows a user of every group', () => {
    expect(editBlockReason([minerva], user('ganymede', ['minerva', 'juno']))).toBe('');
  });
  it('names the visible group where the user is only a monitor', () => {
    const reason = editBlockReason([minerva, juno], user('ganymede', ['minerva', 'juno']));
    expect(reason).toContain('juno');
    expect(reason).not.toContain('`');
    expect(reason).not.toContain('minerva');
  });
  it('names a group that disallows entities, even for an admin', () => {
    const closed = group('closed', { owners: ['root'] }, false);
    expect(editBlockReason([closed], user('root', ['closed'], 'Admin'))).toContain('closed');
  });
  it('does not block on groups whose details are unknown', () => {
    expect(editBlockReason([{} as Group], user('ganymede', ['minerva']))).toBe('');
    expect(editBlockReason([juno], null)).toBe('');
  });
  it('allows a manager', () => {
    expect(editBlockReason([juno], user('europa', ['juno'], 'Developer'))).toBe('');
  });
});

describe('deleteBlockReason', () => {
  const minerva = group('minerva', { users: ['ganymede'], owners: ['europa'] });
  it('allows the submitter, an admin, and a manager or owner', () => {
    expect(deleteBlockReason([minerva], user('ganymede', ['minerva']), 'ganymede')).toBe('');
    expect(deleteBlockReason([minerva], user('root', ['minerva'], 'Admin'), 'europa')).toBe('');
    expect(deleteBlockReason([minerva], user('europa', ['minerva']), 'someone')).toBe('');
  });
  it('blocks a plain member who did not submit it, naming the group', () => {
    expect(deleteBlockReason([minerva], user('ganymede', ['minerva']), 'europa')).toContain('minerva');
    expect(deleteBlockReason([minerva], user('ganymede', ['minerva']), 'europa')).not.toContain('`');
  });
});

describe('createBlockReason', () => {
  it('allows while group details are unknown or incomplete', () => {
    expect(createBlockReason([], user('ganymede', ['minerva']))).toBe('');
    expect(createBlockReason([group('minerva', { monitors: ['ganymede'] })], user('ganymede', ['minerva', 'juno']))).toBe('');
  });
  it('allows a user who can edit in one group', () => {
    const groups = [group('minerva', { users: ['ganymede'] }), group('juno', { monitors: ['ganymede'] })];
    expect(createBlockReason(groups, user('ganymede', ['minerva', 'juno']))).toBe('');
  });
  it('blocks a user who can edit in none', () => {
    const groups = [group('juno', { monitors: ['ganymede'] })];
    expect(createBlockReason(groups, user('ganymede', ['juno']))).not.toBe('');
  });
});

describe('pipelineCreateBlockReason', () => {
  const minerva = group('minerva', { users: ['io', 'ganymede'] });
  const mars = group('mars', { monitors: ['io'] });
  const developer = { Developer: { k8s: true, bare_metal: false, windows: false, external: false, kvm: false } };
  it('blocks a global User even when they belong to a group', () => {
    expect(pipelineCreateBlockReason([minerva], user('ganymede', ['minerva']))).toContain('Analyst, Admin, or Developer');
  });
  it('allows an analyst who is a user of a group', () => {
    expect(pipelineCreateBlockReason([minerva], user('io', ['minerva'], 'Analyst'))).toBe('');
  });
  it('allows a developer who is a user of a group', () => {
    expect(pipelineCreateBlockReason([minerva], { username: 'io', role: developer, groups: ['minerva'] } as unknown as UserInfo)).toBe('');
  });
  it('blocks an analyst who only monitors or analyzes in their groups', () => {
    expect(pipelineCreateBlockReason([mars], user('io', ['mars'], 'Analyst'))).toContain('user, manager, or owner');
  });
  it('allows admins whatever the groups', () => {
    expect(pipelineCreateBlockReason([mars], user('root', ['mars'], 'Admin'))).toBe('');
  });
  it('allows while group details are unknown', () => {
    expect(pipelineCreateBlockReason([], user('io', ['minerva'], 'Analyst'))).toBe('');
  });
});

describe('canCreatePipelineInGroupName', () => {
  const groups = [group('minerva', { users: ['io'] }), group('mars', { monitors: ['io'] })];
  const io = user('io', ['minerva', 'mars'], 'Analyst');
  it('offers only the groups the user can develop in', () => {
    expect(canCreatePipelineInGroupName(groups, 'minerva', io)).toBe(true);
    expect(canCreatePipelineInGroupName(groups, 'mars', io)).toBe(false);
  });
  it('keeps a group whose details are unknown', () => {
    expect(canCreatePipelineInGroupName(groups, 'juno', io)).toBe(true);
  });
  it('rules out every group for a global User', () => {
    expect(canCreatePipelineInGroupName(groups, 'minerva', user('io', ['minerva']))).toBe(false);
  });
});

describe('versionDeleteBlockReason', () => {
  const groups = [group('minerva', { users: ['ganymede', 'test'], managers: ['europa'] })];
  it("blocks a member deleting another user's version, naming the group", () => {
    expect(versionDeleteBlockReason(groups, user('ganymede', ['minerva']), 'test')).toContain('of minerva');
  });
  it('allows the version submitter', () => {
    expect(versionDeleteBlockReason(groups, user('ganymede', ['minerva']), 'ganymede')).toBe('');
  });
  it('allows a manager whatever the submitter', () => {
    expect(versionDeleteBlockReason(groups, user('europa', ['minerva']), 'test')).toBe('');
    expect(versionDeleteBlockReason(groups, user('europa', ['minerva']), undefined)).toBe('');
  });
  it('waits for the submitter before offering a member the delete', () => {
    expect(versionDeleteBlockReason(groups, user('ganymede', ['minerva']), undefined)).toContain('checking who submitted');
  });
  it('offers the delete when the submitter cannot be read', () => {
    expect(versionDeleteBlockReason(groups, user('ganymede', ['minerva']), null)).toBe('');
  });
});

describe('toolDeleteBlockReason', () => {
  const groups = [group('minerva', { users: ['ganymede', 'test'], managers: ['europa'] })];
  const ganymede = user('ganymede', ['minerva']);
  it("blocks the tool's submitter when another user's version exists", () => {
    const reason = toolDeleteBlockReason(groups, ganymede, 'ganymede', ['ganymede', 'test']);
    expect(reason).toContain('of minerva');
    expect(reason).toContain('blocks deleting');
  });
  it('blocks while a version submitter is still being read', () => {
    expect(toolDeleteBlockReason(groups, ganymede, 'ganymede', ['ganymede', undefined])).toContain('checking who submitted');
  });
  it('allows the submitter when every version is their own', () => {
    expect(toolDeleteBlockReason(groups, ganymede, 'ganymede', ['ganymede', 'ganymede'])).toBe('');
  });
  it('allows a tool with no versions, and versions whose submitter cannot be read', () => {
    expect(toolDeleteBlockReason(groups, ganymede, 'ganymede', [])).toBe('');
    expect(toolDeleteBlockReason(groups, ganymede, 'ganymede', [null])).toBe('');
  });
  it("keeps the tool's own refusal when the user did not submit it", () => {
    expect(toolDeleteBlockReason(groups, ganymede, 'test', ['ganymede'])).not.toContain('blocks deleting');
    expect(toolDeleteBlockReason(groups, ganymede, 'test', ['ganymede'])).toContain('of minerva');
  });
  it('allows a manager and an admin whatever the versions', () => {
    expect(toolDeleteBlockReason(groups, user('europa', ['minerva']), 'test', ['ganymede', undefined])).toBe('');
    expect(toolDeleteBlockReason(groups, user('root', ['minerva'], 'Admin'), 'test', ['ganymede'])).toBe('');
  });
});

describe('toolGroupWideningNote', () => {
  it('names the group every version gains', () => {
    expect(toolGroupWideningNote('juno', false)).toBe('Every version of this tool gains `juno`, so members of `juno` can see and run it.');
  });
  it('mentions network policies for an Image tool', () => {
    expect(toolGroupWideningNote('juno', true)).toContain('network policies must also exist in `juno`');
  });
});
