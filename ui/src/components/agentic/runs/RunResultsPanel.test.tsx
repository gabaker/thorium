import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { MemoryRouter } from 'react-router-dom';
import { ServerStyleSheet } from 'styled-components';
import { describe, expect, it, vi } from 'vitest';

// project imports
import { InputResults, RunInputResults, loadRunResults } from './RunResultsPanel';
import { Reaction, ReactionStatus } from '@models/reactions';
import { getRunStatusUpdates, getSampleRunResults } from '@thorpi/runs';

// the reads need the API, which is not under test here
vi.mock('@thorpi/runs', () => ({ getRunStatusUpdates: vi.fn(), getSampleRunResults: vi.fn(), getRepoRunResults: vi.fn() }));

/** The sample every rendered input is on. */
const SHA = 'e8d3ec08'.padEnd(64, '0');

/** One input's results, with a single stored result by default. */
const entry = (overrides: Partial<InputResults> = {}): InputResults => ({
  input: SHA,
  repo: false,
  results: { results: { 'jup-stub-tool': [{ id: 'r1', result: { flags: [] }, files: [], uploaded: '2026-10-29T07:13:39Z' }] } },
  error: '',
  others: 0,
  overlapped: false,
  ...overrides,
});

/** Render one input's results to markup. */
const render = (input: InputResults, status = ReactionStatus.Completed, confirmed = true, name?: string): string =>
  renderToStaticMarkup(
    <MemoryRouter>
      <RunInputResults entry={input} status={status} confirmed={confirmed} name={name} />
    </MemoryRouter>,
  );

describe('RunInputResults', () => {
  it('spaces each input from the next but leaves the last with no margin to add to the card padding', () => {
    const sheet = new ServerStyleSheet();
    renderToStaticMarkup(
      sheet.collectStyles(
        <MemoryRouter>
          <RunInputResults entry={entry()} status={ReactionStatus.Completed} confirmed={true} />
        </MemoryRouter>,
      ),
    );
    const css = sheet.getStyleTags();
    sheet.seal();
    // the margin applies only to blocks that have a sibling after them
    expect(css).toMatch(/:not\(:last-child\)\{margin-bottom:1rem;?\}/);
    // no inline margin keeps the last block's margin
    expect(render(entry())).not.toMatch(/data-testid="run-input-results" style=/);
  });
  it('shows only the error when the read failed', () => {
    const markup = render(entry({ results: null, error: 'Get Run Results: injected failure' }));
    expect(markup).toContain('data-testid="run-results-error"');
    expect(markup).toContain('injected failure');
    // a failed read does not know whether a result exists, so no no-results note is shown
    expect(markup).not.toContain('run-no-results');
    expect(markup).not.toContain('pruned');
  });
  it('says no result is kept when the read succeeded empty', () => {
    const markup = render(entry({ results: { results: {} } }));
    expect(markup).toContain('data-testid="run-no-results"');
    expect(markup).not.toContain('run-results-error');
  });
  it('shows a confirmed run result without a caveat', () => {
    const markup = render(entry());
    expect(markup).toContain('data-testid="run-result-json"');
    expect(markup).not.toContain('run-unconfirmed-results');
  });
  it('warns that a cancelled run may be showing another run result', () => {
    const markup = render(entry(), ReactionStatus.Failed, false);
    expect(markup).toContain('data-testid="run-unconfirmed-results"');
    expect(markup).toContain('This run failed, so it may not have stored the result below');
    expect(markup).toContain('data-testid="run-result-json"');
  });
  it('warns when no job of an unfailed run reported completing', () => {
    expect(render(entry(), ReactionStatus.Completed, false)).toContain('No job of this run reported completing');
  });
  it('names the input file next to its SHA256 when it has a name', () => {
    expect(render(entry(), ReactionStatus.Completed, true, 'prompt.txt')).toContain('(prompt.txt)');
    expect(render(entry())).not.toContain('run-result-input-name');
  });
});

describe('loadRunResults', () => {
  const run = { id: 'run1', group: 'minerva', status: ReactionStatus.Completed, samples: [SHA], repos: [] } as unknown as Reaction;
  it('returns the error and reads no results when the status updates cannot be read', async () => {
    vi.mocked(getRunStatusUpdates).mockImplementation((_group, _id, onError) => {
      onError('Get Run Status Updates: injected failure');
      return Promise.resolve([]);
    });
    vi.mocked(getSampleRunResults).mockClear();
    const found = await loadRunResults(run, 'jup-stub-tool');
    expect(found.error).toContain('injected failure');
    expect(found.inputs).toEqual([]);
    expect(found.confirmed).toBe(false);
    // results from other runs must not be read without the run's lifetime
    expect(getSampleRunResults).not.toHaveBeenCalled();
  });
  it('reads the results of a run whose updates were read', async () => {
    vi.mocked(getRunStatusUpdates).mockResolvedValue([
      { action: 'ReactionCreated', timestamp: '2026-10-29T06:21:21Z' },
      { action: 'JobCompleted', timestamp: '2026-10-29T06:21:25Z' },
      { action: 'ReactionCompleted', timestamp: '2026-10-29T06:21:28Z' },
    ] as never);
    vi.mocked(getSampleRunResults).mockResolvedValue({
      results: {
        'jup-stub-tool': [
          { id: 'mine', result: {}, files: [], uploaded: '2026-10-29T06:21:25Z' },
          { id: 'other', result: {}, files: [], uploaded: '2026-10-28T20:42:17Z' },
        ],
      },
    });
    const found = await loadRunResults(run, 'jup-stub-tool');
    expect(found.error).toBe('');
    expect(found.confirmed).toBe(true);
    expect(found.inputs[0].others).toBe(1);
    expect(found.inputs[0].results?.results['jup-stub-tool'].map((r) => r.id)).toEqual(['mine']);
  });
});
