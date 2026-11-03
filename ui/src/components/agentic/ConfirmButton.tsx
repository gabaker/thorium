import { FC, ReactNode, useState } from 'react';

// project imports
import { Muted } from './shared';
import { Button, ButtonSize, ButtonVariant } from '@components/shared/buttons';

/** Props for {@link ConfirmButton}. */
interface ConfirmButtonProps {
  /** The first button's content */
  children: ReactNode;
  /** The text of the confirming button */
  confirmText?: string;
  /** The text of the button that backs out, `Cancel` unless given */
  cancelText?: string;
  /** A short question shown before the confirming button, such as "Fail this run?"; none unless given */
  question?: string;
  /** Called after the user confirms */
  onConfirm: () => void;
  /** The test id of the first button; the confirming button adds `-confirm` */
  testId: string;
  /** Whether the button is disabled */
  disabled?: boolean;
  /** The tooltip of the first button, such as the reason it is disabled */
  title?: string;
  /** The size of the buttons; Medium like the other header actions unless the button sits in a dense table row */
  size?: ButtonSize;
}

/** A destructive button that needs a second click to act, so a stray click never deletes. */
export const ConfirmButton: FC<ConfirmButtonProps> = ({
  children,
  confirmText = 'Confirm',
  cancelText = 'Cancel',
  question,
  onConfirm,
  testId,
  disabled,
  title,
  size = ButtonSize.Medium,
}) => {
  const [armed, setArmed] = useState(false);
  if (!armed) {
    return (
      <Button
        variant={ButtonVariant.Danger}
        size={size}
        data-testid={testId}
        disabled={disabled}
        title={title || undefined}
        onClick={() => setArmed(true)}
      >
        {children}
      </Button>
    );
  }
  return (
    <>
      {question && <Muted>{question}</Muted>}
      <Button
        variant={ButtonVariant.Danger}
        size={size}
        data-testid={`${testId}-confirm`}
        disabled={disabled}
        onClick={() => {
          setArmed(false);
          onConfirm();
        }}
      >
        {confirmText}
      </Button>
      <Button variant={ButtonVariant.Secondary} size={size} onClick={() => setArmed(false)}>
        {cancelText}
      </Button>
    </>
  );
};
