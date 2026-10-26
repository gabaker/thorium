import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { MemoryRouter } from 'react-router-dom';
import { ServerStyleSheet } from 'styled-components';
import { describe, expect, it, vi } from 'vitest';

// project imports
import ProfileRefreshButton from './ProfileRefreshButton';
import RunTokenTable from './RunTokenTable';
import TokenView from './TokenView';
import { ScopedTokenList } from './useScopedTokens';
import { ScopedToken, ScopedTokenRoleKey } from '@models/users';

// the token view reads the signed-in user from the auth context, which a bare render has no provider for
vi.mock('@utilities/auth', () => ({
  useAuth: () => ({
    userInfo: { token: 'root-token', token_expiration: '2031-01-01', groups: ['g1'] },
    scopedToken: undefined,
    clearScopedToken: () => {},
    setScopedToken: () => {},
    revoke: async () => {},
  }),
}));

/** Build a scoped token, adding a run scope when `run` is set. */
const token = (name: string, run = false): ScopedToken => ({
  name,
  owner: 'ada',
  role: ScopedTokenRoleKey.User,
  groups: ['g1'],
  token: 'secret-value',
  token_expiration: '2030-01-02T03:04:05Z',
  expires: null,
  ...(run ? { run: { reaction: '11111111-2222-3333-4444-555555555555', tool_instances: ['t-1', 't-2'] } } : {}),
});

/** Build a token list state, defaulting to a loaded list with no error. */
const list = (tokens: ScopedToken[], rest: Partial<ScopedTokenList> = {}): ScopedTokenList => ({
  tokens,
  loaded: true,
  loading: false,
  error: '',
  reload: async () => {},
  ...rest,
});

/** Render the token view for a token list. */
const render = (state: ScopedTokenList): string =>
  renderToStaticMarkup(
    <MemoryRouter>
      <TokenView list={state} />
    </MemoryRouter>,
  );

/** The markup of the table carrying `label` as its accessible name. */
const tableOf = (html: string, label: string): string => {
  const start = html.indexOf(`aria-label="${label}"`);
  expect(start).toBeGreaterThan(-1);
  return html.slice(start, html.indexOf('</table>', start));
};

describe('TokenView scoped token list', () => {
  it('gives a user token its activate, edit, and delete controls', () => {
    const rows = tableOf(render(list([token('mine')])), 'Scoped tokens');
    expect(rows).toContain('mine');
    expect(rows).toContain('Activate');
    expect(rows).toContain('Edit');
    expect(rows).toContain('Delete');
  });
  it('lists a run token read-only, with no controls and no token value', () => {
    const html = render(list([token('mine'), token('run-abc', true)]));
    const runRows = tableOf(html, 'Run tokens');
    expect(runRows).toContain('run-abc');
    expect(runRows).toContain('11111111-2222-3333-4444-555555555555');
    expect(runRows).not.toContain('<button');
    expect(runRows).not.toContain('<input');
    expect(html).not.toContain('secret-value');
    // the run token is not repeated in the table of tokens the user manages
    expect(tableOf(html, 'Scoped tokens')).not.toContain('run-abc');
  });
  it('omits the run token section when there are no run tokens', () => {
    expect(render(list([token('mine')]))).not.toContain('Run tokens');
  });
  it('shows a loading note, not the empty message, before the first load finishes', () => {
    const html = render(list([], { loaded: false, loading: true }));
    expect(html).toContain('Loading scoped tokens');
    expect(html).not.toContain('No scoped tokens yet');
  });
  it('shows the empty message once an empty list has loaded', () => {
    expect(render(list([]))).toContain('No scoped tokens yet.');
  });
  it('shows the load error and no empty message when the first load failed', () => {
    const html = render(list([], { loaded: false, error: 'List Scoped Tokens failed' }));
    expect(html).toContain('List Scoped Tokens failed');
    expect(html).not.toContain('No scoped tokens yet');
  });
  it('marks the table busy while a reload is in flight', () => {
    expect(render(list([token('mine')], { loading: true }))).toContain('aria-busy="true"');
  });
  it('lists only run tokens read-only when the user has made none', () => {
    const html = render(list([token('run-abc', true)]));
    expect(html).toContain('No scoped tokens yet.');
    expect(tableOf(html, 'Run tokens')).not.toContain('<button');
  });
});

describe('RunTokenTable', () => {
  it('shows the number of tool instances a run token may run', () => {
    const rows = renderToStaticMarkup(<RunTokenTable tokens={[token('run-abc', true)]} />);
    expect(rows).toContain('<td>2</td>');
  });
  it('gives the groups cell a minimum width so an ordinary group name is not split mid-word', () => {
    // render with a style sheet so the generated CSS can be read back
    const sheet = new ServerStyleSheet();
    const html = renderToStaticMarkup(sheet.collectStyles(<RunTokenTable tokens={[token('run-abc', true)]} />));
    const css = sheet.getStyleTags();
    sheet.seal();
    // find the class of the cell that holds the group pills
    const cellClasses = /<td class="([^"]+)"><div[^>]*><span[^>]*title="g1"/.exec(html)?.[1].split(' ') ?? [];
    expect(cellClasses.length).toBeGreaterThan(0);
    // one of the cell's classes must set a minimum width
    const sized = cellClasses.some((cls) => new RegExp(`\\.${cls}\\{[^}]*min-width:6\\.5rem`).test(css));
    expect(sized).toBe(true);
  });
});

describe('ProfileRefreshButton', () => {
  it('is enabled and labelled Refresh when idle', () => {
    const html = renderToStaticMarkup(<ProfileRefreshButton refreshing={false} onRefresh={() => {}} />);
    expect(html).toContain('Refresh');
    expect(html).not.toContain('disabled');
    expect(html).toContain('aria-busy="false"');
  });
  it('is disabled and shows progress while refreshing', () => {
    const html = renderToStaticMarkup(<ProfileRefreshButton refreshing={true} onRefresh={() => {}} />);
    expect(html).toContain('Refreshing…');
    expect(html).toContain('disabled');
    expect(html).toContain('aria-busy="true"');
  });
});
