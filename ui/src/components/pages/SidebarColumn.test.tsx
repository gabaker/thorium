import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it } from 'vitest';

// project imports
import { SecondaryToggle } from './SidebarColumn';

/** Render the toggle in the given state. */
const render = (expanded: boolean): string => renderToStaticMarkup(<SecondaryToggle expanded={expanded} onToggle={() => {}} />);

describe('SecondaryToggle', () => {
  it('names the collapsed toggle and marks it not expanded', () => {
    const html = render(false);
    expect(html).toContain('aria-label="Show more"');
    expect(html).toContain('aria-expanded="false"');
  });
  it('names the expanded toggle and marks it expanded', () => {
    const html = render(true);
    expect(html).toContain('aria-label="Show less"');
    expect(html).toContain('aria-expanded="true"');
  });
});
