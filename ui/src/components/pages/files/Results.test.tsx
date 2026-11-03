import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it, vi } from 'vitest';

// project imports
import Results from './Results';

// the results fetch needs the auth context and the API, neither of which is under test here
vi.mock('@utilities/auth', () => ({ useAuth: () => ({ checkCookie: vi.fn() }) }));
vi.mock('@thorpi/results', () => ({ getResults: vi.fn(), getRepoResults: vi.fn() }));

/** Render the results panel with the given results and no fetch (effects do not run in a static render). */
const render = (results: Record<string, unknown[]>, repo = false): string =>
  renderToStaticMarkup(
    <Results
      repo={repo}
      sha256="abc"
      results={results as never}
      setResults={vi.fn()}
      numResults={Object.keys(results).length}
      setNumResults={vi.fn()}
    />,
  );

describe('Results', () => {
  it('uses a single full-width column when there are no results', () => {
    const markup = render({});
    expect(markup).toContain('results-container-single');
    expect(markup).toContain('No Tool Results Available');
    expect(markup).not.toContain('results-toc-col');
  });
  it('uses plain banner text for the empty state of a repo and a heading for a file', () => {
    expect(render({}, true)).toContain('No tool results available.');
    expect(render({}, true)).not.toContain('<h5');
    expect(render({}, false)).toContain('<h3>No Tool Results Available</h3>');
  });
});
