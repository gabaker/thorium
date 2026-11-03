import React, { ReactNode, useEffect, useId, useRef } from 'react';
import { createPortal } from 'react-dom';
import { FaXmark } from 'react-icons/fa6';
import styled from 'styled-components';

// project imports
import { IconButton } from '@components/shared/buttons';
import { ButtonSize, ButtonVariant } from '@components/shared/buttons/types';
import { spacers } from '@styles';

/** How wide a modal dialog is drawn. */
export enum ModalSize {
  /** A short confirmation. */
  Small = 'sm',
  /** A form or a list (the default). */
  Medium = 'md',
  /** A wide form or table. */
  Large = 'lg',
}

/** The width of each modal size, capped by the viewport. */
const MODAL_WIDTH: Record<ModalSize, string> = {
  [ModalSize.Small]: '380px',
  [ModalSize.Medium]: '500px',
  [ModalSize.Large]: '800px',
};

/** The elements a keyboard user can tab to inside the dialog. */
const FOCUSABLE =
  'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

const Backdrop = styled.div`
  position: fixed;
  inset: 0;
  z-index: 1055;
  display: flex;
  align-items: flex-start;
  justify-content: center;
  overflow-y: auto;
  padding: ${spacers.six} ${spacers.four};
  background: rgba(0, 0, 0, 0.5);
`;

const Dialog = styled.div<{ $size: ModalSize }>`
  width: ${({ $size }) => MODAL_WIDTH[$size]};
  max-width: 100%;
  display: flex;
  flex-direction: column;
  background: var(--thorium-panel-bg);
  border: 1px solid var(--thorium-panel-border);
  border-radius: 8px;
  box-shadow: 0 8px 24px rgba(0, 0, 0, 0.5);
  color: var(--thorium-text);

  &:focus {
    outline: none;
  }
`;

const Header = styled.div`
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: ${spacers.three};
  padding: ${spacers.four};
  border-bottom: 1px solid var(--thorium-panel-border);
`;

const Title = styled.h2`
  margin: 0;
  font-size: 1.25rem;
  font-weight: 500;
  overflow-wrap: anywhere;
`;

const Body = styled.div`
  display: flex;
  flex-direction: column;
  gap: ${spacers.three};
  padding: ${spacers.four};
`;

const Footer = styled.div`
  display: flex;
  justify-content: flex-end;
  flex-wrap: wrap;
  gap: ${spacers.three};
  padding: ${spacers.three} ${spacers.four};
  border-top: 1px solid var(--thorium-panel-border);
`;

/**
 * Pick the element focus moves to when Tab leaves one end of the dialog, so focus cycles inside it.
 *
 * @param current - The index of the focused element among the focusable ones, or -1 when none of them has focus.
 * @param count - How many focusable elements the dialog holds.
 * @param backwards - Whether Shift+Tab was pressed.
 * @returns The index to focus, or `null` to let the browser move focus normally.
 */
export const wrapFocusIndex = (current: number, count: number, backwards: boolean): number | null => {
  if (count === 0) return null;
  if (backwards) return current <= 0 ? count - 1 : null;
  return current === -1 || current === count - 1 ? 0 : null;
};

/**
 * Act on a key pressed while a modal is open: Escape closes it and Tab wraps around at either end of it.
 *
 * Keys are taken from the whole document rather than the dialog alone, because focus falls to the page body when the
 * focused control is disabled or removed (a dialog's button while its request runs), and keys pressed then never
 * reach the dialog.
 *
 * @param event - The key pressed.
 * @param dialog - The open dialog.
 * @param active - The element that has focus, which is the page body when focus has fallen out of the dialog.
 * @param onClose - Closes the dialog.
 */
export const handleModalKey = (
  event: Pick<KeyboardEvent, 'key' | 'shiftKey' | 'preventDefault'>,
  dialog: HTMLElement,
  active: Element | null,
  onClose: () => void,
): void => {
  if (event.key === 'Escape') {
    onClose();
    return;
  }
  if (event.key !== 'Tab') return;
  // focus outside the dialog counts as none of its elements, so Tab brings it in at the first or last one
  const focusable = Array.from(dialog.querySelectorAll<HTMLElement>(FOCUSABLE));
  const next = wrapFocusIndex(focusable.indexOf(active as HTMLElement), focusable.length, event.shiftKey);
  if (next !== null) {
    event.preventDefault();
    focusable[next].focus();
  }
};

/**
 * Pull focus back into an open modal when something outside it takes focus.
 *
 * @param target - The element that took focus.
 * @param dialog - The open dialog.
 */
export const keepFocusInside = (target: EventTarget | null, dialog: HTMLElement): void => {
  if (!dialog.contains(target as Node | null)) dialog.focus();
};

/** The props of {@link ModalFrame}. */
export interface ModalFrameProps {
  /** The dialog's heading, which also names it for screen readers. */
  title: ReactNode;
  /** The dialog's content. */
  children: ReactNode;
  /** The buttons at the foot of the dialog; no footer is drawn without them. */
  actions?: ReactNode;
  /** How wide the dialog is drawn. */
  size?: ModalSize;
  /** Called when the close button, Escape, or a click on the backdrop dismisses the dialog. */
  onClose: () => void;
  /** Receives the dialog element, so the caller can move focus into it. */
  dialogRef?: React.Ref<HTMLDivElement>;
}

/**
 * Draw a dialog and its backdrop in place, without a portal or focus handling.
 *
 * @param props - The dialog's title, content, actions, size, and close handler.
 * @returns The backdrop holding the dialog.
 */
export const ModalFrame: React.FC<ModalFrameProps> = ({ title, children, actions, size = ModalSize.Medium, onClose, dialogRef }) => {
  const titleId = useId();
  return (
    <Backdrop onMouseDown={onClose}>
      <Dialog
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        tabIndex={-1}
        $size={size}
        onMouseDown={(event) => event.stopPropagation()}
      >
        <Header>
          <Title id={titleId}>{title}</Title>
          <IconButton variant={ButtonVariant.Icon} size={ButtonSize.Small} aria-label="Close" onClick={onClose}>
            <FaXmark size={18} />
          </IconButton>
        </Header>
        <Body>{children}</Body>
        {actions && <Footer>{actions}</Footer>}
      </Dialog>
    </Backdrop>
  );
};

/** The props of {@link Modal}. */
export interface ModalProps extends Omit<ModalFrameProps, 'dialogRef'> {
  /** Whether the dialog is open. */
  show: boolean;
}

/**
 * A modal dialog drawn over the page: it takes focus while open, keeps Tab inside itself, closes on
 * Escape, the close button, or a backdrop click, and gives focus back to what had it before.
 *
 * @param props - Whether the dialog is open, and its title, content, actions, size, and close handler.
 * @returns The dialog in a portal on the document body, or nothing while it is closed.
 */
const Modal: React.FC<ModalProps> = ({ show, onClose, ...frame }) => {
  const dialog = useRef<HTMLDivElement>(null);
  // the document listeners outlive a render, so they read the latest close handler through a ref
  const close = useRef(onClose);
  close.current = onClose;
  useEffect(() => {
    if (!show) return;
    // focus returns to the opener on close, and the page behind the dialog stops scrolling while it is open
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const overflow = document.body.style.overflow;
    document.body.style.overflow = 'hidden';
    dialog.current?.focus();
    // Escape and Tab work wherever focus is, and focus that leaves the dialog is pulled back into it
    const onKeyDown = (event: KeyboardEvent) => {
      if (dialog.current) handleModalKey(event, dialog.current, document.activeElement, () => close.current());
    };
    const onFocusIn = (event: FocusEvent) => {
      if (dialog.current) keepFocusInside(event.target, dialog.current);
    };
    document.addEventListener('keydown', onKeyDown);
    document.addEventListener('focusin', onFocusIn);
    return () => {
      document.removeEventListener('keydown', onKeyDown);
      document.removeEventListener('focusin', onFocusIn);
      document.body.style.overflow = overflow;
      opener?.focus();
    };
  }, [show]);
  if (!show) return null;
  return createPortal(<ModalFrame {...frame} onClose={onClose} dialogRef={dialog} />, document.body);
};

export default Modal;
