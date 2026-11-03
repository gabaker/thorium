import { useState } from 'react';
import { Button, Modal } from 'react-bootstrap';
import { FaBan } from 'react-icons/fa';

// project imports
import AlertBanner from '@components/shared/alerts/AlertBanner';
import { OverlayTipBottom } from '@components/shared/overlay/tips';
import { cancelHunt } from '@thorpi/hunts';
import { Hunt, isActiveHuntStatus } from '@models/entities/hunts';

type CancelHuntProps = {
  hunt: Hunt;
  setError: (error: string) => void;
  onCancelled: () => void;
};

/**
 * Whether the cancel action applies to a hunt: a real loaded entity that is still active.
 *
 * The empty-id check mirrors the `refetchIntervalMs` guard — on a missing or mismatched
 * route id the details factory leaves the `BlankHunt` placeholder (whose Created status
 * counts as active) in place, and cancelling it would POST `/entities/hunt//cancel`.
 *
 * @param hunt - The hunt the details page is showing.
 * @returns True when the hunt is loaded and still cancellable.
 */
export const canCancelHunt = (hunt: Hunt): boolean => hunt.id !== '' && isActiveHuntStatus(hunt.metadata.Hunt.status);

/**
 * The hunt details Cancel action: confirm modal, `POST /entities/hunt/{id}/cancel`, then an
 * immediate page refresh so the Cancelled badge lands without waiting for a poll tick.
 *
 * Rendered only for a loaded hunt that is still active (Created or Running) — cancel on a
 * terminal hunt is a server 400 the user can never need, and the blank placeholder has
 * nothing to cancel. Failures (non-submitter 401s, terminal-race 400s) render inside the
 * open modal — the page banner sits behind the static backdrop — and move to that banner
 * if the user closes the modal without resolving them.
 */
const CancelHuntButton: React.FC<CancelHuntProps> = ({ hunt, setError, onCancelled }) => {
  const [showCancelModal, setShowCancelModal] = useState(false);
  const [disableConfirmButton, setDisableConfirmButton] = useState(false);
  // a failed cancel's error text, rendered inside the open modal
  const [cancelError, setCancelError] = useState('');

  // nothing to cancel on the blank placeholder or once the hunt is terminal
  if (!canCancelHunt(hunt)) {
    return null;
  }

  /** Open the confirm modal, dropping any error a previously abandoned attempt left behind. */
  const openCancelModal = () => {
    setCancelError('');
    setShowCancelModal(true);
  };

  /**
   * Close the confirm modal, promoting an unresolved failure to the page banner so the
   * record survives after the in-modal alert unmounts.
   */
  const closeCancelModal = () => {
    // keep a failure the user walks away from visible on the page
    if (cancelError !== '') {
      setError(cancelError);
    }
    setShowCancelModal(false);
    setCancelError('');
  };

  /** Fire the cancel POST: close on success, surface a failure inside the open modal. */
  const handleCancelClick = async () => {
    // clear any stale failure from a prior attempt before retrying
    setCancelError('');
    setDisableConfirmButton(true);
    const cancelled = await cancelHunt(hunt.id, setCancelError);
    if (cancelled) {
      // close directly: closeCancelModal would promote this render's stale cancelError
      setShowCancelModal(false);
      onCancelled();
    }
    setDisableConfirmButton(false);
  };

  return (
    <>
      <OverlayTipBottom tip={`Cancel hunt "${hunt.name}"`}>
        <Button className="icon-btn mx-1" variant="" aria-label="Cancel hunt" onClick={openCancelModal}>
          <FaBan size={20} />
        </Button>
      </OverlayTipBottom>

      <Modal show={showCancelModal} onHide={closeCancelModal} backdrop="static" keyboard={false}>
        <Modal.Header closeButton>
          <Modal.Title>Confirm cancellation</Modal.Title>
        </Modal.Header>
        <Modal.Body>
          <div className="text-center">
            Do you really want to cancel this hunt? Running chunks stop within one chunk and the hunt cannot be resumed.
            <div className="pt-4">
              <b>{hunt.name}</b>
            </div>
            <b>{hunt.id}</b>
            {cancelError !== '' && <AlertBanner className="mt-4">{cancelError}</AlertBanner>}
          </div>
        </Modal.Body>
        <Modal.Footer className="d-flex justify-content-center">
          <Button
            className="danger-btn"
            onClick={() => {
              void handleCancelClick();
            }}
            disabled={disableConfirmButton}
          >
            Confirm
          </Button>
          <Button className="primary-btn" onClick={closeCancelModal}>
            Keep Running
          </Button>
        </Modal.Footer>
      </Modal>
    </>
  );
};

export default CancelHuntButton;
