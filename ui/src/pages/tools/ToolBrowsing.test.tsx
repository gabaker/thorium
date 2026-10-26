import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { MemoryRouter } from 'react-router-dom';
import { ServerStyleSheet } from 'styled-components';
import { beforeEach, describe, expect, it, vi } from 'vitest';

// project imports
import ToolBrowsing from './ToolBrowsing';
import { AgenticKinds } from '@models/agentic';

/** A tool with a long hyphenated name and a deprecated version, the kind of row that crowds the list at 1024. */
const TOOL = {
  id: 'tttttttt-0000',
  name: 'uiloop-v2a2-gan1024-v1',
  description: 'A tool',
  groups: ['g'],
  metadata: {
    Tool: {
      kind: 'Image',
      latest: 'inst-2',
      latest_version: { SemVer: '2.0.0' },
      modified: '2026-10-28T15:31:15.123Z',
      instances: [
        { id: 'inst-2', version: { SemVer: '2.0.0' }, deprecated: false },
        { id: 'inst-1', version: { SemVer: '1.0.0' }, deprecated: true },
      ],
    },
  },
};

// the page shell needs the auth provider, and the lists need the API; neither is under test here
vi.mock('@components/pages/Page', () => ({
  default: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
}));
// the list the mocked hook returns; a test sets it to simulate a failed or empty listing
const listState = { tools: [TOOL] as unknown[], error: '' };
vi.mock('@components/agentic/hooks', () => ({
  useAgenticList: (kind: AgenticKinds) => ({
    items: kind === AgenticKinds.Tool ? listState.tools : [],
    loading: false,
    error: listState.error,
    reload: () => undefined,
  }),
  useGroupNames: () => ['g'],
  useCreateBlockReason: () => '',
}));

/** Render the tool list with the mocked tool. */
const render = (): string =>
  renderToStaticMarkup(
    <MemoryRouter>
      <ToolBrowsing />
    </MemoryRouter>,
  );

describe('ToolBrowsing', () => {
  beforeEach(() => {
    listState.tools = [TOOL];
    listState.error = '';
  });

  it('shows the load error without a zero count or an empty-state message when the list failed', () => {
    listState.tools = [];
    listState.error = 'List Tool: injected failure';
    const markup = render();
    expect(markup).toContain('List Tool: injected failure');
    expect(markup).not.toContain('tool-count');
    expect(markup).not.toContain('No tools found.');
  });

  it('shows the empty state and a zero count when the list loaded with no tools', () => {
    listState.tools = [];
    const markup = render();
    expect(markup).toContain('No tools found.');
    expect(markup).toContain('0 tools');
  });

  it('keeps the tool name whole in its link so a narrow window never splits it at a hyphen', () => {
    // the name is the only content of a no-wrap span inside the link
    expect(render()).toMatch(/<a [^>]*href="\/tools\/tttttttt-0000"[^>]*><span class="[^"]*">uiloop-v2a2-gan1024-v1<\/span><\/a>/);
  });

  it('gives the name column no fixed minimum width that would cap it below a long name', () => {
    expect(render()).toContain('<th>Name</th>');
  });

  it('lets the modified time wrap between its date and time but never inside the date', () => {
    // the date and the time are each an unbreakable span with a breakable space between them
    expect(render()).toMatch(/<td class="[^"]*"><span class="[^"]*">2026-10-28<\/span> <span class="[^"]*">15:31:15<\/span><\/td>/);
  });

  it('keeps the modified cell on one line on a wide window only', () => {
    // render with a style sheet so the CSS generated for the modified cell can be read back
    const sheet = new ServerStyleSheet();
    renderToStaticMarkup(
      sheet.collectStyles(
        <MemoryRouter>
          <ToolBrowsing />
        </MemoryRouter>,
      ),
    );
    const css = sheet.getStyleTags();
    sheet.seal();
    // the no-wrap rule sits inside a min-width media query at the xl breakpoint, not on the cell unconditionally
    expect(css).toMatch(/@media \(min-width: 1200px\)\{[^}]*white-space:nowrap/);
  });
});
