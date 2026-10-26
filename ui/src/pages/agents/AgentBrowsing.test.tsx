import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { MemoryRouter } from 'react-router-dom';
import { describe, expect, it, vi } from 'vitest';

// project imports
import AgentBrowsing from './AgentBrowsing';
import { AgenticKinds } from '@models/agentic';

// the page shell needs the auth provider, and the lists need the API; neither is under test here
vi.mock('@components/pages/Page', () => ({
  default: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
}));
vi.mock('@components/agentic/hooks', () => ({
  useAgenticList: (kind: AgenticKinds) => ({
    items:
      kind === AgenticKinds.Agent
        ? [
            {
              id: 'aaaaaaaa-0000',
              name: 'long-agent-name',
              groups: ['g'],
              metadata: {
                Agent: {
                  config: { models: [{ model_id: 'stub-model-a' }, { model_id: 'stub-model-b' }] },
                  harness: { name: 'jup-stub-harness' },
                },
              },
            },
            {
              id: 'bbbbbbbb-0000',
              name: 'pinned-agent',
              groups: ['g'],
              metadata: { Agent: { config: {}, harness: { name: 'jup-stub-harness@1.0.0' } } },
            },
            { id: 'cccccccc-0000', name: 'bare-agent', groups: ['g'], metadata: { Agent: { config: {} } } },
          ]
        : [],
    loading: false,
    error: '',
    reload: () => undefined,
  }),
}));

describe('AgentBrowsing', () => {
  it('shows the workflow count as a muted line under the name instead of a column', () => {
    const markup = renderToStaticMarkup(
      <MemoryRouter>
        <AgentBrowsing />
      </MemoryRouter>,
    );
    expect(markup).toMatch(/<div data-testid="agent-used-by-long-agent-name"><span class="[^"]*">used by 0 workflows<\/span><\/div>/);
    expect(markup).not.toContain('<th>Used by</th>');
  });

  it('keeps a short agent name whole inside its link, with no clipping', () => {
    const markup = renderToStaticMarkup(
      <MemoryRouter>
        <AgentBrowsing />
      </MemoryRouter>,
    );
    // the name is the only content of a span inside the link, and nothing in the page clips it with an ellipsis
    expect(markup).toMatch(/<a [^>]*href="\/agents\/aaaaaaaa-0000"[^>]*><span class="[^"]*">long-agent-name<\/span><\/a>/);
    expect(markup).not.toContain('title="long-agent-name"');
  });

  it('keeps the harness and each model id whole and joins a trailing separator to its id with a no-break space', () => {
    const markup = renderToStaticMarkup(
      <MemoryRouter>
        <AgentBrowsing />
      </MemoryRouter>,
    );
    expect(markup).toMatch(/<td><span class="[^"]*">jup-stub-harness<\/span><\/td>/);
    // the separator trails its id after a no-break space, so a plain space between the models is the only place the column breaks
    expect(markup).toMatch(
      /<td><span><span class="[^"]*">stub-model-a<\/span>\u00a0&gt;<\/span> <span><span class="[^"]*">stub-model-b<\/span><\/span><\/td>/,
    );
  });

  it('uses the shared table with no override of its own', () => {
    const markup = renderToStaticMarkup(
      <MemoryRouter>
        <AgentBrowsing />
      </MemoryRouter>,
    );
    // the scroll box directly wraps the table, which keeps the shared padding and text size
    expect(markup).toMatch(/<div class="[^"]*"><table [^>]*data-testid="agent-table"/);
  });

  it('keeps a pinned harness version whole and lets the column break only before it', () => {
    const markup = renderToStaticMarkup(
      <MemoryRouter>
        <AgentBrowsing />
      </MemoryRouter>,
    );
    expect(markup).toMatch(/<td><span class="[^"]*">jup-stub-harness<\/span><wbr\/><span class="[^"]*">@1\.0\.0<\/span><\/td>/);
  });

  it('names the workflow default for an agent with no models', () => {
    const markup = renderToStaticMarkup(
      <MemoryRouter>
        <AgentBrowsing />
      </MemoryRouter>,
    );
    expect(markup).toContain('<td>workflow default</td>');
  });

  it('says none in muted text for an agent with no harness', () => {
    const markup = renderToStaticMarkup(
      <MemoryRouter>
        <AgentBrowsing />
      </MemoryRouter>,
    );
    expect(markup).toMatch(/<td><span class="[^"]*">none<\/span><\/td>/);
  });
});
