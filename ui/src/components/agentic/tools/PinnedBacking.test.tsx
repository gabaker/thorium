import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it } from 'vitest';

// project imports
import { PinnedBacking } from './PinnedBacking';

describe('PinnedBacking', () => {
  it('shows the instance id until the pinned version has been read', () => {
    const markup = renderToStaticMarkup(<PinnedBacking id="47f99e34-a163-41cb-9169-94680f68939e" />);
    expect(markup).toContain('data-testid="pinned-backing"');
    expect(markup).toContain('47f99e34-a163-41cb-9169-94680f68939e');
  });
});
