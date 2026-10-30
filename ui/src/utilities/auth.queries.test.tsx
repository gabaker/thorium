import { QueryClient } from '@tanstack/react-query';
import { renderToStaticMarkup } from 'react-dom/server';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

// project imports
import { Auth, useAuth } from './auth';
import { agenticKeys } from '@utilities/queries/agentic';
import { createTestQueryClient, withQueryClient } from '@utilities/testing/queryClient';
import { AgenticKinds } from '@models/agentic';
import { ScopedToken } from '@models/users';

// the session helpers write browser storage, which the node test environment lacks
vi.mock('@thorpi/client', () => ({
  default: {},
  clearScopedSession: vi.fn(),
  getScopedFromSession: vi.fn(() => undefined),
  setScopedSession: vi.fn(),
}));

// the auth requests answer as a server that accepts every login
vi.mock('@thorpi/users', () => ({
  authUserPass: vi.fn(() => Promise.resolve({ status: 'ok', token: 'next-token', expires: 'Wed, 01 Jan 2030 00:00:00 GMT' })),
  createUser: vi.fn(() => Promise.resolve({ status: 'ok', token: 'next-token', expires: 'Wed, 01 Jan 2030 00:00:00 GMT' })),
  logout: vi.fn(() => Promise.resolve({ status: 200 })),
  whoami: vi.fn(() => Promise.resolve(null)),
}));

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
  ];
  it.each(actions)('drops the cached data on %s', async (_name, action) => {
    const client = renderWithCachedData();
    expect(client.getQueryCache().getAll()).toHaveLength(1);
    await action();
    expect(client.getQueryCache().getAll()).toHaveLength(0);
  });
});
