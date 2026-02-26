import React, { useState } from 'react';
import { Alert, Button, Modal } from 'react-bootstrap';
import { OverlayTipLeft } from '@components';
import { deleteUser } from '@thorpi';
import { ActionsContainer } from './styles';

interface UserActionsProps {
  username: string;
  token: string;
  expires: string;
  isEditing: boolean;
  onEditToggle: () => void;
  onDelete: () => void;
  onMasquerade: (token: string, expires: string) => void;
}

const UserActions: React.FC<UserActionsProps> = ({ username, token, expires, isEditing, onEditToggle, onDelete, onMasquerade }) => {
  const [showDeleteModal, setShowDeleteModal] = useState(false);
  const [deleteError, setDeleteError] = useState('');
  const [showMasqueradeModal, setShowMasqueradeModal] = useState(false);

  const handleDelete = async () => {
    const result = await deleteUser(username, setDeleteError);
    if (result) {
      setShowDeleteModal(false);
      onDelete();
    }
  };

  return (
    <ActionsContainer>
      <OverlayTipLeft tip="Edit this user's role, email, theme, password, and verification status.">
        <Button className={isEditing ? 'ok-btn' : 'secondary-btn'} size="sm" onClick={onEditToggle}>
          Edit
        </Button>
      </OverlayTipLeft>

      <OverlayTipLeft
        tip={`Masquerade as ${username} after logging out of your current Thorium Session. This is used to troubleshoot Thorium UI issues that are specific to an individual user.`}
      >
        <Button className="primary-btn" size="sm" onClick={() => setShowMasqueradeModal(true)}>
          Masquerade
        </Button>
      </OverlayTipLeft>

      <OverlayTipLeft tip="Delete this user.">
        <Button className="warning-btn" size="sm" onClick={() => setShowDeleteModal(true)}>
          Delete
        </Button>
      </OverlayTipLeft>

      <Modal show={showMasqueradeModal} onHide={() => setShowMasqueradeModal(false)} backdrop="static" keyboard={false}>
        <Modal.Header closeButton>
          <Modal.Title>Masquerade as {username}?</Modal.Title>
        </Modal.Header>
        <Modal.Body>
          Do you really want to logout of your current session and login as <b>{username}</b>?
        </Modal.Body>
        <Modal.Footer className="d-flex justify-content-center">
          <Button
            className="warning-btn"
            onClick={() => {
              setShowMasqueradeModal(false);
              onMasquerade(token, expires);
            }}
          >
            Confirm
          </Button>
        </Modal.Footer>
      </Modal>

      <Modal show={showDeleteModal} onHide={() => setShowDeleteModal(false)} backdrop="static" keyboard={false}>
        <Modal.Header closeButton>
          <Modal.Title>Confirm deletion?</Modal.Title>
        </Modal.Header>
        <Modal.Body>
          Do you really want to delete <b>{username}</b>
          {"'s"} user account?
          {deleteError !== '' && (
            <Alert variant="danger" className="mt-2">
              {deleteError}
            </Alert>
          )}
        </Modal.Body>
        <Modal.Footer className="d-flex justify-content-center">
          <Button className="danger-btn" onClick={handleDelete}>
            Confirm
          </Button>
        </Modal.Footer>
      </Modal>
    </ActionsContainer>
  );
};

export default UserActions;
