import { describe, expect, it, vi } from 'vitest';

// project imports
import { formatExpire, grantableDeveloperPermissions, isRunToken, runProfileRefresh, splitScopedTokens } from './scopedTokens';
import { RoleKey, ScopedToken, ScopedTokenRoleKey, ThoriumRole } from '@models/users';

/** Build a scoped token, adding a run scope when `run` is set. */
const token = (name: string, run = false): ScopedToken => ({
  name,
  owner: 'ada',
  role: ScopedTokenRoleKey.User,
  groups: ['g1'],
  token: 'secret-value',
  token_expiration: '2030-01-02T03:04:05Z',
  expires: null,
  ...(run ? { run: { reaction: 'r-1', tool_instances: ['t-1', 't-2'] } } : {}),
});

describe('isRunToken', () => {
  it('is true only for a token with a run scope', () => {
    expect(isRunToken(token('mine'))).toBe(false);
    expect(isRunToken(token('run-abc', true))).toBe(true);
  });
});

describe('splitScopedTokens', () => {
  it('separates run tokens from user tokens and sorts each by name', () => {
    const { userTokens, runTokens } = splitScopedTokens([token('b'), token('run-b', true), token('a'), token('run-a', true)]);
    expect(userTokens.map((t) => t.name)).toEqual(['a', 'b']);
    expect(runTokens.map((t) => t.name)).toEqual(['run-a', 'run-b']);
  });
  it('puts the active token first among the user tokens', () => {
    const tokens = [token('a'), token('b'), token('c')];
    const { userTokens } = splitScopedTokens(tokens, tokens[2]);
    expect(userTokens.map((t) => t.name)).toEqual(['c', 'a', 'b']);
  });
  it('does not change the list it was given', () => {
    const tokens = [token('b'), token('a')];
    splitScopedTokens(tokens);
    expect(tokens.map((t) => t.name)).toEqual(['b', 'a']);
  });
});

describe('formatExpire', () => {
  it('shows the date part, or a dash when there is no expiration', () => {
    expect(formatExpire('2030-01-02T03:04:05Z')).toBe('2030-01-02');
    expect(formatExpire(null)).toBe('-');
  });
});

describe('runProfileRefresh', () => {
  it('reloads whoami past its cache and the token list, then clears the busy state', async () => {
    const calls: string[] = [];
    await runProfileRefresh({
      refreshUserInfo: vi.fn((force?: boolean) => {
        calls.push(`whoami:${String(force)}`);
        return Promise.resolve();
      }),
      reloadTokens: vi.fn(() => {
        calls.push('tokens');
        return Promise.resolve();
      }),
      setRefreshing: (busy) => calls.push(`busy:${String(busy)}`),
    });
    expect(calls[0]).toBe('busy:true');
    expect(calls.slice(1, 3).sort()).toEqual(['tokens', 'whoami:true']);
    expect(calls[3]).toBe('busy:false');
  });
  it('stays busy until both reloads finish', async () => {
    const busy: boolean[] = [];
    let finishTokens: () => void = () => {};
    const tokensDone = new Promise<void>((resolve) => {
      finishTokens = resolve;
    });
    const refresh = runProfileRefresh({
      refreshUserInfo: async () => {},
      reloadTokens: async () => await tokensDone,
      setRefreshing: (value) => busy.push(value),
    });
    // let the quick whoami reload settle while the token reload is still pending
    await Promise.resolve();
    await Promise.resolve();
    expect(busy).toEqual([true]);
    finishTokens();
    await refresh;
    expect(busy).toEqual([true, false]);
  });
  it('clears the busy state and still reloads the other when one reload fails', async () => {
    const reloadTokens = vi.fn(async () => {});
    const busy: boolean[] = [];
    await runProfileRefresh({
      refreshUserInfo: () => Promise.reject(new Error('whoami down')),
      reloadTokens,
      setRefreshing: (value) => busy.push(value),
    });
    expect(reloadTokens).toHaveBeenCalledTimes(1);
    expect(busy).toEqual([true, false]);
  });
});

// a user's role is serialized as its plain name unless it is a developer, whose role is an object
describe('grantableDeveloperPermissions', () => {
  it('grants every permission to an admin or analyst', () => {
    const all = { k8s: true, bare_metal: true, windows: true, external: true, kvm: true };
    expect(grantableDeveloperPermissions(RoleKey.Admin as unknown as ThoriumRole)).toEqual(all);
    expect(grantableDeveloperPermissions(RoleKey.Analyst as unknown as ThoriumRole)).toEqual(all);
  });
  it('grants a developer only the permissions it holds', () => {
    const held = { k8s: true, bare_metal: false, windows: false, external: false, kvm: false };
    expect(grantableDeveloperPermissions({ Developer: held })).toEqual(held);
  });
  it('grants nothing to a user, a disabled user, or an unknown role', () => {
    expect(grantableDeveloperPermissions(RoleKey.User as unknown as ThoriumRole)).toBeNull();
    expect(grantableDeveloperPermissions(RoleKey.Disabled as unknown as ThoriumRole)).toBeNull();
    expect(grantableDeveloperPermissions(undefined)).toBeNull();
  });
});
