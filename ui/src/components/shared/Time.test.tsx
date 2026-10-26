import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it } from 'vitest';
import Time from './Time';

/** Renders a timestamp through the shared Time component to plain text. */
const render = (value: string, verbose?: boolean): string =>
  renderToStaticMarkup(<Time verbose={verbose}>{value}</Time>).replace(/<[^>]+>/g, '');

describe('Time', () => {
  it('drops the trailing Z from timestamps without fractional seconds', () => {
    expect(render('2026-10-28T12:00:03Z')).toBe('2026-10-28 12:00:03');
  });
  it('drops fractional seconds and the trailing Z', () => {
    expect(render('2026-10-28T12:00:03.123456Z')).toBe('2026-10-28 12:00:03');
  });
  it('renders zoneless timestamps in the same format', () => {
    expect(render('2026-10-28T11:55:11')).toBe('2026-10-28 11:55:11');
  });
  it('renders the verbose form without the Z', () => {
    expect(render('2026-10-28T12:00:03Z', true)).toBe('on 2026-10-28 at 12:00:03');
  });
  it('returns values without a time part unchanged', () => {
    expect(render('unknown')).toBe('unknown');
  });
});
