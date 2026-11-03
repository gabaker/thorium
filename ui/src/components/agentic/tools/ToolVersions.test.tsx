import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it } from 'vitest';

// project imports
import { ToolVersions } from './ToolVersions';
import { ToolInstanceSummary } from '@models/tools';

/** Two versions of a tool, newest first. */
const instances: ToolInstanceSummary[] = [
  { id: 'bbbbbbbb-0000', version: { SemVer: '1.1.0' }, spec_hash: 'f'.repeat(64), deprecated: false, created: '2026-10-28T15:31:15Z' },
  { id: 'aaaaaaaa-0000', version: { SemVer: '1.0.0' }, spec_hash: 'e'.repeat(64), deprecated: false, created: '2026-10-28T15:30:00Z' },
];

/**
 * Render the versions table to markup.
 *
 * @param deleteReasonOf - Why each version can't be deleted.
 */
const render = (deleteReasonOf?: (id: string) => string): string =>
  renderToStaticMarkup(
    <ToolVersions
      instances={instances}
      latest="bbbbbbbb-0000"
      selected="bbbbbbbb-0000"
      runnable={true}
      onSelect={() => undefined}
      onDeprecate={() => undefined}
      onDelete={() => undefined}
      onRun={() => undefined}
      deleteReasonOf={deleteReasonOf}
    />,
  );

/**
 * Find the opening tag of the element with a test id.
 *
 * @param markup - The rendered markup.
 * @param testId - The element's test id.
 */
const tagOf = (markup: string, testId: string): string => {
  const match = new RegExp(`<[^>]*data-testid="${testId}"[^>]*>`).exec(markup);
  return match?.[0] ?? '';
};

describe('ToolVersions', () => {
  it('disables only the versions whose delete is blocked, with the reason as the tooltip', () => {
    const markup = render((id) => (id === 'aaaaaaaa-0000' ? 'Only the submitter or a manager or owner of `minerva` can delete this' : ''));
    const blocked = tagOf(markup, 'delete-version-1.0.0');
    expect(blocked).toContain('disabled');
    expect(blocked).toContain('`minerva`');
    expect(tagOf(markup, 'delete-version-1.1.0')).not.toContain('disabled');
  });
  it('offers every delete when no reason is given', () => {
    const markup = render();
    expect(tagOf(markup, 'delete-version-1.0.0')).not.toContain('disabled');
    expect(tagOf(markup, 'delete-version-1.1.0')).not.toContain('disabled');
  });
  it('lets the action buttons wrap so Delete stays reachable in a narrow window', () => {
    const actions = tagOf(render(), 'version-actions-1.0.0');
    expect(actions).not.toContain('nowrap');
  });
  it('keeps the created time on one line', () => {
    expect(render()).toContain('<td style="white-space:nowrap">2026-10-28 15:31:15</td>');
  });

  it('lets the action buttons of each row wrap instead of overflowing a narrow table', () => {
    const markup = render();
    const actions = tagOf(markup, 'version-actions-1.1.0');
    expect(actions).toContain('flex-wrap:wrap');
    expect(actions).not.toContain('nowrap');
  });
});
