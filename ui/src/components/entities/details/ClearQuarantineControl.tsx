import { useState } from 'react';
import { Button, Modal } from 'react-bootstrap';

// project imports
import AlertBanner from '@components/shared/alerts/AlertBanner';
import { deleteEntityTags } from '@thorpi/entities';
import { CLEARABLE_QUARANTINE_STATUSES, YARA_RULE_STATUS_TAG, YaraRule } from '@models/entities/rules/yara';

type ClearQuarantineProps = {
  rule: YaraRule;
  setError: (error: string) => void;
  onCleared: () => void;
};

/**
 * The rule's current `YaraRuleStatus` tag values (e.g. Noisy/Slow/Broken), sorted for a
 * stable display order.
 *
 * @param rule - The yara rule to read the status tag from.
 * @returns The sorted status values, empty when the rule is not quarantined.
 */
export const quarantineStatusValues = (rule: YaraRule): string[] =>
  Object.keys(rule.tags?.[YARA_RULE_STATUS_TAG] ?? {}).sort((a: string, b: string) => a.localeCompare(b));

/**
 * The subset of the rule's status values a clear may actually delete (Noisy/Slow).
 *
 * `Broken` stays: the scanner re-tags still-broken rules (re-notifying their submitter)
 * on its next restart, so only fixing the rule's source genuinely clears it, matching
 * the set the API's exemption route clears.
 *
 * @param rule - The yara rule to read the status tag from.
 * @returns The sorted clearable status values.
 */
export const clearableQuarantineValues = (rule: YaraRule): string[] =>
  quarantineStatusValues(rule).filter((value: string) => CLEARABLE_QUARANTINE_STATUSES.includes(value));

/**
 * Whether the clear-quarantine action applies: a real loaded rule that currently carries
 * at least one clearable (Noisy/Slow) `YaraRuleStatus` value.
 *
 * Unlike the exemption control this is not admin-gated — the backend's tag delete route
 * only requires edit permission in the rule's groups — so the empty-id check (the blank
 * placeholder) and the clearable values' presence are the whole render guard.
 *
 * @param rule - The yara rule the details page is showing.
 * @returns True when the rule is loaded and clearably quarantined.
 */
export const canClearQuarantine = (rule: YaraRule): boolean => rule.id !== '' && clearableQuarantineValues(rule).length > 0;

/**
 * The yara rule details clear-quarantine action: confirm modal, a `YaraRuleStatus` tag
 * delete via `DELETE /entities/tags/{id}`, then an immediate page refresh so the status
 * badges drop without a reload.
 *
 * Rendered only while the rule carries a `YaraRuleStatus` value (clearing nothing is a
 * no-op the user can never need). Failures render inside the open modal — the page
 * banner sits behind the static backdrop — and move to that banner if the user closes
 * the modal without resolving them.
 */
const ClearQuarantineControl: React.FC<ClearQuarantineProps> = ({ rule, setError, onCleared }) => {
  const [showModal, setShowModal] = useState(false);
  const [disableConfirmButton, setDisableConfirmButton] = useState(false);
  // a failed clear's error text, rendered inside the open modal
  const [modalError, setModalError] = useState('');

  // nothing to clear on the blank placeholder or an unquarantined rule
  if (!canClearQuarantine(rule)) {
    return null;
  }

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

  /** Fire the tag delete: close on success, surface a failure inside the open modal. */
  const handleConfirmClick = async () => {
    // clear any stale failure from a prior attempt before retrying
    setModalError('');
    setDisableConfirmButton(true);
    // delete the clearable status values across the rule's groups in one request;
    // Broken is left alone since only a source fix genuinely clears it
    const values = { [YARA_RULE_STATUS_TAG]: clearableQuarantineValues(rule) };
    const cleared = await deleteEntityTags(rule.id, values, rule.groups, setModalError);
    if (cleared) {
      // close directly: closeModal would promote this render's stale modalError
      setShowModal(false);
      onCleared();
    }
    setDisableConfirmButton(false);
  };

  return (
    <>
      <Button size="sm" className="primary-btn me-2" onClick={openModal}>
        Clear Quarantine
      </Button>

      <Modal show={showModal} onHide={closeModal} backdrop="static" keyboard={false}>
        <Modal.Header closeButton>
          <Modal.Title>Confirm clearing quarantine</Modal.Title>
        </Modal.Header>
        <Modal.Body>
          <div className="text-center">
            Do you really want to clear the quarantine status from this rule? It rejoins live scanning and new hunts, and the event handler
            may auto-quarantine it again if it keeps scanning noisy or slow.
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

export default ClearQuarantineControl;
