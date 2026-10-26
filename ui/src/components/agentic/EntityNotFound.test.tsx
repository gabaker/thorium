import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { MemoryRouter } from 'react-router-dom';
import { describe, expect, it, vi } from 'vitest';

// project imports
import { EntityNotFound } from './EntityNotFound';

// the page shell needs the auth provider, which is not under test here
vi.mock('@components/pages/Page', () => ({
  default: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
}));

/** Render the not-found body for a file, with an optional notice and loading state. */
const render = (notice?: React.ReactNode, loading = false): string =>
  renderToStaticMarkup(
    <MemoryRouter>
      <EntityNotFound what="File" loading={loading} listPath="/files" listName="files" error="sample abc not found" notice={notice} />
    </MemoryRouter>,
  );

describe('EntityNotFound', () => {
  it('shows the title, the way back, and the error', () => {
    const markup = render();
    expect(markup).toContain('File not found');
    expect(markup).toContain('href="/files"');
    expect(markup).toContain('sample abc not found');
  });
  it('shows the notice above the title', () => {
    const markup = render(<p>Submission deleted successfully!</p>);
    expect(markup.indexOf('Submission deleted successfully!')).toBeGreaterThan(-1);
    expect(markup.indexOf('Submission deleted successfully!')).toBeLessThan(markup.indexOf('File not found'));
  });
  it('shows no notice while loading', () => {
    expect(render(<p>Submission deleted successfully!</p>, true)).not.toContain('Submission deleted successfully!');
  });
  it('says the entity could not be loaded, with a Retry, when the read failed', () => {
    const markup = renderToStaticMarkup(
      <MemoryRouter>
        <EntityNotFound
          what="Tool"
          loading={false}
          listPath="/tools"
          listName="tools"
          error="Get Entity: boom"
          failed
          onRetry={() => undefined}
        />
      </MemoryRouter>,
    );
    expect(markup).toContain('Tool could not be loaded');
    expect(markup).toContain('entity-retry');
    expect(markup).toContain('Get Entity: boom');
    expect(markup).not.toContain('not found');
    expect(markup).not.toContain('does not exist');
  });
  it('offers no Retry for an entity that is missing or hidden', () => {
    expect(render()).not.toContain('entity-retry');
  });
});
