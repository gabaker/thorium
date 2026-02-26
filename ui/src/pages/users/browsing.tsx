import React, { useEffect, useState } from 'react';
import { Alert, Button, ButtonToolbar, Col, Row } from 'react-bootstrap';

import { LoadingSpinner, Title, Page } from '@components';
import { useAuth } from '@utilities';
import { listUsers, syncLdap } from '@thorpi';
import { UserCard } from '../../components/users';
import { UserInfo } from '@models';

const Users: React.FC = () => {
  const [loading, setLoading] = useState(false);
  const [users, setUsers] = useState<UserInfo[]>([]);
  const [syncing, setSyncing] = useState(false);
  const [syncError, setSyncError] = useState('');
  const [syncSuccess, setSyncSuccess] = useState(false);
  const { checkCookie, impersonate } = useAuth();

  const getUserInfo = async () => {
    setLoading(true);
    const reqUsers = await listUsers(checkCookie, true);
    if (reqUsers) {
      setUsers(reqUsers);
    }
    setLoading(false);
  };

  const handleSyncLdap = async () => {
    setSyncing(true);
    setSyncError('');
    setSyncSuccess(false);
    const result = await syncLdap(setSyncError);
    if (result) {
      setSyncSuccess(true);
      await getUserInfo();
    }
    setSyncing(false);
  };

  useEffect(() => {
    getUserInfo();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return (
    <Page title="Users · Thorium">
      <Row className="d-flex justify-content-md-center">
        <Col xs="auto">
          <Title>Users</Title>
        </Col>
      </Row>
      <Row className="d-flex justify-content-md-center">
        <Col xs="auto">
          <ButtonToolbar className="mb-2">
            <Button className="secondary-btn" size="sm" disabled={syncing} onClick={handleSyncLdap}>
              {syncing ? 'Syncing...' : 'LDAP Sync'}
            </Button>
          </ButtonToolbar>
        </Col>
      </Row>
      {syncSuccess && (
        <Row className="d-flex justify-content-md-center">
          <Col xs={4}>
            <Alert variant="success" dismissible onClose={() => setSyncSuccess(false)}>
              <center>LDAP sync completed successfully.</center>
            </Alert>
          </Col>
        </Row>
      )}
      {syncError !== '' && (
        <Row className="d-flex justify-content-md-center">
          <Col xs={4}>
            <Alert variant="danger" dismissible onClose={() => setSyncError('')}>
              <center>{syncError}</center>
            </Alert>
          </Col>
        </Row>
      )}
      <LoadingSpinner loading={loading} />
      <Row>
        {users.length > 0 &&
          [...users]
            .sort((a, b) => a.username.localeCompare(b.username))
            .map((user) => (
              <UserCard
                key={user.username}
                username={user.username}
                role={user.role}
                email={user.email}
                groups={user.groups}
                token={user.token}
                expires={user.token_expiration}
                theme={user.settings?.theme}
                verified={user.verified}
                local={user.local}
                onDelete={getUserInfo}
                onMasquerade={impersonate}
              />
            ))}
      </Row>
    </Page>
  );
};

export default Users;
