import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { MemoryRouter } from 'react-router-dom';
import { ServerStyleSheet } from 'styled-components';
import { describe, expect, it } from 'vitest';

// project imports
import { UsedBy } from './UsedBy';
import { Badge, InlineList } from './shared';

const USERS = [
  { id: 'a', name: 'uiloop-r3w-wf-test-l6y6' },
  { id: 'b', name: 'uiloop-a3v-wf-v1b' },
];

describe('UsedBy', () => {
  it('keeps each name whole in its own no-wrap item so a line breaks between links, not inside one', () => {
    const markup = renderToStaticMarkup(
      <MemoryRouter>
        <UsedBy users={USERS} linkTo={(id) => `/workflows/${id}`} loading={false} error="" />
      </MemoryRouter>,
    );
    // every link sits alone in a span, and the spans are the only children of one list container
    const items = [...markup.matchAll(/<span class="[^"]*"><a [^>]*href="\/workflows\/(\w)"[^>]*>([^<]*)<\/a><\/span>/g)];
    expect(items.map((item) => item[2])).toEqual(USERS.map((user) => user.name));
    expect(markup.startsWith('<div ')).toBe(true);
  });

  it('says none, loading, or unavailable instead of listing', () => {
    const render = (loading: boolean, error: string, users = USERS) =>
      renderToStaticMarkup(
        <MemoryRouter>
          <UsedBy users={users} linkTo={(id) => id} loading={loading} error={error} />
        </MemoryRouter>,
      );
    expect(render(false, '', [])).toContain('none');
    expect(render(true, '')).toContain('loading');
    expect(render(false, 'boom')).toContain('unavailable');
  });
});

describe('InlineList', () => {
  it('wraps its items with a gap, and the badges add no margin that would indent a wrapped badge', () => {
    const sheet = new ServerStyleSheet();
    renderToStaticMarkup(
      sheet.collectStyles(
        <InlineList>
          <Badge>one</Badge>
          <Badge>two</Badge>
        </InlineList>,
      ),
    );
    const css = sheet.getStyleTags().replace(/\s+/g, '');
    sheet.seal();
    expect(css).toContain('flex-wrap:wrap');
    expect(css).toContain('gap:');
    // a badge has no outer margin of its own, so the gap alone spaces the items
    expect(css).toContain('margin:0;');
    expect(css).not.toContain(':not(:first-child){margin-left');
  });
});
