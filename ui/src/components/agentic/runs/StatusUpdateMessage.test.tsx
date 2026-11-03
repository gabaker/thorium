import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it } from 'vitest';

// project imports
import { StatusUpdateMessage } from './StatusUpdateMessage';
import { ENTRY_LIMIT, TEXT_LIMIT } from './updateMessage';

/** Render a message to markup. */
const render = (msg: string | null, update: Record<string, string> = {}): string =>
  renderToStaticMarkup(<StatusUpdateMessage update={{ msg, update }} />);

describe('StatusUpdateMessage', () => {
  it('labels the known fields and draws the status as a pill', () => {
    const markup = render(null, { status: '"Created"', stage: 'scan', job: 'j1' });
    expect(markup).toContain('data-testid="update-field-status"');
    expect(markup).toContain('data-status="Created"');
    expect(markup).toContain('>Stage<');
    expect(markup).not.toContain('&quot;Created&quot;');
  });
  it('shows plain text as it is', () => {
    expect(render('the worker was lost')).toContain('the worker was lost');
  });
  it('escapes markup in the payload instead of rendering it', () => {
    const markup = render('{"error":"<img src=x onerror=alert(1)>"}');
    expect(markup).not.toContain('<img');
    expect(markup).toContain('&lt;img');
  });
  it('indents a nested object and collapses it past the open depth', () => {
    const markup = render('{"counts":{"done":2,"deep":{"deeper":{"x":1}}}}');
    expect(markup).toContain('<dl');
    expect(markup).toContain('<details');
    expect(markup).toContain('1 keys');
  });
  it('cuts a huge payload with a Show more control', () => {
    const markup = render('x'.repeat(TEXT_LIMIT + 50));
    expect(markup).toContain('data-testid="update-show-more"');
    expect(markup).toContain('Show 50 more characters');
  });
  it('cuts a long list with a Show more control', () => {
    const items = JSON.stringify(Array.from({ length: ENTRY_LIMIT + 3 }, (_, index) => index));
    expect(render(items)).toContain('Show 3 more');
  });
  it('offers the raw JSON, hidden until asked for', () => {
    const markup = render('{"a":1}');
    expect(markup).toContain('data-testid="update-raw-toggle"');
    expect(markup).not.toContain('data-testid="update-raw"');
  });
});
