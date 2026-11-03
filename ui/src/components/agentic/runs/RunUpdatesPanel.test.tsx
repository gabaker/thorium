import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it } from 'vitest';

// project imports
import { UpdatesContent } from './RunUpdatesPanel';
import { StatusUpdate } from '@models/reactions';

/** A status update the run recorded. */
const update = (overrides: Partial<StatusUpdate> = {}): StatusUpdate => ({
  group: 'minerva',
  pipeline: 'p',
  reaction: 'r',
  timestamp: '2026-10-29T07:13:39Z',
  update: {},
  action: 'Created',
  ...overrides,
});

/** Render the updates body to markup. */
const render = (updates: StatusUpdate[] | null, error = ''): string =>
  renderToStaticMarkup(<UpdatesContent updates={updates} error={error} />);

describe('UpdatesContent', () => {
  it('shows only the error when the read failed', () => {
    const markup = render([], 'Get Run Status Updates: injected failure');
    expect(markup).toContain('data-testid="run-updates-error"');
    expect(markup).toContain('injected failure');
    // a failed read does not know whether updates exist, so no empty note is shown
    expect(markup).not.toContain('No status updates yet.');
  });
  it('says there are no updates when the read succeeded empty', () => {
    const markup = render([]);
    expect(markup).toContain('No status updates yet.');
    expect(markup).not.toContain('run-updates-error');
  });
  it('shows a loading note before the first read finishes', () => {
    const markup = render(null);
    expect(markup).toContain('Loading status updates...');
    expect(markup).not.toContain('No status updates yet.');
  });
  it('lists the updates in a table', () => {
    const markup = render([update()]);
    expect(markup).toContain('<table');
    expect(markup).toContain('Created');
  });
  it('keeps the updates shown beside the error of a failed refresh', () => {
    const markup = render([update()], 'Get Run Status Updates: injected failure');
    expect(markup).toContain('run-updates-error');
    expect(markup).toContain('Created');
  });
});
