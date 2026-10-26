import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { renderToString } from 'react-dom/server';
import { ServerStyleSheet } from 'styled-components';
import { describe, expect, it } from 'vitest';

// project imports
import AlertBanner, { Severity } from './AlertBanner';

describe('AlertBanner', () => {
  it('wraps long unbroken text instead of widening the page', () => {
    const sheet = new ServerStyleSheet();
    const html = renderToString(sheet.collectStyles(<AlertBanner severity={Severity.Info}>{'x'.repeat(400)}</AlertBanner>));
    const css = sheet.getStyleTags();
    sheet.seal();
    expect(html).toContain('x'.repeat(400));
    expect(css).toContain('overflow-wrap:anywhere');
  });
});

const read = (path: string) => readFileSync(resolve(__dirname, path), 'utf8');

describe('AlertBanner text colors on the light themes', () => {
  const banner = read('./AlertBanner.tsx');
  const scss = read('../../../styles/colors.scss');

  it('reads a themed text variable for the error, warning and success tones', () => {
    for (const tone of ['error', 'warning', 'success']) {
      expect(banner).toContain(`var(--thorium-alert-${tone}-text,`);
    }
  });

  it('sets each themed text variable in both light themes', () => {
    for (const tone of ['error', 'warning', 'success']) {
      expect(scss.match(new RegExp(`--thorium-alert-${tone}-text:`, 'g'))).toHaveLength(2);
    }
  });
});
