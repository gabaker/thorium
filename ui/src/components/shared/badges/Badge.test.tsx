import { renderToStaticMarkup } from 'react-dom/server';
import { ServerStyleSheet } from 'styled-components';
import { describe, expect, it } from 'vitest';

// project imports
import { Badge, badgeColor, BadgeSize, BadgeTone, statusTone, WrapBadge } from './Badge';

/** Render some badges, returning their markup and CSS. */
function render(node: React.ReactNode) {
  const sheet = new ServerStyleSheet();
  const markup = renderToStaticMarkup(sheet.collectStyles(node));
  const css = sheet.getStyleTags();
  sheet.seal();
  return { markup, css };
}

describe('Badge', () => {
  const { markup, css } = render(
    <>
      <Badge tone={BadgeTone.Ok} data-testid="b">
        Ready
      </Badge>
      <Badge size={BadgeSize.Small} count>
        3
      </Badge>
    </>,
  );
  it('has nearly square sides from the shared radius token, never a full pill', () => {
    expect(css).toContain('border-radius:var(--thorium-badge-radius, 4px)');
    expect(css).toContain('border-radius:var(--thorium-badge-radius-sm, 3px)');
    expect(css).not.toContain('999px');
  });
  it('centres on the line of a table cell and keeps no outer margin', () => {
    expect(css).toContain('display:inline-flex');
    expect(css).toContain('align-items:center');
    expect(css).toContain('vertical-align:middle');
    expect(css).toContain('margin:0');
  });
  it('draws a tone from theme variables and keeps counts at equal width', () => {
    expect(css).toContain('var(--thorium-ok-bg)');
    expect(css).toContain('font-variant-numeric:tabular-nums');
    expect(css).not.toMatch(/#[0-9a-f]{3,6}\b/i);
  });
  it('passes other attributes through and keeps $tone as an alias', () => {
    expect(markup).toContain('data-testid="b"');
    expect(render(<Badge $tone={BadgeTone.Error}>x</Badge>).css).toContain('var(--thorium-danger-bg)');
  });
  it('draws a given colour in place of the tone, for a badge coloured per value', () => {
    const custom = render(
      <Badge tone={BadgeTone.Ok} color="goldenrod">
        Analyst
      </Badge>,
    ).css;
    expect(custom).toContain('border:1px solid goldenrod');
    expect(custom).not.toContain('var(--thorium-ok-bg)');
  });
  it('wraps the text of a WrapBadge', () => {
    expect(render(<WrapBadge>long</WrapBadge>).css).toContain('overflow-wrap:anywhere');
  });
});

describe('statusTone', () => {
  it('gives every status one tone', () => {
    expect(['Completed', 'Ready'].map(statusTone)).toEqual([BadgeTone.Ok, BadgeTone.Ok]);
    expect(['Failed', 'Invalid', 'banned'].map(statusTone)).toEqual([BadgeTone.Error, BadgeTone.Error, BadgeTone.Error]);
    expect(['Started', 'Running'].map(statusTone)).toEqual([BadgeTone.Active, BadgeTone.Active]);
    expect(['Created', 'Sleeping', 'whatever'].map(statusTone)).toEqual([BadgeTone.Neutral, BadgeTone.Neutral, BadgeTone.Neutral]);
    expect(['no live version', 'Deprecated', 'no ready instance', 'Stopped'].map(statusTone)).toEqual([
      BadgeTone.Warn,
      BadgeTone.Warn,
      BadgeTone.Warn,
      BadgeTone.Warn,
    ]);
  });
});

describe('badgeColor', () => {
  it('gives the theme variable of a tone, with the link colour behind the active blue', () => {
    expect(badgeColor(BadgeTone.Ok)).toBe('var(--thorium-ok-bg)');
    expect(badgeColor(BadgeTone.Active)).toBe('var(--thorium-badge-active, var(--thorium-link-text))');
  });
});
