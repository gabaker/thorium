import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { ServerStyleSheet } from 'styled-components';
import { describe, expect, it } from 'vitest';

// project imports
import { ConfirmButton } from './ConfirmButton';
import { ButtonSize } from '@components/shared/buttons';

/**
 * Render a confirm button with its styles.
 *
 * @param size - The size to ask for, or undefined for the default.
 * @returns The CSS the button collected, without whitespace.
 */
const cssOf = (size?: ButtonSize): string => {
  const sheet = new ServerStyleSheet();
  renderToStaticMarkup(
    sheet.collectStyles(
      <ConfirmButton testId="delete" size={size} onConfirm={() => undefined}>
        Delete
      </ConfirmButton>,
    ),
  );
  const css = sheet.getStyleTags().replace(/\s+/g, '');
  sheet.seal();
  return css;
};

describe('ConfirmButton', () => {
  it('is as large as the other header buttons by default', () => {
    // a Medium button is 0.85rem, a Small one 0.78rem
    expect(cssOf()).toContain('font-size:0.85rem;');
  });

  it('can be made small for a dense table row', () => {
    expect(cssOf(ButtonSize.Small)).toContain('font-size:0.78rem;');
  });
});

describe('ConfirmButton labels', () => {
  it('shows only the first button until it is armed', () => {
    const markup = renderToStaticMarkup(
      <ConfirmButton
        testId="cancel"
        question="Fail this run?"
        confirmText="Yes, fail it"
        cancelText="Keep running"
        onConfirm={() => undefined}
      >
        Cancel run
      </ConfirmButton>,
    );
    expect(markup).toContain('data-testid="cancel"');
    expect(markup).toContain('Cancel run');
    expect(markup).not.toContain('Fail this run?');
    expect(markup).not.toContain('cancel-confirm');
  });
});
