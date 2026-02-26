import React, { useEffect, useReducer, useState } from 'react';
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
  return value;
};

const formatStorage = (value: string, unit: string): string => {
  if (unit === 'bytes') return value;
  return `${value}${unit}`;
};

// --- Form reducer ---

type FormState = {
  reservedCpu: string;
  reservedCpuUnits: string;
  reservedMemory: string;
  reservedMemoryUnits: string;
  reservedStorage: string;
  reservedStorageUnits: string;
  fairshareCpu: string;
  fairshareCpuUnits: string;
  fairshareMemory: string;
  fairshareMemoryUnits: string;
  fairshareStorage: string;
  fairshareStorageUnits: string;
  allowUnrestrictedHostPaths: boolean;
  hostPaths: string[];
  newPath: string;
};

const initialFormState: FormState = {
  reservedCpu: '',
  reservedCpuUnits: 'mCPU',
  reservedMemory: '',
  reservedMemoryUnits: 'Gi',
  reservedStorage: '',
  reservedStorageUnits: 'Gi',
  fairshareCpu: '',
  fairshareCpuUnits: 'mCPU',
  fairshareMemory: '',
  fairshareMemoryUnits: 'Gi',
  fairshareStorage: '',
  fairshareStorageUnits: 'Gi',
  allowUnrestrictedHostPaths: false,
  hostPaths: [],
  newPath: '',
};

type FormAction =
  | { type: 'SET_FIELD'; field: keyof FormState; value: FormState[keyof FormState] }
  | { type: 'ADD_PATH' }
  | { type: 'REMOVE_PATH'; path: string }
  | { type: 'POPULATE'; settings: SystemSettingsType };

const formReducer = (state: FormState, action: FormAction): FormState => {
  switch (action.type) {
    case 'SET_FIELD':
      return { ...state, [action.field]: action.value };
    case 'ADD_PATH': {
      const trimmed = state.newPath.trim();
      if (!trimmed || state.hostPaths.includes(trimmed)) return state;
      return { ...state, hostPaths: [...state.hostPaths, trimmed], newPath: '' };
    }
    case 'REMOVE_PATH':
      return { ...state, hostPaths: state.hostPaths.filter((p) => p !== action.path) };
    case 'POPULATE': {
      const s = action.settings;
      const rm = bytesToDisplay(s.reserved_memory);
      const rs = bytesToDisplay(s.reserved_storage);
      const fm = bytesToDisplay(s.fairshare_memory);
      const fs = bytesToDisplay(s.fairshare_storage);
      return {
        ...state,
        reservedCpu: String(s.reserved_cpu),
        reservedCpuUnits: 'mCPU',
        reservedMemory: rm.value,
        reservedMemoryUnits: rm.unit,
        reservedStorage: rs.value,
        reservedStorageUnits: rs.unit,
        fairshareCpu: String(s.fairshare_cpu),
        fairshareCpuUnits: 'mCPU',
        fairshareMemory: fm.value,
        fairshareMemoryUnits: fm.unit,
        fairshareStorage: fs.value,
        fairshareStorageUnits: fs.unit,
        allowUnrestrictedHostPaths: s.allow_unrestricted_host_paths,
        hostPaths: s.host_path_whitelist ?? [],
        newPath: '',
      };
    }
  }
};

const SystemSettings: React.FC = () => {
  const [error, setError] = useState('');
  const [success, setSuccess] = useState('');
  const [saving, setSaving] = useState(false);
  const [settings, setSettings] = useState<SystemSettingsType | null>(null);
  const [form, dispatch] = useReducer(formReducer, initialFormState);

  const setField = <K extends keyof FormState>(field: K, value: FormState[K]) => dispatch({ type: 'SET_FIELD', field, value });

  const fetchSettings = async () => {
    const s = await getSystemSettings(setError);
    if (s) {
      setSettings(s);
      dispatch({ type: 'POPULATE', settings: s });
    }
  };

  useEffect(() => {
    fetchSettings();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const handleSave = async () => {
    setError('');
    setSuccess('');
    setSaving(true);

    const original = settings;
    const currentPaths = new Set(form.hostPaths);
    const originalPaths = new Set(original?.host_path_whitelist ?? []);

    const addPaths = form.hostPaths.filter((p) => !originalPaths.has(p));
    const removePaths = (original?.host_path_whitelist ?? []).filter((p: string) => !currentPaths.has(p));

    const update: SystemSettingsUpdate = {};

    update.reserved_cpu = formatCpu(form.reservedCpu, form.reservedCpuUnits);
    update.reserved_memory = formatStorage(form.reservedMemory, form.reservedMemoryUnits);
    update.reserved_storage = formatStorage(form.reservedStorage, form.reservedStorageUnits);
    update.fairshare_cpu = formatCpu(form.fairshareCpu, form.fairshareCpuUnits);
    update.fairshare_memory = formatStorage(form.fairshareMemory, form.fairshareMemoryUnits);
    update.fairshare_storage = formatStorage(form.fairshareStorage, form.fairshareStorageUnits);
    if (form.allowUnrestrictedHostPaths !== original?.allow_unrestricted_host_paths)
      update.allow_unrestricted_host_paths = form.allowUnrestrictedHostPaths;
    if (addPaths.length > 0 || removePaths.length > 0) {
      update.host_path_whitelist = { add_paths: addPaths, remove_paths: removePaths };
    }

    const result = await updateSystemSettings(update, setError);
    if (result) {
      setSettings(result);
      dispatch({ type: 'POPULATE', settings: result });
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
                      value={form.reservedCpu}
                      placeholder={form.reservedCpuUnits === 'mCPU' ? '1000' : '1'}
                      onChange={(e) => setField('reservedCpu', e.target.value.replace(/[^0-9]/g, ''))}
                    />
                  </Col>
                  <Col xs="auto">
                    <Form.Select value={form.reservedCpuUnits} onChange={(e) => setField('reservedCpuUnits', e.target.value)}>
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
                      value={form.reservedMemory}
                      placeholder={form.reservedMemoryUnits === 'Mi' ? '4000' : '1'}
                      onChange={(e) => setField('reservedMemory', e.target.value.replace(/[^0-9]/g, ''))}
                    />
                  </Col>
                  <Col xs="auto">
                    <Form.Select value={form.reservedMemoryUnits} onChange={(e) => setField('reservedMemoryUnits', e.target.value)}>
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
                      value={form.reservedStorage}
                      placeholder={form.reservedStorageUnits === 'Mi' ? '8192' : '8'}
                      onChange={(e) => setField('reservedStorage', e.target.value.replace(/[^0-9]/g, ''))}
                    />
                  </Col>
                  <Col xs="auto">
                    <Form.Select value={form.reservedStorageUnits} onChange={(e) => setField('reservedStorageUnits', e.target.value)}>
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
                      value={form.fairshareCpu}
                      placeholder={form.fairshareCpuUnits === 'mCPU' ? '1000' : '1'}
                      onChange={(e) => setField('fairshareCpu', e.target.value.replace(/[^0-9]/g, ''))}
                    />
                  </Col>
                  <Col xs="auto">
                    <Form.Select value={form.fairshareCpuUnits} onChange={(e) => setField('fairshareCpuUnits', e.target.value)}>
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
                      value={form.fairshareMemory}
                      placeholder={form.fairshareMemoryUnits === 'Mi' ? '4000' : '1'}
                      onChange={(e) => setField('fairshareMemory', e.target.value.replace(/[^0-9]/g, ''))}
                    />
                  </Col>
                  <Col xs="auto">
                    <Form.Select value={form.fairshareMemoryUnits} onChange={(e) => setField('fairshareMemoryUnits', e.target.value)}>
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
                      value={form.fairshareStorage}
                      placeholder={form.fairshareStorageUnits === 'Mi' ? '8192' : '8'}
                      onChange={(e) => setField('fairshareStorage', e.target.value.replace(/[^0-9]/g, ''))}
                    />
                  </Col>
                  <Col xs="auto">
                    <Form.Select value={form.fairshareStorageUnits} onChange={(e) => setField('fairshareStorageUnits', e.target.value)}>
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
              checked={form.allowUnrestrictedHostPaths}
              onChange={(e) => setField('allowUnrestrictedHostPaths', e.target.checked)}
              className="mb-3"
            />

            <Form.Label>Host Path Whitelist</Form.Label>
            <div className="mb-2">
              {form.hostPaths.map((path) => (
                <Badge
                  key={path}
                  pill
                  bg=""
                  className="bg-blue px-3 py-2 me-1 mb-1"
                  style={{ cursor: 'pointer' }}
                  onClick={() => dispatch({ type: 'REMOVE_PATH', path })}
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
                  value={form.newPath}
                  onChange={(e) => setField('newPath', e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter') {
                      e.preventDefault();
                      dispatch({ type: 'ADD_PATH' });
                    }
                  }}
                />
              </Col>
              <Col xs="auto">
                <Button className="secondary-btn" size="sm" onClick={() => dispatch({ type: 'ADD_PATH' })} disabled={!form.newPath.trim()}>
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
