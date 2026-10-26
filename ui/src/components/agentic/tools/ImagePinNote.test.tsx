import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it } from 'vitest';

// project imports
import { ImagePinNote } from './ImagePinNote';

describe('ImagePinNote', () => {
  it('explains that a pinned Image tool version blocks deletion', () => {
    const markup = renderToStaticMarkup(<ImagePinNote />);
    expect(markup).toContain('data-testid="image-pin-note"');
    expect(markup).toContain('pin a version of an Image tool');
    expect(markup).toContain('can&#x27;t be deleted');
    expect(markup).toContain('versions before it are already gone');
  });

  it('explains that deprecating a pinned version keeps existing pins working', () => {
    const markup = renderToStaticMarkup(<ImagePinNote />);
    expect(markup).toContain('Deprecating a version leaves the tools that already pin it');
    expect(markup).toContain('no new pin can name it');
  });
});
