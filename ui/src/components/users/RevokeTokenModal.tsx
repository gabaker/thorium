import React from 'react';
import { Button, Modal } from 'react-bootstrap';

interface RevokeTokenModalProps {
  show: boolean;
  onHide: () => void;
  onConfirm: () => void;
  username?: string;
}

const RevokeTokenModal: React.FC<RevokeTokenModalProps> = ({ show, onHide, onConfirm, username }) => {
  return (
    <Modal show={show} onHide={onHide}>
      <Modal.Header closeButton>
        <Modal.Title>Revoke {username ? `${username}'s` : 'Your'} Token?</Modal.Title>
      </Modal.Header>
      <Modal.Body>
        Revoking {username ? `${username}'s` : 'your'} token will automatically log {username ? 'them' : 'you'} out and any currently
        running or queued analysis jobs (reactions) may fail. Are you sure?
      </Modal.Body>
      <Modal.Footer className="d-flex justify-content-center">
        <Button className="danger-btn" onClick={onConfirm}>
          Confirm
        </Button>
      </Modal.Footer>
    </Modal>
  );
};

export default RevokeTokenModal;
