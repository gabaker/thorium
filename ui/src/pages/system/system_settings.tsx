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

// Convert raw byte count to a display value + unit
const bytesToDisplay = (bytes: number): { value: string; unit: string } => {
  const gi = 1024 * 1024 * 1024;
  const mi = 1024 * 1024;
  if (bytes >= gi && bytes % gi === 0) return { value: String(bytes / gi), unit: 'Gi' };
  if (bytes >= mi && bytes % mi === 0) return { value: String(bytes / mi), unit: 'Mi' };
  if (bytes === 0) return { value: '0', unit: 'Gi' };
  return { value: String(bytes), unit: 'bytes' };
};

// Format a value + unit into the API string representation
const formatCpu = (value: string, unit: string): string => {
  if (unit === 'mCPU') return `${value}m`;
  return value; // whole CPU
};

const formatStorage = (value: string, unit: string): string => {
  if (unit === 'bytes') return value;
  return `${value}${unit}`;
};

const SystemSettings: React.FC = () => {
  const [error, setError] = useState('');
  const [success, setSuccess] = useState('');
  const [saving, setSaving] = useState(false);
  const [settings, setSettings] = useState<SystemSettingsType | null>(null);

  // form state — values are the numeric part, units track the dropdown selection
  const [reservedCpu, setReservedCpu] = useState('');
  const [reservedCpuUnits, setReservedCpuUnits] = useState('mCPU');
  const [reservedMemory, setReservedMemory] = useState('');
  const [reservedMemoryUnits, setReservedMemoryUnits] = useState('Gi');
  const [reservedStorage, setReservedStorage] = useState('');
  const [reservedStorageUnits, setReservedStorageUnits] = useState('Gi');
  const [fairshareCpu, setFairshareCpu] = useState('');
  const [fairshareCpuUnits, setFairshareCpuUnits] = useState('mCPU');
  const [fairshareMemory, setFairshareMemory] = useState('');
  const [fairshareMemoryUnits, setFairshareMemoryUnits] = useState('Gi');
  const [fairshareStorage, setFairshareStorage] = useState('');
  const [fairshareStorageUnits, setFairshareStorageUnits] = useState('Gi');
  const [allowUnrestrictedHostPaths, setAllowUnrestrictedHostPaths] = useState(false);
  const [hostPaths, setHostPaths] = useState<string[]>([]);
  const [newPath, setNewPath] = useState('');

  const populateForm = (s: SystemSettingsType) => {
    // CPU values come back as millicpu — default to mCPU unit
    setReservedCpu(String(s.reserved_cpu));
    setReservedCpuUnits('mCPU');
    setFairshareCpu(String(s.fairshare_cpu));
    setFairshareCpuUnits('mCPU');

    // Memory / storage come back as bytes — pick best display unit
    const rm = bytesToDisplay(s.reserved_memory);
    setReservedMemory(rm.value);
    setReservedMemoryUnits(rm.unit);
    const rs = bytesToDisplay(s.reserved_storage);
    setReservedStorage(rs.value);
    setReservedStorageUnits(rs.unit);
    const fm = bytesToDisplay(s.fairshare_memory);
    setFairshareMemory(fm.value);
    setFairshareMemoryUnits(fm.unit);
    const fs = bytesToDisplay(s.fairshare_storage);
    setFairshareStorage(fs.value);
    setFairshareStorageUnits(fs.unit);

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

    // Always send formatted resource values so the API parses the units
    update.reserved_cpu = formatCpu(reservedCpu, reservedCpuUnits);
    update.reserved_memory = formatStorage(reservedMemory, reservedMemoryUnits);
    update.reserved_storage = formatStorage(reservedStorage, reservedStorageUnits);
    update.fairshare_cpu = formatCpu(fairshareCpu, fairshareCpuUnits);
    update.fairshare_memory = formatStorage(fairshareMemory, fairshareMemoryUnits);
    update.fairshare_storage = formatStorage(fairshareStorage, fairshareStorageUnits);
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
                <Form.Label>CPU</Form.Label>
                <Row className="g-1">
                  <Col>
                    <Form.Control
                      type="text"
                      value={reservedCpu}
                      placeholder={reservedCpuUnits === 'mCPU' ? '1000' : '1'}
                      onChange={(e) => setReservedCpu(e.target.value.replace(/[^0-9]/g, ''))}
                    />
                  </Col>
                  <Col xs="auto">
                    <Form.Select value={reservedCpuUnits} onChange={(e) => setReservedCpuUnits(e.target.value)}>
                      <option value="CPU">CPU</option>
                      <option value="mCPU">mCPU</option>
                    </Form.Select>
                  </Col>
                </Row>
              </Col>
              <Col>
                <Form.Label>Memory</Form.Label>
                <Row className="g-1">
                  <Col>
                    <Form.Control
                      type="text"
                      value={reservedMemory}
                      placeholder={reservedMemoryUnits === 'Mi' ? '4000' : '1'}
                      onChange={(e) => setReservedMemory(e.target.value.replace(/[^0-9]/g, ''))}
                    />
                  </Col>
                  <Col xs="auto">
                    <Form.Select value={reservedMemoryUnits} onChange={(e) => setReservedMemoryUnits(e.target.value)}>
                      <option value="Gi">GiB</option>
                      <option value="Mi">MiB</option>
                    </Form.Select>
                  </Col>
                </Row>
              </Col>
              <Col>
                <Form.Label>Storage</Form.Label>
                <Row className="g-1">
                  <Col>
                    <Form.Control
                      type="text"
                      value={reservedStorage}
                      placeholder={reservedStorageUnits === 'Mi' ? '8192' : '8'}
                      onChange={(e) => setReservedStorage(e.target.value.replace(/[^0-9]/g, ''))}
                    />
                  </Col>
                  <Col xs="auto">
                    <Form.Select value={reservedStorageUnits} onChange={(e) => setReservedStorageUnits(e.target.value)}>
                      <option value="Gi">GiB</option>
                      <option value="Mi">MiB</option>
                    </Form.Select>
                  </Col>
                </Row>
              </Col>
            </Row>

            <hr className="my-3" />

            <Subtitle>Fairshare Resources</Subtitle>
            <Row className="mb-2">
              <Col>
                <Form.Label>CPU</Form.Label>
                <Row className="g-1">
                  <Col>
                    <Form.Control
                      type="text"
                      value={fairshareCpu}
                      placeholder={fairshareCpuUnits === 'mCPU' ? '1000' : '1'}
                      onChange={(e) => setFairshareCpu(e.target.value.replace(/[^0-9]/g, ''))}
                    />
                  </Col>
                  <Col xs="auto">
                    <Form.Select value={fairshareCpuUnits} onChange={(e) => setFairshareCpuUnits(e.target.value)}>
                      <option value="CPU">CPU</option>
                      <option value="mCPU">mCPU</option>
                    </Form.Select>
                  </Col>
                </Row>
              </Col>
              <Col>
                <Form.Label>Memory</Form.Label>
                <Row className="g-1">
                  <Col>
                    <Form.Control
                      type="text"
                      value={fairshareMemory}
                      placeholder={fairshareMemoryUnits === 'Mi' ? '4000' : '1'}
                      onChange={(e) => setFairshareMemory(e.target.value.replace(/[^0-9]/g, ''))}
                    />
                  </Col>
                  <Col xs="auto">
                    <Form.Select value={fairshareMemoryUnits} onChange={(e) => setFairshareMemoryUnits(e.target.value)}>
                      <option value="Gi">GiB</option>
                      <option value="Mi">MiB</option>
                    </Form.Select>
                  </Col>
                </Row>
              </Col>
              <Col>
                <Form.Label>Storage</Form.Label>
                <Row className="g-1">
                  <Col>
                    <Form.Control
                      type="text"
                      value={fairshareStorage}
                      placeholder={fairshareStorageUnits === 'Mi' ? '8192' : '8'}
                      onChange={(e) => setFairshareStorage(e.target.value.replace(/[^0-9]/g, ''))}
                    />
                  </Col>
                  <Col xs="auto">
                    <Form.Select value={fairshareStorageUnits} onChange={(e) => setFairshareStorageUnits(e.target.value)}>
                      <option value="Gi">GiB</option>
                      <option value="Mi">MiB</option>
                    </Form.Select>
                  </Col>
                </Row>
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
