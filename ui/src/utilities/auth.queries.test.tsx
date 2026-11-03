import { QueryClient } from '@tanstack/react-query';
import { AxiosError, AxiosResponse } from 'axios';
import { renderToStaticMarkup } from 'react-dom/server';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

// project imports
import { Auth, followOtherTabs, useAuth } from './auth';
import { IDENTITY_CHANGED, IDENTITY_CHANNEL } from './identityChannel';
import { agenticKeys } from '@utilities/queries/agentic';
import { createTestQueryClient, withQueryClient } from '@utilities/testing/queryClient';
import { AgenticKinds } from '@models/agentic';
import { ScopedToken } from '@models/users';

/**
 * Build the error axios raises for a response with an error status.
 *
 * @param status - The HTTP status the server answered with.
 * @returns The error.
 */
function statusError(status: number): AxiosError {
  return new AxiosError(`status ${status}`, 'ERR_BAD_RESPONSE', undefined, undefined, { status, data: {} } as AxiosResponse);
}

// the session helpers write browser storage, which the node test environment lacks, and the requests a test makes
// answer through the client's mocked get
const get = vi.hoisted(() => vi.fn());
vi.mock('@thorpi/client', () => ({
  default: { get },
  parseRequestError: vi.fn(),
  thoriumCookieAuthConfig: vi.fn(),
  clearScopedSession: vi.fn(),
  getScopedFromSession: vi.fn(() => undefined),
  setScopedSession: vi.fn(),
}));

// the cached tag data lives in local storage, which the node test environment lacks
vi.mock('./tags', () => ({ clearTagDataFromLocalStorage: vi.fn(), fetchLocalStorageTags: vi.fn() }));

// the auth requests answer as a server that accepts every login; the session check is the real one, over the client
vi.mock('@thorpi/users', async (importOriginal) => ({
  ...(await importOriginal<typeof import('@thorpi/users')>()),
  authUserPass: vi.fn(() => Promise.resolve({ status: 'ok', token: 'next-token', expires: 'Wed, 01 Jan 2030 00:00:00 GMT' })),
  createUser: vi.fn(() => Promise.resolve({ status: 'ok', token: 'next-token', expires: 'Wed, 01 Jan 2030 00:00:00 GMT' })),
  logout: vi.fn(() => Promise.resolve({ status: 200 })),
}));

// the server knows no session unless a test says otherwise
beforeEach(() => {
  get.mockReset().mockRejectedValue(statusError(401));
});

/** The auth actions captured from the provider by {@link Probe}. */
let auth: ReturnType<typeof useAuth> | undefined;

/**
 * Capture the auth context so a test can run its actions after rendering.
 *
 * @returns Nothing visible.
 */
const Probe = () => {
  auth = useAuth();
  return null;
};

/**
 * Render the auth provider over a query client that holds one user's cached data.
 *
 * @returns The client, holding a cached agentic list.
 */
function renderWithCachedData(): QueryClient {
  const client = createTestQueryClient();
  client.setQueryData(agenticKeys.list(AgenticKinds.Tool), { items: ['a tool of the first user'], error: '' });
  renderToStaticMarkup(
    withQueryClient(
      <Auth>
        <Probe />
      </Auth>,
      client,
    ),
  );
  return client;
}

describe('the query cache across identity changes', () => {
  beforeEach(() => {
    // the cookie the provider reads and writes, and the hostname the cookie names
    vi.stubGlobal('document', { cookie: '' });
    vi.stubGlobal('location', { hostname: 'thorium.example.com', pathname: '/' });
  });
  afterEach(() => {
    vi.unstubAllGlobals();
    auth = undefined;
  });
  const scoped = { token: 'scoped', expires: '2030-01-01T00:00:00Z' } as unknown as ScopedToken;
  const actions: [string, () => unknown][] = [
    ['logging out', () => auth?.logout()],
    ['revoking the session', () => auth?.revoke()],
    ['impersonating a user', () => auth?.impersonate('other-token', 'Wed, 01 Jan 2030 00:00:00 GMT')],
    ['starting a scoped token', () => auth?.setScopedToken(scoped)],
    ['ending a scoped token', () => auth?.clearScopedToken()],
    ['logging in', () => auth?.login('next', 'password', () => undefined)],
    ['finishing an OAuth login', () => auth?.completeOAuth('next-token', 'Wed, 01 Jan 2030 00:00:00 GMT')],
    ['registering an account that is signed in at once', () => auth?.register('next', 'password', () => undefined)],
  ];
  it.each(actions)('drops the cached data on %s', async (_name, action) => {
    const client = renderWithCachedData();
    expect(client.getQueryCache().getAll()).toHaveLength(1);
    await action();
    expect(client.getQueryCache().getAll()).toHaveLength(0);
  });
  // the session's cookie names a token the server no longer accepts, so whoami answers 401
  const expired: [string, () => unknown][] = [
    ['a cookie check', () => auth?.checkCookie()],
    ['a user info refresh', () => auth?.refreshUserInfo(true)],
  ];
  it.each(expired)('drops the cached data when %s finds the session expired', async (_name, action) => {
    document.cookie = 'THORIUM_TOKEN=expired-token';
    const client = renderWithCachedData();
    await action();
    expect(client.getQueryCache().getAll()).toHaveLength(0);
  });
});

/**
 * Wait for the next message on the identity channel, as another tab would receive it.
 *
 * @returns The message's data, or `'none'` when nothing arrives within a second.
 */
function nextIdentityMessage(): Promise<unknown> {
  const tab = new BroadcastChannel(IDENTITY_CHANNEL);
  return new Promise((resolve) => {
    const timer = setTimeout(() => {
      tab.close();
      resolve('none');
    }, 1000);
    tab.onmessage = (event: MessageEvent) => {
      clearTimeout(timer);
      tab.close();
      resolve(event.data);
    };
  });
}

describe('identity changes across tabs', () => {
  beforeEach(() => {
    vi.stubGlobal('document', { cookie: 'THORIUM_TOKEN=first-token' });
    vi.stubGlobal('location', { hostname: 'thorium.example.com', pathname: '/' });
  });
  afterEach(() => {
    vi.unstubAllGlobals();
    auth = undefined;
  });
  it('drops the cached data and resyncs the session when another tab changes the identity', async () => {
    const client = createTestQueryClient();
    client.setQueryData(agenticKeys.list(AgenticKinds.Tool), { items: ['a tool of the first user'], error: '' });
    const resynced = new Promise<void>((resolve) => {
      const stop = followOtherTabs(client, () => {
        stop();
        resolve();
      });
    });
    const otherTab = new BroadcastChannel(IDENTITY_CHANNEL);
    otherTab.postMessage(IDENTITY_CHANGED);
    otherTab.close();
    await resynced;
    expect(client.getQueryCache().getAll()).toHaveLength(0);
  });
  it('ignores other messages on the identity channel', async () => {
    const client = createTestQueryClient();
    client.setQueryData(agenticKeys.list(AgenticKinds.Tool), { items: ['a tool of the first user'], error: '' });
    let resyncs = 0;
    let cachedAtFirstResync = -1;
    const resynced = new Promise<void>((resolve) => {
      const stop = followOtherTabs(client, () => {
        resyncs += 1;
        if (resyncs === 1) cachedAtFirstResync = client.getQueryCache().getAll().length;
        setTimeout(() => {
          stop();
          resolve();
        }, 100);
      });
    });
    // one tab's messages arrive in order, so the announcement after the foreign message is handled after it
    const otherTab = new BroadcastChannel(IDENTITY_CHANNEL);
    otherTab.postMessage({ token: 'x' });
    otherTab.postMessage(IDENTITY_CHANGED);
    otherTab.close();
    await resynced;
    expect(resyncs).toBe(1);
    expect(cachedAtFirstResync).toBe(0);
  });
  it('keeps the cached data when only a foreign message arrives', async () => {
    const client = createTestQueryClient();
    client.setQueryData(agenticKeys.list(AgenticKinds.Tool), { items: ['a tool of the first user'], error: '' });
    const resync = vi.fn();
    const stop = followOtherTabs(client, resync);
    const otherTab = new BroadcastChannel(IDENTITY_CHANNEL);
    otherTab.postMessage({ token: 'x' });
    otherTab.close();
    await new Promise((resolve) => setTimeout(resolve, 200));
    stop();
    expect(resync).not.toHaveBeenCalled();
    expect(client.getQueryCache().getAll()).toHaveLength(1);
  });
  const announced: [string, () => unknown][] = [
    ['logging out', () => auth?.logout()],
    ['revoking the session', () => auth?.revoke()],
    ['logging in', () => auth?.login('next', 'password', () => undefined)],
    ['registering an account that is signed in at once', () => auth?.register('next', 'password', () => undefined)],
    ['impersonating a user', () => auth?.impersonate('other-token', 'Wed, 01 Jan 2030 00:00:00 GMT')],
    ['finishing an OAuth login', () => auth?.completeOAuth('next-token', 'Wed, 01 Jan 2030 00:00:00 GMT')],
    ['finding the session expired', () => auth?.checkCookie()],
  ];
  it.each(announced)('tells the other tabs on %s', async (_name, action) => {
    renderWithCachedData();
    const message = nextIdentityMessage();
    await action();
    expect(await message).not.toBe('none');
  });
  const checks: [string, () => unknown][] = [
    ['a cookie check', () => auth?.checkCookie()],
    ['a user info refresh', () => auth?.refreshUserInfo(true)],
  ];
  it.each(checks)('ends the session here and in the other tabs when %s gets a 401', async (_name, check) => {
    const client = renderWithCachedData();
    const message = nextIdentityMessage();
    await check();
    expect(await message).toBe(IDENTITY_CHANGED);
    expect(client.getQueryCache().getAll()).toHaveLength(0);
    expect(document.cookie).toContain('max-age=0');
  });
  const outages: [string, string, () => unknown, () => AxiosError][] = checks.flatMap(([name, check]) => [
    [name, 'a network error', check, () => new AxiosError('Network Error', 'ERR_NETWORK')] as [
      string,
      string,
      () => unknown,
      () => AxiosError,
    ],
    [name, 'a 500', check, () => statusError(500)] as [string, string, () => unknown, () => AxiosError],
  ]);
  it.each(outages)('keeps the session when %s meets %s', async (_name, _failure, check, failure) => {
    get.mockRejectedValue(failure());
    const queries = renderWithCachedData();
    const message = nextIdentityMessage();
    await check();
    expect(await message).toBe('none');
    expect(queries.getQueryCache().getAll()).toHaveLength(1);
    expect(document.cookie).toBe('THORIUM_TOKEN=first-token');
  });
  it('keeps a scoped token to its own tab', async () => {
    renderWithCachedData();
    const message = nextIdentityMessage();
    auth?.setScopedToken({ token: 'scoped', expires: '2030-01-01T00:00:00Z' } as unknown as ScopedToken);
    expect(await message).toBe('none');
  });
});
