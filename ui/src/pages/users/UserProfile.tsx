import { useRef, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { Badge, Button, Col, Container, Form, Modal, Row } from 'react-bootstrap';
import styled from 'styled-components';
import { FaCircleUser } from 'react-icons/fa6';

// project imports
import Page from '@components/pages/Page';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import Subtitle from '@components/shared/titles/Subtitle';
import { useAuth } from '@utilities/auth';
import { fileToResizedBlob } from '@utilities/image';
import { getThoriumRoleBadge } from '@utilities/role';
import { useUserImage } from '@utilities/useUserImage';
import { deleteUserImage, updateUser, uploadUserImage } from '@thorpi/users';
import { ThoriumRole } from '@models/users';

// spec: ./UserProfile.spec.md

const ProfileCard = styled.div`
  width: 50rem;
  border: none;
  background-color: var(--thorium-body-bg);
  flex: column;
  justify-content-center;
  align-items: center;
  padding: 1rem;

  // hidden token
  .hidden {
    color: var(--thorium-secondary-text);
  }

  .wrap-token {
    overflow-wrap: anywhere;
  }

  @media (max-width: 576px) {
    width: 400px;
  }
`;

const Themes = ['Dark', 'Light', 'Ocean', 'Crab', 'Automatic'];

type RoleProps = {
  role: ThoriumRole;
};

const Role: React.FC<RoleProps> = ({ role }) => {
  const badge = getThoriumRoleBadge(role);
  return (
    <Container>
      <Row>
        <Col xs={2}>
          <Subtitle>Role</Subtitle>
        </Col>
        <Col>
          {badge && (
            <Badge pill bg="" className={`${badge.className} px-3 py-2`}>
              {badge.label}
            </Badge>
          )}
        </Col>
      </Row>
    </Container>
  );
};

const RevokeTokenModal = ({ show, onHide }: { show: boolean; onHide: () => void }) => {
  const { revoke } = useAuth();
  const navigate = useNavigate();
  // call thorium logout route and then
  const handleRevoke = () => {
    void revoke().then(() => {
      void navigate('/');
    });
  };
  return (
    <Modal show={show} onHide={onHide}>
      <Modal.Header closeButton>
        <Modal.Title>Revoke Your Token?</Modal.Title>
      </Modal.Header>
      <Modal.Body>
        Revoking your token will automatically log you out of this page and any currently running or queued analysis jobs (reactions) may
        fail. Are you sure?
      </Modal.Body>
      <Modal.Footer className="d-flex justify-content-center">
        <Button className="danger-btn" onClick={() => handleRevoke()}>
          Confirm
        </Button>
      </Modal.Footer>
    </Modal>
  );
};

const Groups = ({ groups }: { groups: string[] | undefined }) => {
  return (
    <Container>
      <Row>
        <Col xs={2}>
          <Subtitle className="me-4">Groups</Subtitle>
        </Col>
        <Col>
          {groups &&
            [...groups].sort().map((group: string, idx: number) => (
              <Badge key={idx} pill bg="" className="bg-blue px-3 py-2 me-1">
                {group}
              </Badge>
            ))}
        </Col>
      </Row>
    </Container>
  );
};

const Token = () => {
  const [showRevokeTokenModal, setShowRevokeTokenModal] = useState(false);
  const [tokenShowing, setTokenShowing] = useState(false);
  const { userInfo } = useAuth();

  // toggle display of revoke token model from previous value
  const handleToggleRevokeTokenModalDisplay = () => {
    setShowRevokeTokenModal((prev) => !prev);
  };
  return (
    <Container>
      <Row>
        <Col xs={2}>
          <Subtitle>Token</Subtitle>
        </Col>
        <Col xs={10}>
          <Row>
            <Col>
              <div className="wrap-token">
                {tokenShowing ? (
                  <p>{userInfo?.token}</p>
                ) : (
                  <p className="hidden">****************************************************************</p>
                )}
              </div>
            </Col>
          </Row>
        </Col>
      </Row>
      <Row>
        <Col className="d-flex justify-content-center pt-2">
          <Button className="primary-btn" onClick={() => setTokenShowing(!tokenShowing)}>
            {tokenShowing ? 'Hide' : 'Show'}
          </Button>
          <Button className="danger-btn" onClick={() => handleToggleRevokeTokenModalDisplay()}>
            Revoke
          </Button>
        </Col>
      </Row>
      <Row className="pt-3">
        <Col xs={2}>
          <Subtitle>Expiry</Subtitle>
        </Col>
        <Col>
          <p>{userInfo?.token_expiration}</p>
        </Col>
      </Row>
      <RevokeTokenModal show={showRevokeTokenModal} onHide={handleToggleRevokeTokenModalDisplay} />
    </Container>
  );
};

const Theme = ({ theme }: { theme: string | undefined }) => {
  const { refreshUserInfo } = useAuth();
  // Send API new user theme settings
  const updateTheme = (theme: string) => {
    const settings = { settings: { theme: theme } };
    void updateUser(settings, console.log).then(() => {
      void refreshUserInfo(true);
    });
  };
  return (
    <Container>
      <Row>
        <Col xs={2}>
          <Subtitle>Theme</Subtitle>
        </Col>
        <Col className="d-flex justify-content-start">
          <Form>
            <Form.Group>
              <Form.Select value={theme ? theme : ''} onChange={(e) => updateTheme(String(e.target.value))}>
                {Themes.map((theme) => (
                  <option key={theme} value={theme}>
                    {theme}
                  </option>
                ))}
              </Form.Select>
            </Form.Group>
          </Form>
        </Col>
      </Row>
    </Container>
  );
};

const MethodSection = styled.div`
  display: grid;
  grid-template-columns: minmax(0, 2fr) minmax(0, 10fr);
  gap: 1rem;
  padding: 0 0.75rem;
  align-items: start;
`;

const MethodBadges = styled.div`
  display: flex;
  flex-wrap: wrap;
  gap: 0.5rem;
  margin-bottom: 0.5rem;
`;

const MethodPill = styled.span<{ $tone: 'ok' | 'warn' | 'muted' }>`
  display: inline-flex;
  align-items: center;
  border-radius: 999px;
  padding: 0.25rem 0.75rem;
  font-size: 0.85rem;
  color: var(--thorium-button-text);
  background-color: ${({ $tone }) =>
    $tone === 'ok' ? 'var(--thorium-ok-bg)' : $tone === 'warn' ? 'var(--thorium-warning-bg)' : 'var(--thorium-secondary-panel-bg)'};
`;

const MethodGuidance = styled.p`
  color: var(--thorium-secondary-text);
  font-size: 0.9rem;
  margin: 0;
`;

// Read-only summary of how this account can sign in. Linked-provider management is not shown
// because the current API does not expose a user's linked providers on whoami.
const SignInMethods: React.FC<{ local?: boolean; verified?: boolean }> = ({ local, verified }) => {
  return (
    <MethodSection>
      <Subtitle>Sign-in</Subtitle>
      <div>
        <MethodBadges>
          {/* `local` mirrors the backend ScrubbedUser.local (the account has a password set) */}
          {local && <MethodPill $tone="ok">Local Login</MethodPill>}
          <MethodPill $tone={verified ? 'ok' : 'warn'}>{verified ? 'Email verified' : 'Email not verified'}</MethodPill>
        </MethodBadges>
        <MethodGuidance>
          To add a single sign-on provider, sign in with that provider using this same email address. You&apos;ll receive an email to
          confirm linking it to your account.
        </MethodGuidance>
      </div>
    </MethodSection>
  );
};

// Longest-edge bound (px) the uploaded icon is downscaled to before upload, keeping the
// stored S3 object small.
const PROFILE_ICON_MAX_PX = 256;

const AvatarColumn = styled.div`
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 0.5rem;
`;

const AvatarImage = styled.img`
  width: 150px;
  height: 150px;
  border-radius: 50%;
  object-fit: cover;
  border: 1px solid var(--thorium-panel-border);
`;

const AvatarActions = styled.div`
  display: flex;
  gap: 0.5rem;
`;

// styled replacement for the legacy react-bootstrap Button; keeps the global primary-btn/danger-btn
// classes so the two profile-icon actions match the app's button styling without pulling in react-bootstrap
const AvatarButton = styled.button`
  border: none;
`;

const HiddenFileInput = styled.input`
  display: none;
`;

// Profile icon with upload/remove controls. The icon is resized client-side and uploaded as
// multipart form data; the backend stores it in S3 and the avatar is fetched lazily via
// useUserImage (so whoami stays lightweight).
const ProfileImage = () => {
  const { userInfo, refreshUserInfo } = useAuth();
  const { imageUrl, reload } = useUserImage(userInfo?.username, userInfo?.has_image);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // refetch the icon (after an upload/remove) and refresh has_image on the cached user
  const refresh = async () => {
    await refreshUserInfo(true);
    reload();
  };

  // resize the selected image and upload it as this user's icon
  const handleFileSelected = async (e: React.ChangeEvent<HTMLInputElement>) => {
    const file = e.target.files?.[0];
    // reset the input so selecting the same file again re-triggers onChange
    e.target.value = '';
    if (!file) {
      return;
    }
    if (!file.type.startsWith('image/')) {
      setError('Please choose an image file.');
      return;
    }
    setError(null);
    setBusy(true);
    try {
      const image = await fileToResizedBlob(file, PROFILE_ICON_MAX_PX);
      if (await uploadUserImage(image, setError)) {
        await refresh();
      }
    } catch {
      setError('Failed to process the selected image.');
    } finally {
      setBusy(false);
    }
  };

  // clear this user's icon
  const handleRemove = async () => {
    setError(null);
    setBusy(true);
    if (await deleteUserImage(setError)) {
      await refresh();
    }
    setBusy(false);
  };

  return (
    <AvatarColumn>
      {imageUrl ? <AvatarImage src={imageUrl} alt="Profile icon" /> : <FaCircleUser size={150} />}
      <AvatarActions>
        <AvatarButton type="button" className="primary-btn" disabled={busy} onClick={() => fileInputRef.current?.click()}>
          {userInfo?.has_image ? 'Change' : 'Upload'}
        </AvatarButton>
        {userInfo?.has_image && (
          <AvatarButton type="button" className="danger-btn" disabled={busy} onClick={() => void handleRemove()}>
            Remove
          </AvatarButton>
        )}
      </AvatarActions>
      <HiddenFileInput ref={fileInputRef} type="file" accept="image/*" onChange={(e) => void handleFileSelected(e)} />
      {error && <AlertBanner severity={Severity.Error}>{error}</AlertBanner>}
    </AvatarColumn>
  );
};

const UserProfile = () => {
  const { userInfo } = useAuth();

  return (
    <Page title="Profile · Thorium" className="d-flex justify-content-center">
      <ProfileCard>
        <Row className="d-flex justify-content-center">
          <ProfileImage />
        </Row>
        <Row className="d-flex justify-content-center">
          <h2 className="pt-3 d-flex justify-content-center">{userInfo?.username}</h2>
        </Row>
        <hr />
        {/* Group membership */}
        <Groups groups={userInfo?.groups} />
        <hr />
        {/* Thorium role (not group role) */}
        {userInfo && <Role role={userInfo.role} />}
        <hr />
        {/* User Token */}
        <Token />
        <hr />
        {/* Sign-in methods (local password / SSO + verification status) */}
        <SignInMethods local={userInfo?.local} verified={userInfo?.verified} />
        <hr />
        {/* UI Theme */}
        <Theme theme={userInfo?.settings?.theme} />
      </ProfileCard>
    </Page>
  );
};

export default UserProfile;
