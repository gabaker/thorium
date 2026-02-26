import React, { useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { Alert, Badge, Button, Col, Container, Form, Row } from 'react-bootstrap';
import styled from 'styled-components';
import Avatar from '@mui/material/Avatar';

// project imports
import { Subtitle, Page } from '@components';
import { RevokeTokenModal } from '../../components/users';
import { getThoriumRole, useAuth } from '@utilities';
import { updateUser } from '@thorpi';

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

  /*$grid-breakpoints: (
    xs: 0,
    sm: 576px,
    md: 768px,
    lg: 992px,
    xl: 1200px,
    xxl: 1400px
  );*/

  // 576px
  @media (max-width: 576px) {
    width: 400px;
  }
`;

const Themes = ['Dark', 'Light', 'Ocean', 'Automatic'];

const Role = ({ role }) => {
  const roleString = getThoriumRole(role);
  return (
    <Container>
      <Row>
        <Col xs={2}>
          <Subtitle>Role</Subtitle>
        </Col>
        <Col>
          {role && role == 'Admin' && (
            <Badge pill bg="" className="bg-maroon px-3 py-2">
              {roleString}
            </Badge>
          )}
          {role && role == 'Developer' && (
            <Badge pill bg="" className="bg-corn-flower px-3 py-2">
              {roleString}
            </Badge>
          )}
          {role && role == 'User' && (
            <Badge pill bg="" className="bg-cadet px-3 py-2">
              {roleString}
            </Badge>
          )}
        </Col>
      </Row>
    </Container>
  );
};

const Groups = ({ groups }) => {
  return (
    <Container>
      <Row>
        <Col xs={2}>
          <Subtitle className="me-4">Groups</Subtitle>
        </Col>
        <Col>
          {groups &&
            groups.sort().map((group, idx) => (
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
  const { userInfo, revoke } = useAuth();
  const navigate = useNavigate();

  const handleRevoke = () => {
    setShowRevokeTokenModal(false);
    revoke().then(() => {
      navigate('/');
    });
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
          <Button className="danger-btn" onClick={() => setShowRevokeTokenModal(true)}>
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
      <RevokeTokenModal show={showRevokeTokenModal} onHide={() => setShowRevokeTokenModal(false)} onConfirm={handleRevoke} />
    </Container>
  );
};

const Theme = ({ theme }) => {
  const { refreshUserInfo } = useAuth();
  // Send API new user theme settings
  const updateTheme = async (theme) => {
    const settings = { settings: { theme: theme } };
    updateUser(settings, console.log).then(() => {
      refreshUserInfo(true);
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

const ChangePassword = () => {
  const [newPassword, setNewPassword] = useState('');
  const [confirmPassword, setConfirmPassword] = useState('');
  const [error, setError] = useState('');
  const [warning, setWarning] = useState('');
  const [success, setSuccess] = useState(false);
  const navigate = useNavigate();

  const handleChangePassword = async () => {
    setError('');
    setWarning('');
    setSuccess(false);
    if (!newPassword) {
      setWarning('You must enter a new password');
      return;
    }
    if (newPassword !== confirmPassword) {
      setWarning('Passwords do not match');
      return;
    }
    const result = await updateUser({ password: newPassword }, setError);
    if (result) {
      setSuccess(true);
      setNewPassword('');
      setConfirmPassword('');
    }
  };

  return (
    <Container>
      <Row>
        <Col xs={2}>
          <Subtitle>Password</Subtitle>
        </Col>
        <Col>
          <Form>
            <Form.Group className="mb-2">
              <Form.Control
                type="password"
                placeholder="New Password"
                value={newPassword}
                onChange={(e) => setNewPassword(e.target.value)}
              />
            </Form.Group>
            <Form.Group className="mb-2">
              <Form.Control
                type="password"
                placeholder="Confirm Password"
                value={confirmPassword}
                onChange={(e) => setConfirmPassword(e.target.value)}
              />
            </Form.Group>
          </Form>
          {warning != '' && (
            <Alert variant="warning">
              <center>{warning}</center>
            </Alert>
          )}
          {error != '' && (
            <Alert variant="danger">
              <center>{error}</center>
            </Alert>
          )}
          {success && (
            <Alert variant="success">
              <center>Password changed successfully. You will need to log in again.</center>
            </Alert>
          )}
        </Col>
      </Row>
      <Row>
        <Col className="d-flex justify-content-center pt-2">
          <Button className="primary-btn" onClick={() => handleChangePassword()}>
            Change Password
          </Button>
        </Col>
      </Row>
    </Container>
  );
};

const ProfileContainer = () => {
  const { userInfo } = useAuth();

  return (
    <Page title="Profile · Thorium" className="d-flex justify-content-center">
      <ProfileCard>
        <Row className="d-flex justify-content-center">
          <Avatar sx={{ width: 150, height: 150 }} />
        </Row>
        <Row className="d-flex justify-content-center">
          <h2 className="pt-3 d-flex justify-content-center">{userInfo?.username}</h2>
        </Row>
        <hr />
        {/* Group membership */}
        <Groups groups={userInfo?.groups} />
        <hr />
        {/* Thorium role (not group role) */}
        <Role role={userInfo?.role} />
        <hr />
        {/* User Token */}
        <Token />
        <hr />
        {/* UI Theme */}
        <Theme theme={userInfo?.settings?.theme} />
        {/* Change Password (only for local users) */}
        {userInfo?.local && (
          <>
            <hr />
            <ChangePassword />
          </>
        )}
      </ProfileCard>
    </Page>
  );
};

export default ProfileContainer;
