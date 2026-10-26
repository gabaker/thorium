import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { MemoryRouter } from 'react-router-dom';
import { describe, expect, it, vi } from 'vitest';

// project imports
import WorkflowBrowsing from './WorkflowBrowsing';
import { AgenticKinds } from '@models/agentic';

// the page shell needs the auth provider, and the lists need the API; neither is under test here
vi.mock('@components/pages/Page', () => ({
  default: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
}));
vi.mock('@components/agentic/hooks', () => ({
  useAgenticList: (kind: AgenticKinds) => ({
    items:
      kind === AgenticKinds.WorkflowCatalog
        ? [
            {
              id: 'wwwwwwww-0000',
              name: 'long-workflow-name',
              groups: ['g'],
              metadata: {
                WorkflowCatalog: { agents: [{}], entry_agent: 'aaaaaaaa-0000', latest_instance: 'iiiiiiii-0000', highest_revision: 2 },
              },
            },
          ]
        : kind === AgenticKinds.Agent
          ? [{ id: 'aaaaaaaa-0000', name: 'long-entry-agent', groups: ['g'], metadata: { Agent: {} } }]
          : [{ id: 'iiiiiiii-0000', name: 'long-workflow-name#r2', metadata: { WorkflowInstance: { status: 'Ready' } } }],
    loading: false,
    error: '',
    reload: () => undefined,
  }),
}));

/** Render the workflow list. */
const render = (): string =>
  renderToStaticMarkup(
    <MemoryRouter>
      <WorkflowBrowsing />
    </MemoryRouter>,
  );

describe('WorkflowBrowsing', () => {
  it('lets the workflow name break between its chunks inside its link', () => {
    // each chunk of the name is a whole run and a <wbr> offers the break between them, so a narrow card wraps the name instead of scrolling
    const link = /<a [^>]*>(.*?)<\/a>/.exec(render())?.[1] ?? '';
    expect(link).toContain('<wbr/>');
    expect(link.replace(/<[^>]*>/g, '')).toBe('long-workflow-name');
  });

  it('lets the entry agent and latest instance names break between their chunks', () => {
    const markup = render();
    // the revision suffix stays glued to the last chunk, so no line holds only the suffix
    expect(markup).toMatch(/<td>(<span class="[^"]*">[^<]*<\/span><wbr\/>)+<span class="[^"]*">[^<]*<\/span><\/td>/);
    expect(markup.replace(/<[^>]*>/g, '')).toContain('long-entry-agent');
    expect(markup.replace(/<[^>]*>/g, '')).toContain('long-workflow-name#r2');
  });

  it('puts the status badge under the latest instance name', () => {
    expect(render()).toMatch(/#r2<\/span><div><div class="[^"]*"><span class="[^"]*">Ready<\/span><\/div><\/div>/);
  });

  it('puts the table in a scrolling box', () => {
    expect(render()).toMatch(/<div class="[^"]*"><table [^>]*data-testid="workflow-table"/);
  });
});
