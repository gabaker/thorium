import React, { useEffect, useState } from 'react';
import { Alert, Badge, Button, Col, Form, Row } from 'react-bootstrap';
import styled from 'styled-components';

// project imports
import { Subtitle, Title, Page } from '@components';
import { getSystemSettings, updateSystemSettings } from '@thorpi';
import { SystemSettings as SystemSettingsType, SystemSettingsUpdate } from '@models';

const SettingsContainer = styled.div`
  display: flex;
  align-items: center;
  flex-direction: column;
  padding-top: 10px;
`;

const SettingsPanel = styled.div`
  width: 50rem;
  padding: 1rem;

  @media (max-width: 576px) {
    width: 100%;
  }
`;

const SystemSettings: React.FC = () => {
  const [error, setError] = useState('');
  const [success, setSuccess] = useState('');
  const [saving, setSaving] = useState(false);
  const [settings, setSettings] = useState<SystemSettingsType | null>(null);

  // form state
  const [reservedCpu, setReservedCpu] = useState('');
  const [reservedMemory, setReservedMemory] = useState('');
  const [reservedStorage, setReservedStorage] = useState('');
  const [fairshareCpu, setFairshareCpu] = useState('');
  const [fairshareMemory, setFairshareMemory] = useState('');
  const [fairshareStorage, setFairshareStorage] = useState('');
  const [allowUnrestrictedHostPaths, setAllowUnrestrictedHostPaths] = useState(false);
  const [hostPaths, setHostPaths] = useState<string[]>([]);
  const [newPath, setNewPath] = useState('');

  const populateForm = (s: SystemSettingsType) => {
    setReservedCpu(String(s.reserved_cpu));
    setReservedMemory(String(s.reserved_memory));
    setReservedStorage(String(s.reserved_storage));
    setFairshareCpu(String(s.fairshare_cpu));
    setFairshareMemory(String(s.fairshare_memory));
    setFairshareStorage(String(s.fairshare_storage));
    setAllowUnrestrictedHostPaths(s.allow_unrestricted_host_paths);
    setHostPaths(s.host_path_whitelist ?? []);
  };

  const fetchSettings = async () => {
    const s = await getSystemSettings(setError);
    if (s) {
      setSettings(s);
      populateForm(s);
    }
  };

  useEffect(() => {
    fetchSettings();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const handleAddPath = () => {
    const trimmed = newPath.trim();
    if (trimmed && !hostPaths.includes(trimmed)) {
      setHostPaths([...hostPaths, trimmed]);
      setNewPath('');
    }
  };

  const handleRemovePath = (path: string) => {
    setHostPaths(hostPaths.filter((p) => p !== path));
  };

  const handleSave = async () => {
    setError('');
    setSuccess('');
    setSaving(true);

    const original = settings;
    const currentPaths = new Set(hostPaths);
    const originalPaths = new Set(original?.host_path_whitelist ?? []);

    const addPaths = hostPaths.filter((p) => !originalPaths.has(p));
    const removePaths = (original?.host_path_whitelist ?? []).filter((p: string) => !currentPaths.has(p));

    const update: SystemSettingsUpdate = {};

    if (reservedCpu !== String(original?.reserved_cpu ?? '')) update.reserved_cpu = reservedCpu;
    if (reservedMemory !== String(original?.reserved_memory ?? '')) update.reserved_memory = reservedMemory;
    if (reservedStorage !== String(original?.reserved_storage ?? '')) update.reserved_storage = reservedStorage;
    if (fairshareCpu !== String(original?.fairshare_cpu ?? '')) update.fairshare_cpu = fairshareCpu;
    if (fairshareMemory !== String(original?.fairshare_memory ?? '')) update.fairshare_memory = fairshareMemory;
    if (fairshareStorage !== String(original?.fairshare_storage ?? '')) update.fairshare_storage = fairshareStorage;
    if (allowUnrestrictedHostPaths !== original?.allow_unrestricted_host_paths)
      update.allow_unrestricted_host_paths = allowUnrestrictedHostPaths;
    if (addPaths.length > 0 || removePaths.length > 0) {
      update.host_path_whitelist = { add_paths: addPaths, remove_paths: removePaths };
    }

    const result = await updateSystemSettings(update, setError);
    if (result) {
      setSettings(result);
      populateForm(result);
      setSuccess('Settings saved successfully.');
    }
    setSaving(false);
  };

  return (
    <Page title="Settings · Thorium" className="settings">
      <SettingsContainer>
        <Title>System Settings</Title>

        {error !== '' && (
          <Row className="d-flex justify-content-md-center">
            <Col xs={6}>
              <Alert variant="danger" dismissible onClose={() => setError('')}>
                <center>{error}</center>
              </Alert>
            </Col>
          </Row>
        )}
        {success !== '' && (
          <Row className="d-flex justify-content-md-center">
            <Col xs={6}>
              <Alert variant="success" dismissible onClose={() => setSuccess('')}>
                <center>{success}</center>
              </Alert>
            </Col>
          </Row>
        )}

        {settings && (
          <SettingsPanel>
            <Subtitle>Reserved Resources</Subtitle>
            <Row className="mb-2">
              <Col>
                <Form.Label>CPU (millicpu)</Form.Label>
                <Form.Control type="text" value={reservedCpu} onChange={(e) => setReservedCpu(e.target.value)} />
              </Col>
              <Col>
                <Form.Label>Memory (bytes)</Form.Label>
                <Form.Control type="text" value={reservedMemory} onChange={(e) => setReservedMemory(e.target.value)} />
              </Col>
              <Col>
                <Form.Label>Storage (bytes)</Form.Label>
                <Form.Control type="text" value={reservedStorage} onChange={(e) => setReservedStorage(e.target.value)} />
              </Col>
            </Row>

            <hr className="my-3" />

            <Subtitle>Fairshare Resources</Subtitle>
            <Row className="mb-2">
              <Col>
                <Form.Label>CPU (millicpu)</Form.Label>
                <Form.Control type="text" value={fairshareCpu} onChange={(e) => setFairshareCpu(e.target.value)} />
              </Col>
              <Col>
                <Form.Label>Memory (bytes)</Form.Label>
                <Form.Control type="text" value={fairshareMemory} onChange={(e) => setFairshareMemory(e.target.value)} />
              </Col>
              <Col>
                <Form.Label>Storage (bytes)</Form.Label>
                <Form.Control type="text" value={fairshareStorage} onChange={(e) => setFairshareStorage(e.target.value)} />
              </Col>
            </Row>

            <hr className="my-3" />

            <Subtitle>Host Path Settings</Subtitle>
            <Form.Check
              type="switch"
              id="allow-unrestricted-host-paths"
              label="Allow unrestricted host paths"
              checked={allowUnrestrictedHostPaths}
              onChange={(e) => setAllowUnrestrictedHostPaths(e.target.checked)}
              className="mb-3"
            />

            <Form.Label>Host Path Whitelist</Form.Label>
            <div className="mb-2">
              {hostPaths.map((path) => (
                <Badge
                  key={path}
                  pill
                  bg=""
                  className="bg-blue px-3 py-2 me-1 mb-1"
                  style={{ cursor: 'pointer' }}
                  onClick={() => handleRemovePath(path)}
                >
                  {path} &times;
                </Badge>
              ))}
            </div>
            <Row className="mb-2">
              <Col>
                <Form.Control
                  type="text"
                  placeholder="Add a host path..."
                  value={newPath}
                  onChange={(e) => setNewPath(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter') {
                      e.preventDefault();
                      handleAddPath();
                    }
                  }}
                />
              </Col>
              <Col xs="auto">
                <Button className="secondary-btn" size="sm" onClick={handleAddPath} disabled={!newPath.trim()}>
                  Add
                </Button>
              </Col>
            </Row>

            <hr className="my-3" />

            <Row className="d-flex justify-content-md-center">
              <Col xs="auto">
                <Button className="secondary-btn" onClick={handleSave} disabled={saving}>
                  {saving ? 'Saving...' : 'Save'}
                </Button>
              </Col>
            </Row>
          </SettingsPanel>
        )}
      </SettingsContainer>
    </Page>
  );
};

export default SystemSettings;
