import { renderToStaticMarkup } from 'react-dom/server';
import { ServerStyleSheet } from 'styled-components';
import { describe, expect, it, vi } from 'vitest';

// project imports
import { NameList } from './WorkflowInstanceDetails';
import { SHORT_NAME_LIMIT } from '@utilities/agentic/display';

// the page shell needs the auth provider, which the name lists do not use
vi.mock('@components/pages/Page', () => ({
  default: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
}));

/**
 * Render a name list and report, for each name, whether its span is styled to stay whole on one line.
 *
 * @param names - The names to list.
 * @param details - Optional per-name details shown after each name.
 * @returns The text of each rendered span with whether its style forbids wrapping.
 */
const wholeFlags = (names: string[], details?: string[]): [string, boolean][] => {
  const sheet = new ServerStyleSheet();
  const markup = renderToStaticMarkup(sheet.collectStyles(<NameList names={names} details={details} />));
  const css = sheet.getStyleTags();
  sheet.seal();
  return [...markup.matchAll(/<span class="([^"]*)">([^<]*)<\/span>/g)].map(([, classes, text]) => {
    const rule = classes
      .split(' ')
      .map((cls) => css.match(new RegExp(`\\.${cls}\\{([^}]*)\\}`))?.[1] ?? '')
      .join('');
    return [text, rule.includes('white-space:nowrap')];
  });
};

describe('NameList', () => {
  it('keeps a name at the short limit whole when a comma follows it', () => {
    const name = 'x'.repeat(SHORT_NAME_LIMIT);
    expect(wholeFlags([name, 'other'])).toEqual([
      [`${name},`, true],
      ['other', true],
    ]);
  });

  it('treats a limit-length name the same with a colon as with a comma or nothing', () => {
    const name = 'x'.repeat(SHORT_NAME_LIMIT);
    expect(wholeFlags([name], ['env'])).toEqual([
      [`${name}:`, true],
      ['env', true],
    ]);
  });

  it('still lets a name over the limit break', () => {
    const name = 'x'.repeat(SHORT_NAME_LIMIT + 1);
    expect(wholeFlags([name, 'other'])[0]).toEqual([`${name},`, false]);
  });
});
