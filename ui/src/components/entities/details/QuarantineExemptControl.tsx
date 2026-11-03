import { useState } from 'react';
import { Button, Modal } from 'react-bootstrap';

// project imports
import AlertBanner from '@components/shared/alerts/AlertBanner';
import { setQuarantineExemption, clearQuarantineExemption } from '@thorpi/entities';
import { useAuth } from '@utilities/auth';
import { isAdmin } from '@utilities/permissions';
import { YaraRule } from '@models/entities/rules/yara';
import { UserInfo } from '@models/users';

type QuarantineExemptProps = {
  rule: YaraRule;
  setError: (error: string) => void;
  onChanged: () => void;
};

/**
 * Whether the quarantine exemption control applies: an admin viewing a real loaded rule.
 *
 * `userInfo` is null while the whoami fetch is in flight, so the control renders nothing
 * rather than flashing an admin action at a user who may not be one — mirroring the
 * backend's admin-only gate on the exemption route. The empty-id check mirrors
 * `canCancelHunt`: the blank placeholder has nothing to exempt.
 *
 * @param userInfo - The logged-in user (null while loading).
 * @param rule - The yara rule the details page is showing.
 * @returns True when the exemption control may render.
 */
export const canManageQuarantineExemption = (userInfo: UserInfo | null, rule: YaraRule): boolean =>
  userInfo !== null && isAdmin(userInfo) && rule.id !== '';

/**
 * The yara rule details quarantine-exemption action: confirm modal, a POST or DELETE on
 * `/entities/rules/{id}/quarantine-exemption`, then an immediate page refresh so the
 * Exempt badge lands without a reload.
 *
 * Rendered only for admins (the backend rejects everyone else) once whoami has loaded.
 * The button toggles with the rule's current state: granting an exemption also clears
 * any Noisy/Slow quarantine status server-side, while removing one only re-enables
 * future auto-quarantine. Failures render inside the open modal — the page banner sits
 * behind the static backdrop — and move to that banner if the user closes the modal
 * without resolving them.
 */
const QuarantineExemptControl: React.FC<QuarantineExemptProps> = ({ rule, setError, onChanged }) => {
  const { userInfo } = useAuth();
  const [showModal, setShowModal] = useState(false);
  const [disableConfirmButton, setDisableConfirmButton] = useState(false);
  // a failed exemption toggle's error text, rendered inside the open modal
  const [modalError, setModalError] = useState('');

  // nothing to render while whoami loads, for non-admins, or on the blank placeholder
  if (!canManageQuarantineExemption(userInfo, rule)) {
    return null;
  }

  // the rule's current exemption state decides which action the button offers
  const exempt = rule.metadata.YaraRule.quarantine_exempt ?? false;

  /** Open the confirm modal, dropping any error a previously abandoned attempt left behind. */
  const openModal = () => {
    setModalError('');
    setShowModal(true);
  };

  /**
   * Close the confirm modal, promoting an unresolved failure to the page banner so the
   * record survives after the in-modal alert unmounts.
   */
  const closeModal = () => {
    // keep a failure the user walks away from visible on the page
    if (modalError !== '') {
      setError(modalError);
    }
    setShowModal(false);
    setModalError('');
  };

  /** Fire the exemption toggle: close on success, surface a failure inside the open modal. */
  const handleConfirmClick = async () => {
    // clear any stale failure from a prior attempt before retrying
    setModalError('');
    setDisableConfirmButton(true);
    const changed = exempt ? await clearQuarantineExemption(rule.id, setModalError) : await setQuarantineExemption(rule.id, setModalError);
    if (changed) {
      // close directly: closeModal would promote this render's stale modalError
      setShowModal(false);
      onChanged();
    }
    setDisableConfirmButton(false);
  };

  return (
    <>
      <Button size="sm" className="primary-btn me-2" onClick={openModal}>
        {exempt ? 'Remove Exemption' : 'Exempt from Quarantine'}
      </Button>

      <Modal show={showModal} onHide={closeModal} backdrop="static" keyboard={false}>
        <Modal.Header closeButton>
          <Modal.Title>{exempt ? 'Confirm removing the exemption' : 'Confirm quarantine exemption'}</Modal.Title>
        </Modal.Header>
        <Modal.Body>
          <div className="text-center">
            {exempt
              ? 'Do you really want to remove the quarantine exemption from this rule? The event handler may ' +
                'auto-quarantine it again if it keeps scanning noisy or slow.'
              : 'Do you really want to exempt this rule from quarantine? Setting the exemption also clears any ' +
                'Noisy/Slow quarantine status, and the rule will never be auto-quarantined while exempt.'}
            <div className="pt-4">
              <b>{rule.name}</b>
            </div>
            <b>{rule.id}</b>
            {modalError !== '' && <AlertBanner className="mt-4">{modalError}</AlertBanner>}
          </div>
        </Modal.Body>
        <Modal.Footer className="d-flex justify-content-center">
          <Button
            className="danger-btn"
            onClick={() => {
              void handleConfirmClick();
            }}
            disabled={disableConfirmButton}
          >
            Confirm
          </Button>
          <Button className="primary-btn" onClick={closeModal}>
            Cancel
          </Button>
        </Modal.Footer>
      </Modal>
    </>
  );
};

export default QuarantineExemptControl;
