import React from 'react';
import { Col, Form, Row } from 'react-bootstrap';
import { ThoriumRole } from '@models';
import { DeveloperOptionsRow } from './styles';

interface DeveloperOptions {
  k8s: boolean;
  bare_metal: boolean;
  windows: boolean;
  external: boolean;
}

interface RoleEditorProps {
  role: string;
  developerOptions: DeveloperOptions;
  onRoleChange: (role: string) => void;
  onDeveloperOptionChange: (key: keyof DeveloperOptions) => void;
}

const AVAILABLE_ROLES = ['Admin', 'Analyst', 'Developer', 'User'];

const RoleEditor: React.FC<RoleEditorProps> = ({ role, developerOptions, onRoleChange, onDeveloperOptionChange }) => {
  return (
    <>
      <Form.Select value={role} onChange={(e) => onRoleChange(e.target.value)} style={{ maxWidth: '200px' }}>
        {AVAILABLE_ROLES.map((r) => (
          <option key={r} value={r}>
            {r}
          </option>
        ))}
      </Form.Select>
      {role === 'Developer' && (
        <DeveloperOptionsRow>
          {(['k8s', 'bare_metal', 'windows', 'external'] as const).map((key) => (
            <Form.Check
              key={key}
              type="switch"
              id={`dev-${key}`}
              label={key === 'bare_metal' ? 'Bare Metal' : key.charAt(0).toUpperCase() + key.slice(1)}
              checked={developerOptions[key]}
              onChange={() => onDeveloperOptionChange(key)}
            />
          ))}
        </DeveloperOptionsRow>
      )}
    </>
  );
};

export function getDeveloperDefaults(role: ThoriumRole): DeveloperOptions {
  const dev = typeof role === 'object' && role !== null && 'Developer' in role ? (role as any).Developer : null;
  return {
    k8s: dev ? dev.k8s : true,
    bare_metal: dev ? dev.bare_metal : false,
    windows: dev ? dev.windows : false,
    external: dev ? dev.external : false,
  };
}

export type { DeveloperOptions };
export default RoleEditor;
