import React, { useCallback, useEffect, useState } from 'react';
import { Alert, Button, Form } from 'react-bootstrap';
import { FaEye, FaShieldAlt } from 'react-icons/fa';
import { ThoriumRole } from '@models';
import { getThoriumRole } from '@utilities';
import { logoutUser, updateSingleUser } from '@thorpi';
import RoleEditor, { DeveloperOptions, getDeveloperDefaults } from './RoleEditor';
import RevokeTokenModal from './RevokeTokenModal';
import { EditPanelActions, EditPanelContent, EditPanelWrapper, EditSection, EditSubSection, PasswordInputWrapper } from './styles';

const THEMES = ['Dark', 'Light', 'Ocean', 'Automatic'];

interface EditUserPanelProps {
  username: string;
  role: ThoriumRole;
  email: string;
  theme: string;
  verified: boolean;
  local: boolean;
  isOpen: boolean;
  onRoleUpdated: (role: ThoriumRole) => void;
  onClose: () => void;
}

const EditUserPanel: React.FC<EditUserPanelProps> = ({ username, role, email, theme, verified, local, isOpen, onRoleUpdated, onClose }) => {
  const currentRoleStr = getThoriumRole(role);
  const [editRole, setEditRole] = useState(currentRoleStr);
  const [devOptions, setDevOptions] = useState<DeveloperOptions>(getDeveloperDefaults(role));
  const [editEmail, setEditEmail] = useState(email);
  const [editTheme, setEditTheme] = useState(theme || 'Automatic');
  const [editVerified, setEditVerified] = useState(verified);
  const [newPassword, setNewPassword] = useState('');
  const [showPassword, setShowPassword] = useState(false);
  const [error, setError] = useState('');
  const [success, setSuccess] = useState('');
  const [revokeError, setRevokeError] = useState('');
  const [showRevokeModal, setShowRevokeModal] = useState(false);
  const [isClosing, setIsClosing] = useState(false);

  useEffect(() => {
    if (isOpen) {
      setEditRole(getThoriumRole(role));
      setDevOptions(getDeveloperDefaults(role));
      setEditEmail(email);
      setEditTheme(theme || 'Automatic');
      setEditVerified(verified);
      setNewPassword('');
      setShowPassword(false);
      setError('');
      setSuccess('');
      setRevokeError('');
      setShowRevokeModal(false);
      setIsClosing(false);
    }
  }, [isOpen, role, email, theme, verified]);

  const handleClose = useCallback(() => {
    setIsClosing(true);
    setTimeout(() => {
      onClose();
      setIsClosing(false);
    }, 300);
  }, [onClose]);

  const handleSave = async () => {
    setError('');
    setSuccess('');

    const update: Record<string, unknown> = {};

    if (editRole !== currentRoleStr || editRole === 'Developer') {
      if (editRole === 'Developer') {
        update.role = { Developer: devOptions };
      } else {
        update.role = editRole;
      }
    }

    if (editEmail !== email) {
      update.email = editEmail;
    }

    if (editTheme !== (theme || 'Automatic')) {
      update.settings = { theme: editTheme };
    }

    if (editVerified !== verified) {
      update.verified = editVerified;
    }

    if (newPassword) {
      update.password = newPassword;
    }

    if (Object.keys(update).length === 0) {
      setError('No changes to save');
      return;
    }

    const result = await updateSingleUser(update, username, setError);
    if (result) {
      setSuccess('User updated successfully');
      setNewPassword('');
      if (update.role) {
        onRoleUpdated(update.role as ThoriumRole);
      }
    }
  };

  const handleRevokeToken = async () => {
    setShowRevokeModal(false);
    setRevokeError('');
    const result = await logoutUser(username, setRevokeError);
    if (result) {
      setSuccess('Token revoked successfully');
    }
  };

  if (!isOpen && !isClosing) return null;

  return (
    <EditPanelWrapper $isOpen={isOpen} $isClosing={isClosing}>
      <EditPanelContent>
        <EditSection>
          <label>Role</label>
          <RoleEditor
            role={editRole}
            developerOptions={devOptions}
            onRoleChange={setEditRole}
            onDeveloperOptionChange={(key) => setDevOptions((prev) => ({ ...prev, [key]: !prev[key] }))}
          />
        </EditSection>
        <hr className="my-3" />

        <EditSection>
          <label>Email</label>
          <Form.Control
            type="email"
            size="sm"
            value={editEmail}
            onChange={(e) => setEditEmail(e.target.value)}
            style={{ maxWidth: '300px' }}
          />
        </EditSection>

        <EditSubSection>
          <Form.Check
            type="switch"
            id={`verified-${username}`}
            label="Email Verified"
            checked={editVerified}
            onChange={() => setEditVerified(!editVerified)}
          />
        </EditSubSection>
        <hr className="my-3" />

        <EditSection>
          <label>Theme</label>
          <Form.Select size="sm" value={editTheme} onChange={(e) => setEditTheme(e.target.value)} style={{ maxWidth: '200px' }}>
            {THEMES.map((t) => (
              <option key={t} value={t}>
                {t}
              </option>
            ))}
          </Form.Select>
        </EditSection>
        <hr className="my-3" />

        {local && (
          <>
            <EditSection>
              <label>Password</label>
              <PasswordInputWrapper>
                <Form.Control
                  type={showPassword ? 'text' : 'password'}
                  size="sm"
                  placeholder="New password"
                  value={newPassword}
                  onChange={(e) => setNewPassword(e.target.value)}
                />
                <button type="button" className="password-toggle" onClick={() => setShowPassword((prev) => !prev)}>
                  {showPassword ? <FaShieldAlt size={14} /> : <FaEye size={14} />}
                </button>
              </PasswordInputWrapper>
            </EditSection>
            <hr className="my-3" />
          </>
        )}

        <EditSection>
          <label>Token</label>
          <Button className="warning-btn" size="sm" onClick={() => setShowRevokeModal(true)}>
            Revoke Token
          </Button>
          {revokeError && (
            <Alert variant="danger" className="mb-0 py-1 px-2">
              {revokeError}
            </Alert>
          )}
        </EditSection>

        <RevokeTokenModal
          show={showRevokeModal}
          onHide={() => setShowRevokeModal(false)}
          onConfirm={handleRevokeToken}
          username={username}
        />

        {error && (
          <Alert variant="danger" className="mb-0 text-center">
            {error}
          </Alert>
        )}
        {success && (
          <Alert variant="success" className="mb-0 text-center">
            {success}
          </Alert>
        )}

        <EditPanelActions>
          <Button className="secondary-btn" size="sm" onClick={handleClose}>
            Cancel
          </Button>
          <Button className="ok-btn" size="sm" onClick={handleSave}>
            Save
          </Button>
        </EditPanelActions>
      </EditPanelContent>
    </EditPanelWrapper>
  );
};

export default EditUserPanel;
