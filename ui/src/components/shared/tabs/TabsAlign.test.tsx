import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { ServerStyleSheet } from 'styled-components';
import { describe, expect, it } from 'vitest';

// project imports
import Tabs from './Tabs';
import { TabsAlign } from './types';

/**
 * Render a two-tab bar with its styles.
 *
 * @param align - The alignment to ask for, or undefined for the default.
 * @returns The CSS the bar collected, without whitespace.
 */
const cssOf = (align?: TabsAlign): string => {
  const sheet = new ServerStyleSheet();
  renderToStaticMarkup(
    sheet.collectStyles(
      <Tabs
        tabs={[
          { key: 'a', label: 'A' },
          { key: 'b', label: 'B' },
        ]}
        active="a"
        onChange={() => undefined}
        align={align}
      />,
    ),
  );
  const css = sheet.getStyleTags().replace(/\s+/g, '');
  sheet.seal();
  return css;
};

describe('Tabs alignment', () => {
  it('starts the tabs at the left edge by default', () => {
    const css = cssOf();
    expect(css).toContain('justify-content:flex-start;');
    expect(css).not.toContain('justify-content:center;');
  });

  it('centres the tabs only when asked', () => {
    expect(cssOf(TabsAlign.Center)).toContain('justify-content:center;');
  });
});
