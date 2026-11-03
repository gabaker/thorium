import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it, vi } from 'vitest';

// project imports
import Modal, { handleModalKey, keepFocusInside, ModalFrame, wrapFocusIndex } from './Modal';

describe('wrapFocusIndex', () => {
  it('moves from the last element to the first on Tab', () => {
    expect(wrapFocusIndex(2, 3, false)).toBe(0);
  });
  it('moves from the first element to the last on Shift+Tab', () => {
    expect(wrapFocusIndex(0, 3, true)).toBe(2);
  });
  it('leaves focus to the browser between the ends', () => {
    expect(wrapFocusIndex(1, 3, false)).toBeNull();
    expect(wrapFocusIndex(1, 3, true)).toBeNull();
  });
  it('brings focus in from the dialog itself', () => {
    expect(wrapFocusIndex(-1, 3, false)).toBe(0);
    expect(wrapFocusIndex(-1, 3, true)).toBe(2);
  });
  it('does nothing in a dialog without focusable elements', () => {
    expect(wrapFocusIndex(-1, 0, false)).toBeNull();
  });
});

describe('ModalFrame', () => {
  it('names the dialog by its title for screen readers', () => {
    const html = renderToStaticMarkup(
      <ModalFrame title="Delete it?" onClose={() => undefined}>
        body text
      </ModalFrame>,
    );
    expect(html).toContain('role="dialog"');
    expect(html).toContain('aria-modal="true"');
    const labelledBy = /aria-labelledby="([^"]+)"/.exec(html)?.[1];
    expect(labelledBy).toBeTruthy();
    expect(html).toMatch(new RegExp(`<h2 id="${labelledBy}"[^>]*>Delete it\\?</h2>`));
    expect(html).toContain('aria-label="Close"');
    expect(html).toContain('body text');
  });
  it('draws a footer only when there are actions', () => {
    const bare = renderToStaticMarkup(
      <ModalFrame title="t" onClose={() => undefined}>
        b
      </ModalFrame>,
    );
    const withActions = renderToStaticMarkup(
      <ModalFrame title="t" onClose={() => undefined} actions={<button>Confirm</button>}>
        b
      </ModalFrame>,
    );
    expect(bare).not.toContain('Confirm');
    expect(withActions).toContain('<button>Confirm</button>');
  });
});

describe('Modal', () => {
  it('renders nothing while closed', () => {
    expect(
      renderToStaticMarkup(
        <Modal show={false} title="t" onClose={() => undefined}>
          b
        </Modal>,
      ),
    ).toBe('');
  });
});

/**
 * Build a stand-in dialog holding the given focusable elements, recording which element took focus.
 *
 * @param count - How many focusable elements the dialog holds.
 * @returns The dialog, its elements, and the log of focus calls ('dialog' or the element's index).
 */
function fakeDialog(count: number) {
  const focused: (number | 'dialog')[] = [];
  const elements = Array.from({ length: count }, (_, index) => ({ focus: () => focused.push(index) }) as unknown as HTMLElement);
  const dialog = {
    contains: (node: unknown) => node === dialog || elements.includes(node as HTMLElement),
    querySelectorAll: () => elements,
    focus: () => focused.push('dialog'),
  } as unknown as HTMLElement;
  return { dialog, elements, focused };
}

/**
 * Build a key event that records whether its default action was prevented.
 *
 * @param key - The key pressed.
 * @param shiftKey - Whether Shift was held.
 * @returns The event and a getter for whether `preventDefault` was called.
 */
function fakeKey(key: string, shiftKey = false) {
  let prevented = false;
  const event = { key, shiftKey, preventDefault: () => (prevented = true) };
  return { event, prevented: () => prevented };
}

describe('handleModalKey', () => {
  it('closes the dialog on Escape wherever focus is, including the page body', () => {
    const { dialog } = fakeDialog(2);
    const onClose = vi.fn();
    handleModalKey(fakeKey('Escape').event, dialog, { tagName: 'BODY' } as unknown as Element, onClose);
    expect(onClose).toHaveBeenCalledTimes(1);
  });
  it('brings Tab back into the dialog when focus has fallen to the page body', () => {
    const { dialog, focused } = fakeDialog(3);
    const forward = fakeKey('Tab');
    handleModalKey(forward.event, dialog, { tagName: 'BODY' } as unknown as Element, vi.fn());
    expect(forward.prevented()).toBe(true);
    const backward = fakeKey('Tab', true);
    handleModalKey(backward.event, dialog, { tagName: 'BODY' } as unknown as Element, vi.fn());
    expect(backward.prevented()).toBe(true);
    expect(focused).toEqual([0, 2]);
  });
  it('wraps Tab from the last element to the first', () => {
    const { dialog, elements, focused } = fakeDialog(3);
    const tab = fakeKey('Tab');
    handleModalKey(tab.event, dialog, elements[2], vi.fn());
    expect(tab.prevented()).toBe(true);
    expect(focused).toEqual([0]);
  });
  it('leaves Tab between the ends and other keys to the browser', () => {
    const { dialog, elements, focused } = fakeDialog(3);
    const onClose = vi.fn();
    const tab = fakeKey('Tab');
    handleModalKey(tab.event, dialog, elements[0], onClose);
    const enter = fakeKey('Enter');
    handleModalKey(enter.event, dialog, elements[0], onClose);
    expect(tab.prevented()).toBe(false);
    expect(enter.prevented()).toBe(false);
    expect(focused).toEqual([]);
    expect(onClose).not.toHaveBeenCalled();
  });
});

describe('keepFocusInside', () => {
  it('pulls focus back to the dialog when something outside it takes focus', () => {
    const { dialog, focused } = fakeDialog(2);
    keepFocusInside({} as EventTarget, dialog);
    expect(focused).toEqual(['dialog']);
  });
  it('leaves focus alone when it moves within the dialog', () => {
    const { dialog, elements, focused } = fakeDialog(2);
    keepFocusInside(elements[1], dialog);
    keepFocusInside(dialog, dialog);
    expect(focused).toEqual([]);
  });
});
