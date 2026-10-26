import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { ServerStyleSheet } from 'styled-components';
import { describe, expect, it } from 'vitest';

// project imports
import { CodeSpans, Mono, opensRow, PageHead, PageTitle, RowClick, Section, Table } from './shared';

/** Render text through CodeSpans and keep only the code tags, so styled-component classes don't matter. */
const render = (text: string): string => renderToStaticMarkup(<CodeSpans text={text} />).replace(/<code[^>]*>/g, '<code>');

describe('CodeSpans', () => {
  it('renders backticked names as inline code without the backticks', () => {
    expect(render('drop `additionalProperties` at #/additionalProperties; `oneOf` to `anyOf` at #/properties/choice/oneOf')).toBe(
      'drop <code>additionalProperties</code> at #/additionalProperties; <code>oneOf</code> to <code>anyOf</code> at #/properties/choice/oneOf',
    );
  });
  it('leaves text without backticks unchanged', () => {
    expect(render('no notes')).toBe('no notes');
  });
  it('keeps an unmatched backtick as text', () => {
    expect(render('a ` b')).toBe('a ` b');
  });
});

/** A plain primary click on a row's own cell. */
const PLAIN: RowClick = { button: 0, modified: false, onControl: false, selecting: false };

describe('opensRow', () => {
  it('opens the row on a plain primary click', () => {
    expect(opensRow(PLAIN)).toBe(true);
  });
  it("leaves clicks on the row's own links and controls to them", () => {
    expect(opensRow({ ...PLAIN, onControl: true })).toBe(false);
  });
  it('leaves modified, non-primary, and text-selecting clicks alone', () => {
    expect(opensRow({ ...PLAIN, modified: true })).toBe(false);
    expect(opensRow({ ...PLAIN, button: 2 })).toBe(false);
    expect(opensRow({ ...PLAIN, selecting: true })).toBe(false);
  });
});

describe('PageTitle', () => {
  it('wraps a long unbroken title instead of overflowing the page', () => {
    const sheet = new ServerStyleSheet();
    renderToStaticMarkup(sheet.collectStyles(<PageTitle>{'x'.repeat(90)}</PageTitle>));
    const css = sheet.getStyleTags();
    sheet.seal();
    expect(css).toContain('overflow-wrap:anywhere');
  });
});

/** Render a component with its styles and return the collected CSS with whitespace removed. */
const cssOf = (node: React.ReactElement): string => {
  const sheet = new ServerStyleSheet();
  renderToStaticMarkup(sheet.collectStyles(node));
  const css = sheet.getStyleTags().replace(/\s+/g, '');
  sheet.seal();
  return css;
};

describe('PageHead', () => {
  it('keeps the height of a Medium button so the title sits at one y with or without header actions', () => {
    expect(cssOf(<PageHead />)).toContain('min-height:2.125rem;');
  });
});

describe('Mono', () => {
  it('takes the colour of its surroundings instead of the browser code colour', () => {
    expect(cssOf(<Mono>id</Mono>)).toContain('color:inherit;');
  });
});

describe('Section', () => {
  it('never ends on a separator: the last row of a closing table and a trailing hr lose their rule', () => {
    const sheet = new ServerStyleSheet();
    renderToStaticMarkup(
      sheet.collectStyles(
        <Section>
          <Table>
            <tbody>
              <tr>
                <td>a</td>
              </tr>
            </tbody>
          </Table>
        </Section>,
      ),
    );
    const css = sheet.getStyleTags().replace(/\s+/g, ' ');
    sheet.seal();
    expect(css).toMatch(/>:last-child tbody>tr:last-child>td,[^{]*>:last-child tbody>tr:last-child>th\{border-bottom:none;\}/);
    expect(css).toMatch(/>hr:last-child\{display:none;\}/);
  });
});
