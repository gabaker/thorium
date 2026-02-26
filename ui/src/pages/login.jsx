import React, { useState, useEffect } from 'react';
import { Link, useNavigate, useLocation, useSearchParams } from 'react-router-dom';
import { Alert, Button, Col, Card, Form, Modal, Row } from 'react-bootstrap';

// project imports
import { LoadingSpinner, SimpleTitle, SimpleSubtitle, Subtitle, Title, Page } from '@components';
import { useAuth } from '@utilities';
import { getBanner, requestPasswordReset, resetPassword } from '@thorpi';

const LoginContainer = () => {
  const [showRegModal, setShowRegModal] = useState(false);
  const handleCloseRegModal = () => setShowRegModal(false);
  const handleShowRegModal = () => setShowRegModal(true);
  const [showForgotModal, setShowForgotModal] = useState(false);
  const [showResetModal, setShowResetModal] = useState(false);
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [loginErr, setLoginErr] = useState('');
  const [banner, setBanner] = useState('');
  const [loggingIn, setLoggingIn] = useState(false);
  const navigate = useNavigate();
  const { state } = useLocation();
  const [searchParams] = useSearchParams();
  const { login } = useAuth();

  // state for reset modal when coming from email link
  const [resetUsername, setResetUsername] = useState('');
  const [resetToken, setResetToken] = useState('');

  // login to Thorium and redirect if successful
  const handleAuthFormSubmit = async (username, password, handleAuthErr) => {
    setLoggingIn(true);
    setLoginErr('');
    if (await login(username, password, handleAuthErr)) {
      navigate(state?.path || '/');
    } else {
      setLoggingIn(false);
    }
  };

  // handle all key presses
  const handleFormKeyPress = async (e) => {
    // key code 13 is enter
    if (e.keyCode === 13) {
      handleAuthFormSubmit(username, password, setLoginErr);
    }
  };

  // fetch banner and set state
  const fetchBanner = async () => {
    const req = await getBanner(setBanner);
    if (req) {
      setBanner(req);
    }
  };

  // async grab banner on page load and check for reset params
  useEffect(() => {
    fetchBanner();
    // check if we have reset params from an email link
    const resetParam = searchParams.get('reset');
    const tokenParam = searchParams.get('token');
    if (resetParam && tokenParam) {
      setResetUsername(resetParam);
      setResetToken(tokenParam);
      setShowResetModal(true);
    }
  }, []);

  const RegisterModal = () => {
    const localAuth = false;
    const [regUsername, setRegUsername] = useState('');
    const [regEmail, setRegEmail] = useState('');
    const [regPass, setRegPass] = useState('');
    const [regVerifyPass, setRegVerifyPass] = useState('');
    const [regError, setRegError] = useState('');
    const [regWarning, setRegWarning] = useState('');
    const [registering, setRegistering] = useState(false);
    const { register } = useAuth();

    const handleRegister = async (username, email, pass, verifyPass) => {
      // handleRegister is a form submit, we want to clear the warnings
      // related to form entry issues
      setRegWarning('');
      setRegError('');
      setRegistering(true);
      // make sure all required fields are present
      if (username && pass && (verifyPass || !localAuth)) {
        if (localAuth && pass != verifyPass) {
          setRegWarning('The entered passwords do not match');
          setRegistering(false);
        } else {
          // create the user account
          register(username, pass, setRegError, email, 'User').then((res) => {
            // redirect after successful registration of user
            if (res) {
              navigate(state?.path || '/');
            } else {
              setRegistering(false);
            }
          });
        }
      } else {
        setRegWarning('You must specify a username and password!');
        setRegistering(false);
      }
      // registration failed, display error and do not redirect
      return;
    };

    // handle all key presses
    const checkEnterSubmit = async (e) => {
      // key code 13 is enter
      if (e.keyCode === 13) {
        handleRegister(regUsername, regEmail, regPass, regVerifyPass);
      }
    };

    return (
      <Modal show={showRegModal} onHide={handleCloseRegModal}>
        <Modal.Header closeButton>
          <Modal.Title>
            <Title>Register</Title>
          </Modal.Title>
        </Modal.Header>
        <Modal.Body>
          <Form>
            <Row>
              <Col>
                <Form.Group>
                  <Form.Label>
                    <Subtitle>Username</Subtitle>
                  </Form.Label>
                  <Form.Control
                    type="text"
                    placeholder="Enter Username"
                    value={regUsername}
                    onKeyDown={(e) => checkEnterSubmit(e)}
                    onChange={(e) => setRegUsername(String(e.target.value))}
                  />
                </Form.Group>
              </Col>
            </Row>
            <Row>
              <Col>
                <Form.Group>
                  <Form.Label>
                    <Subtitle>Email</Subtitle>
                  </Form.Label>
                  <Form.Control
                    type="text"
                    placeholder="Enter Email"
                    value={regEmail}
                    onKeyDown={(e) => checkEnterSubmit(e)}
                    onChange={(e) => setRegEmail(String(e.target.value))}
                  />
                </Form.Group>
              </Col>
            </Row>
            <Row className="my-3">
              <Col>
                <Form.Group>
                  <Form.Label>
                    <Subtitle>Password</Subtitle>
                  </Form.Label>
                  <Form.Control
                    type="password"
                    placeholder="Enter Password"
                    value={regPass}
                    onKeyDown={(e) => checkEnterSubmit(e)}
                    onChange={(e) => setRegPass(String(e.target.value))}
                  />
                  {localAuth && (
                    <>
                      <Form.Label>
                        <Subtitle>Verify Password</Subtitle>
                      </Form.Label>
                      <Form.Control
                        type="password"
                        placeholder="Verify Password"
                        value={regVerifyPass}
                        onKeyDown={(e) => checkEnterSubmit(e)}
                        onChange={(e) => setRegVerifyPass(String(e.target.value))}
                      />
                    </>
                  )}
                </Form.Group>
              </Col>
            </Row>
            {regWarning != '' && (
              <Row>
                <Alert variant="warning">
                  <center>{regWarning}</center>
                </Alert>
              </Row>
            )}
            {regError != '' && (
              <Row>
                <Alert variant="danger">
                  <center>{regError}</center>
                </Alert>
              </Row>
            )}
            {registering ? (
              <LoadingSpinner loading={registering}></LoadingSpinner>
            ) : (
              <Row>
                <Col className="d-flex justify-content-center">
                  <Button className="ok-btn m-2" onClick={() => handleRegister(regUsername, regEmail, regPass, regVerifyPass)}>
                    Submit
                  </Button>
                </Col>
              </Row>
            )}
          </Form>
        </Modal.Body>
      </Modal>
    );
  };

  const ForgotPasswordModal = () => {
    const [forgotUsername, setForgotUsername] = useState('');
    const [forgotError, setForgotError] = useState('');
    const [forgotSuccess, setForgotSuccess] = useState(false);
    const [submitting, setSubmitting] = useState(false);

    const handleSubmit = async () => {
      setForgotError('');
      setSubmitting(true);
      const success = await requestPasswordReset(forgotUsername, setForgotError);
      if (success) {
        setForgotSuccess(true);
      }
      setSubmitting(false);
    };

    const checkEnterSubmit = async (e) => {
      if (e.keyCode === 13) {
        handleSubmit();
      }
    };

    return (
      <Modal show={showForgotModal} onHide={() => setShowForgotModal(false)}>
        <Modal.Header closeButton>
          <Modal.Title>
            <Title>Forgot Password</Title>
          </Modal.Title>
        </Modal.Header>
        <Modal.Body>
          {forgotSuccess ? (
            <Alert variant="success">
              <center>If an account with that username exists, a password reset email has been sent.</center>
            </Alert>
          ) : (
            <Form>
              <Row>
                <Col>
                  <Form.Group>
                    <Form.Label>
                      <Subtitle>Username</Subtitle>
                    </Form.Label>
                    <Form.Control
                      type="text"
                      placeholder="Enter Username"
                      value={forgotUsername}
                      onKeyDown={(e) => checkEnterSubmit(e)}
                      onChange={(e) => setForgotUsername(String(e.target.value))}
                    />
                  </Form.Group>
                </Col>
              </Row>
              {forgotError != '' && (
                <Row className="mt-2">
                  <Alert variant="danger">
                    <center>{forgotError}</center>
                  </Alert>
                </Row>
              )}
              {submitting ? (
                <LoadingSpinner loading={submitting}></LoadingSpinner>
              ) : (
                <Row className="mt-3">
                  <Col className="d-flex justify-content-center">
                    <Button className="ok-btn" onClick={() => handleSubmit()}>
                      Send Reset Email
                    </Button>
                  </Col>
                </Row>
              )}
            </Form>
          )}
        </Modal.Body>
      </Modal>
    );
  };

  const ResetPasswordModal = () => {
    const [newPassword, setNewPassword] = useState('');
    const [confirmPassword, setConfirmPassword] = useState('');
    const [resetError, setResetError] = useState('');
    const [resetWarning, setResetWarning] = useState('');
    const [resetSuccess, setResetSuccess] = useState(false);
    const [submitting, setSubmitting] = useState(false);

    const handleSubmit = async () => {
      setResetError('');
      setResetWarning('');
      if (!newPassword) {
        setResetWarning('You must enter a new password');
        return;
      }
      if (newPassword !== confirmPassword) {
        setResetWarning('Passwords do not match');
        return;
      }
      setSubmitting(true);
      const success = await resetPassword(resetUsername, resetToken, newPassword, setResetError);
      if (success) {
        setResetSuccess(true);
      }
      setSubmitting(false);
    };

    const checkEnterSubmit = async (e) => {
      if (e.keyCode === 13) {
        handleSubmit();
      }
    };

    return (
      <Modal show={showResetModal} onHide={() => setShowResetModal(false)}>
        <Modal.Header closeButton>
          <Modal.Title>
            <Title>Reset Password</Title>
          </Modal.Title>
        </Modal.Header>
        <Modal.Body>
          {resetSuccess ? (
            <Alert variant="success">
              <center>Your password has been reset. You can now log in with your new password.</center>
            </Alert>
          ) : (
            <Form>
              <Row>
                <Col>
                  <Form.Group>
                    <Form.Label>
                      <Subtitle>New Password</Subtitle>
                    </Form.Label>
                    <Form.Control
                      type="password"
                      placeholder="Enter New Password"
                      value={newPassword}
                      onKeyDown={(e) => checkEnterSubmit(e)}
                      onChange={(e) => setNewPassword(String(e.target.value))}
                    />
                  </Form.Group>
                </Col>
              </Row>
              <Row className="mt-2">
                <Col>
                  <Form.Group>
                    <Form.Label>
                      <Subtitle>Confirm Password</Subtitle>
                    </Form.Label>
                    <Form.Control
                      type="password"
                      placeholder="Confirm New Password"
                      value={confirmPassword}
                      onKeyDown={(e) => checkEnterSubmit(e)}
                      onChange={(e) => setConfirmPassword(String(e.target.value))}
                    />
                  </Form.Group>
                </Col>
              </Row>
              {resetWarning != '' && (
                <Row className="mt-2">
                  <Alert variant="warning">
                    <center>{resetWarning}</center>
                  </Alert>
                </Row>
              )}
              {resetError != '' && (
                <Row className="mt-2">
                  <Alert variant="danger">
                    <center>{resetError}</center>
                  </Alert>
                </Row>
              )}
              {submitting ? (
                <LoadingSpinner loading={submitting}></LoadingSpinner>
              ) : (
                <Row className="mt-3">
                  <Col className="d-flex justify-content-center">
                    <Button className="ok-btn" onClick={() => handleSubmit()}>
                      Reset Password
                    </Button>
                  </Col>
                </Row>
              )}
            </Form>
          )}
        </Modal.Body>
      </Modal>
    );
  };

  return (
    <Page title="Login · Thorium">
      <Row>
        <Col className="d-flex justify-content-center align-items-center">
          <Card className="p-4 d-flex justify-content-center align-items-center panel">
            <Card.Title>
              <SimpleTitle>Welcome to Thorium!</SimpleTitle>
            </Card.Title>
            <Card.Body>
              {banner != null && banner != '' && (
                <Row>
                  <Col className="d-flex justify-content-center">
                    <pre className="banner">
                      <center>{String(banner)}</center>
                    </pre>
                  </Col>
                </Row>
              )}
              <Row>
                <Col className="d-flex justify-content-center">
                  <Form.Control
                    className="m-2 login"
                    type="text"
                    value={username}
                    placeholder="username"
                    onChange={(e) => setUsername(String(e.target.value))}
                    onKeyDown={(e) => handleFormKeyPress(e)}
                  />
                </Col>
              </Row>
              <Row>
                <Col className="d-flex justify-content-center">
                  <Form.Control
                    className="m-2 login"
                    type="password"
                    value={password}
                    placeholder="password"
                    onChange={(e) => setPassword(String(e.target.value))}
                    onKeyDown={(e) => handleFormKeyPress(e)}
                  />
                </Col>
              </Row>
              {loggingIn ? (
                <LoadingSpinner loading={loggingIn}></LoadingSpinner>
              ) : (
                <>
                  <Row className="mt-3">
                    <Col className="d-flex justify-content-center align-items-center">
                      <SimpleSubtitle>
                        New user? Create an&nbsp;
                        <Link to="/auth" onClick={() => handleShowRegModal()}>
                          account
                        </Link>
                        .
                      </SimpleSubtitle>
                    </Col>
                  </Row>
                  <Row>
                    <Col className="d-flex justify-content-center align-items-center">
                      <SimpleSubtitle>
                        <Link to="/auth" onClick={() => setShowForgotModal(true)}>
                          Forgot password?
                        </Link>
                      </SimpleSubtitle>
                    </Col>
                  </Row>
                  <Row>
                    {loginErr != '' && (
                      <center>
                        <Alert variant="danger">{loginErr}</Alert>
                      </center>
                    )}
                  </Row>
                  <Row>
                    <Col className="d-flex justify-content-center">
                      <Button
                        className="primary-btn"
                        onClick={() => handleAuthFormSubmit(username, password, setLoginErr)}
                        variant="success"
                      >
                        Login
                      </Button>
                    </Col>
                  </Row>
                </>
              )}
            </Card.Body>
            <RegisterModal />
            <ForgotPasswordModal />
            <ResetPasswordModal />
          </Card>
        </Col>
      </Row>
    </Page>
  );
};

export default LoginContainer;
