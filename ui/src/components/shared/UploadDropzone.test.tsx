import { renderToStaticMarkup } from 'react-dom/server';
import { ServerStyleSheet } from 'styled-components';
import { describe, expect, it } from 'vitest';

// project imports
import UploadDropzone, { DropzoneSize, formatFileSize } from './UploadDropzone';

/** Render the dropzone with the given files and size, returning its markup and CSS. */
function render(files: File[], size?: DropzoneSize) {
  const sheet = new ServerStyleSheet();
  const markup = renderToStaticMarkup(
    sheet.collectStyles(<UploadDropzone onChange={() => undefined} selectedFiles={files} size={size} inputTestId="pick" />),
  );
  const css = sheet.getStyleTags();
  sheet.seal();
  return { markup, css };
}

describe('UploadDropzone', () => {
  it('keeps the upload page look by default: the full-width area and headed file list', () => {
    const { markup, css } = render([new File(['abc'], 'a.bin')]);
    expect(markup).toContain('Drag and drop some files or click to select files');
    expect(markup).toContain('Accepted Files');
    expect(markup).not.toContain('dropzone-compact');
    expect(css).toContain('padding:20px');
    expect(css).toContain('border-width:2px');
  });
  it('draws the compact area like a form input, with a removable file list', () => {
    const { markup, css } = render([new File(['abc'], 'a.bin'), new File(['x'.repeat(2048)], 'b.bin')], DropzoneSize.Compact);
    expect(markup).toContain('data-testid="dropzone-compact"');
    expect(markup).toContain('Drop files here or click to browse');
    expect(markup).not.toContain('Accepted Files');
    expect(markup.match(/data-testid="dropzone-file"/g)).toHaveLength(2);
    expect(markup).toContain('aria-label="Remove a.bin"');
    expect(markup).toContain('2 KB');
    expect(css).toContain('border-radius:6px');
    expect(css).toContain('var(--thorium-secondary-panel-bg)');
  });
  it('lists nothing in the compact area before a file is chosen', () => {
    expect(render([], DropzoneSize.Compact).markup).not.toContain('dropzone-files');
  });
  it('puts the test id on the file input, where tests choose files', () => {
    expect(render([], DropzoneSize.Compact).markup).toMatch(/<input[^>]*data-testid="pick"/);
  });
});

describe('formatFileSize', () => {
  it('formats bytes in the largest whole unit', () => {
    expect(formatFileSize(3)).toBe('3 B');
    expect(formatFileSize(1536)).toBe('1.5 KB');
    expect(formatFileSize(10 * 1024 * 1024)).toBe('10 MB');
  });
});
