import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { MemoryRouter } from 'react-router-dom';
import { describe, expect, it, vi } from 'vitest';

// project imports
import ToolSetBrowsing from './ToolSetBrowsing';
import { AgenticKinds } from '@models/agentic';

/** A toolset name longer than the short-name limit. */
const LONG_NAME = 'a-really-long-toolset-name-to-force-the-scroll';

/** An instance name longer than the short-name limit whose last name segment is short. */
const LONG_INSTANCE_NAME = 'uiloop-r3w-set-europa-and-more-l6y6#r1';

// the page shell needs the auth provider, and the lists need the API; neither is under test here
vi.mock('@components/pages/Page', () => ({
  default: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
}));
vi.mock('@components/agentic/hooks', () => ({
  useAgenticList: (kind: AgenticKinds) => ({
    items:
      kind === AgenticKinds.ToolSet
        ? [
            {
              id: 'ssssssss-0000',
              name: 'long-toolset-name',
              groups: ['g'],
              metadata: {
                ToolSet: {
                  read_only: true,
                  selector: null,
                  tools: [{}, {}, {}],
                  auto_instance: false,
                  latest_instance: 'iiiiiiii-0000',
                  highest_revision: 2,
                },
              },
            },
            {
              id: 'ssssssss-0001',
              name: LONG_NAME,
              groups: ['g'],
              metadata: {
                ToolSet: { read_only: false, selector: null, auto_instance: false, latest_instance: 'iiiiiiii-0001', highest_revision: 1 },
              },
            },
          ]
        : kind === AgenticKinds.ToolSetInstance
          ? [
              {
                id: 'iiiiiiii-0000',
                name: 'long-toolset-name#r2',
                metadata: { ToolSetInstance: { tool_set: 'ssssssss-0000', revision: 2, status: 'Ready', members: [{}, {}] } },
              },
              {
                id: 'iiiiiiii-0001',
                name: LONG_INSTANCE_NAME,
                metadata: { ToolSetInstance: { tool_set: 'ssssssss-0001', revision: 1, status: 'Ready', members: [{}] } },
              },
            ]
          : [
              // one agent that follows the toolset and one that pins an instance of it
              { id: 'a1', name: 'a1', metadata: { Agent: { tool_sets: [{ id: 'ssssssss-0000', kind: 'ToolSet', metadata: {} }] } } },
              {
                id: 'a2',
                name: 'a2',
                metadata: {
                  Agent: {
                    tool_sets: [
                      { id: 'iiiiiiii-0000', kind: 'ToolSetInstance', metadata: { ToolSetInstance: { tool_set: 'ssssssss-0000' } } },
                    ],
                  },
                },
              },
              { id: 'a3', name: 'a3', metadata: { Agent: { tool_sets: [] } } },
            ],
    loading: false,
    error: '',
    reload: () => undefined,
  }),
}));

/** Render the toolset list. */
const render = (): string =>
  renderToStaticMarkup(
    <MemoryRouter>
      <ToolSetBrowsing />
    </MemoryRouter>,
  );

describe('ToolSetBrowsing', () => {
  it('keeps a short latest instance name whole, revision suffix included, before its status and member count', () => {
    const markup = render();
    expect(markup).toMatch(
      /<span class="[^"]*">long-toolset-name#r2<\/span><div><div class="[^"]*"><span class="[^"]*">Ready<\/span><span class="[^"]*">2 members<\/span><\/div><\/div>/,
    );
  });

  it('lets a long toolset name wrap instead of keeping it on one line like a short name', () => {
    const markup = render();
    // the short and the long name are rendered by the same component but with different styles
    const shortClass = /<span class="([^"]*)">long-toolset-name#r2<\/span><div>/.exec(markup)?.[1];
    const longClass = new RegExp(`<span class="([^"]*)">${LONG_NAME}</span>`).exec(markup)?.[1];
    expect(shortClass).toBeDefined();
    expect(longClass).toBeDefined();
    expect(longClass).not.toBe(shortClass);
  });

  it('keeps the last segment of a long instance name with its revision suffix so the suffix never sits alone on a line', () => {
    const markup = render();
    expect(markup).toMatch(/<span class="[^"]*">uiloop-r3w-set-europa-and-more-<\/span><span class="[^"]*">l6y6#r1<\/span>/);
  });

  it('counts the agents that follow the toolset or pin one of its instances under the name instead of in a column of its own', () => {
    const markup = render();
    expect(markup).toMatch(/<div data-testid="toolset-used-by-long-toolset-name"><span class="[^"]*">used by 2 agents<\/span><\/div>/);
    expect(markup).not.toContain('<th>Used by</th>');
    // the name cell holds the name link and the count, and the table keeps its four other columns
    expect(markup.match(/<th>/g)).toHaveLength(4);
  });

  it('shows the explicit tool count with the settings badges', () => {
    expect(render()).toContain('3 explicit tools');
  });

  it('lists the settings badges in a wrapping list container', () => {
    const markup = render();
    expect(markup).toMatch(
      /<div class="[^"]*"><span class="[^"]*">read only<\/span><span class="[^"]*">3 explicit tools<\/span><span class="[^"]*">auto off<\/span><\/div>/,
    );
  });
});
