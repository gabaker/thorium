import { renderToStaticMarkup } from 'react-dom/server';
import { MemoryRouter } from 'react-router-dom';
import { describe, expect, it } from 'vitest';

// project imports
import { FileActionsToolbar } from './FileInfo';

describe('FileActionsToolbar', () => {
  it('links Run Analysis to the run page with the file filled in, as a real link', () => {
    const sha = 'a'.repeat(64);
    const markup = renderToStaticMarkup(
      <MemoryRouter>
        <FileActionsToolbar sha256={sha} onNavigateTab={() => undefined} />
      </MemoryRouter>,
    );
    const link = /<a [^>]*data-testid="file-run-analysis"[^>]*>/.exec(markup)?.[0] ?? '';
    expect(link).toContain(`href="/launch?sample=${sha}"`);
  });
});
