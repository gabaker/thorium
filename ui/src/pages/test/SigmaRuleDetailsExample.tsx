import React, { useState } from 'react';
import { Row, Form, Card } from 'react-bootstrap';
import { FaRegEdit, FaBackspace, FaSave } from 'react-icons/fa';
import styled from 'styled-components';

// project imports
import CodeEditor from '@components/shared/inputs/code/CodeEditor';
import NumberInput from '@components/shared/inputs/NumberInput';
import SelectInputArray from '@components/shared/selectable/SelectInputArray';
import FieldBadge from '@components/shared/badges/FieldBadge';
import InfoHeader from '@entities/shared/InfoHeader';
import InfoValue from '@entities/shared/InfoValue';
import { SigmaRuleChecker } from '@utilities/rules/sigma';

// --- Inlined Sigma types (will move to @models/entities/rules/sigma) ---

enum SigmaRuleAppliesTo {
  WindowsProcesses = 'WindowsProcesses',
  NetworkConnections = 'NetworkConnections',
}

type SigmaAutoFlag = {
  content?: string;
  reasoning: string;
};

type SigmaActionToTake = {
  Flag: SigmaAutoFlag;
};

type SigmaRuleMetaFields = {
  rule: string;
  applies_to: SigmaRuleAppliesTo[];
  score: number;
  actions: SigmaActionToTake[];
};

type SigmaRuleset = {
  id: string;
  name: string;
  metadata: { SigmaRule: SigmaRuleMetaFields };
};

// --- Constants ---

const SAMPLE_RULE = `title: Okta User Account Locked Out
id: 14701da0-4b0f-4ee6-9c95-2ffb4e73bb9a
status: test
description: Detects when a user account is locked out.
references:
    - https://developer.okta.com/docs/reference/api/system-log/
    - https://developer.okta.com/docs/reference/api/event-types/
author: Austin Songer @austinsonger
date: 2021-09-12
modified: 2022-10-09
tags:
    - attack.impact
logsource:
    product: okta
    service: okta
detection:
    selection:
        displaymessage: Max sign in attempts exceeded
    condition: selection
falsepositives:
    - Unknown
level: medium`;

const SAMPLE_ENTITY: SigmaRuleset = {
  id: 'a1b2c3d4-e5f6-7890-abcd-ef1234567890',
  name: 'Okta Account Lockout Detection',
  metadata: {
    SigmaRule: {
      rule: SAMPLE_RULE,
      applies_to: [SigmaRuleAppliesTo.WindowsProcesses],
      score: 75,
      actions: [
        { Flag: { content: 'account_lockout', reasoning: 'Indicates potential brute force or credential stuffing attack' } },
        { Flag: { reasoning: 'Correlate with VPN login failures from same source IP' } },
      ],
    },
  },
};

const sigmaChecker = new SigmaRuleChecker();

// --- Styled components ---

const PageContainer = styled.div`
  padding: 24px;
  max-width: 900px;
  margin: 0 auto;
`;

const Toolbar = styled.div`
  display: flex;
  justify-content: center;
  gap: 0.5rem;
  padding: 0.75rem 0;
`;

const IconButton = styled.button`
  background: none;
  border: none;
  color: var(--thorium-text);
  cursor: pointer;
  padding: 0.25rem 0.5rem;

  &:hover {
    color: var(--thorium-highlight-text);
  }
`;

const RulePreview = styled.pre`
  background-color: var(--thorium-secondary-panel-bg);
  border: 1px solid var(--thorium-panel-border);
  border-radius: 0.375rem;
  padding: 0.75rem;
  font-size: 0.8rem;
  color: var(--thorium-text);
  max-height: 400px;
  overflow: auto;
  white-space: pre-wrap;
  margin: 0;
`;

const ActionCard = styled(Card)`
  margin-bottom: 0.75rem;
  background-color: var(--thorium-panel-bg);
  border: 1px solid var(--thorium-panel-border);
`;

const ActionCardBody = styled(Card.Body)`
  display: flex;
  flex-direction: column;
  gap: 0.75rem;
`;

const ActionHeader = styled.div`
  display: flex;
  justify-content: space-between;
  align-items: center;
`;

const ActionLabel = styled.span`
  font-weight: 600;
  font-size: 0.85rem;
  color: var(--thorium-secondary-text);
  text-transform: uppercase;
  letter-spacing: 0.03em;
`;

const RemoveButton = styled.button`
  background: none;
  border: none;
  color: var(--thorium-secondary-text);
  font-size: 1.25rem;
  cursor: pointer;
  padding: 0 0.25rem;
  line-height: 1;

  &:hover {
    color: var(--bs-danger);
  }
`;

const AddActionButton = styled.button`
  color: var(--thorium-button-text);
  background-color: var(--thorium-empty-bg);
  border: 1px solid var(--thorium-empty-bg);
  border-radius: 4px;
  padding: 0.25rem 0.75rem;
  font-size: 0.875rem;
  cursor: pointer;

  &:hover {
    filter: brightness(80%);
  }

  &:active {
    filter: brightness(70%);
    box-shadow: 0 0 0 0.1rem var(--thorium-highlight-panel-border);
  }
`;

const ActionViewCard = styled.div`
  background-color: var(--thorium-secondary-panel-bg);
  border: 1px solid var(--thorium-panel-border);
  border-radius: 0.375rem;
  padding: 0.5rem 0.75rem;
  margin-bottom: 0.5rem;
  font-size: 0.875rem;
`;

const ActionViewLabel = styled.span`
  font-weight: 600;
  font-size: 0.75rem;
  color: var(--thorium-secondary-text);
  text-transform: uppercase;
  letter-spacing: 0.03em;
`;

const StatePreview = styled.pre`
  margin-top: 24px;
  font-size: 11px;
  opacity: 0.7;
  max-height: 400px;
  overflow: auto;
`;

// --- Actions editor (editing mode) ---

interface ActionsEditorProps {
  actions: SigmaActionToTake[];
  onChange: (actions: SigmaActionToTake[]) => void;
}

const ActionsEditor: React.FC<ActionsEditorProps> = ({ actions, onChange }) => {
  const addAction = () => {
    onChange([...actions, { Flag: { content: '', reasoning: '' } }]);
  };

  const updateAction = (index: number, flag: SigmaAutoFlag) => {
    const next = structuredClone(actions);
    next[index] = { Flag: flag };
    onChange(next);
  };

  const removeAction = (index: number) => {
    onChange(actions.filter((_, i) => i !== index));
  };

  return (
    <>
      {actions.map((action, i) => (
        <ActionCard key={i}>
          <ActionCardBody>
            <ActionHeader>
              <ActionLabel>Flag Action #{i + 1}</ActionLabel>
              <RemoveButton onClick={() => removeAction(i)} title="Remove action">
                &times;
              </RemoveButton>
            </ActionHeader>
            <Form.Group>
              <Form.Label>Content</Form.Label>
              <Form.Control
                type="text"
                placeholder="Interesting characteristic (optional)"
                value={action.Flag.content ?? ''}
                onChange={(e) => updateAction(i, { ...action.Flag, content: e.target.value || undefined })}
              />
            </Form.Group>
            <Form.Group>
              <Form.Label>
                Reasoning<sub>*</sub>
              </Form.Label>
              <Form.Control
                as="textarea"
                rows={2}
                placeholder="Reason for this flag (required)"
                value={action.Flag.reasoning}
                onChange={(e) => updateAction(i, { ...action.Flag, reasoning: e.target.value })}
              />
            </Form.Group>
          </ActionCardBody>
        </ActionCard>
      ))}
      <AddActionButton onClick={addAction}>+ Add Flag Action</AddActionButton>
    </>
  );
};

// --- Actions view (read-only mode) ---

const ActionsView: React.FC<{ actions: SigmaActionToTake[] }> = ({ actions }) => {
  if (actions.length === 0) return <span style={{ color: 'var(--thorium-secondary-text)' }}>None</span>;

  return (
    <>
      {actions.map((action, i) => (
        <ActionViewCard key={i}>
          <ActionViewLabel>Flag #{i + 1}</ActionViewLabel>
          {action.Flag.content && (
            <div>
              <strong>Content:</strong> {action.Flag.content}
            </div>
          )}
          <div>
            <strong>Reasoning:</strong> {action.Flag.reasoning}
          </div>
        </ActionViewCard>
      ))}
    </>
  );
};

// --- MetaInfo render function (follows EntityDetails MetadataComponent pattern) ---

const SigmaRuleMetaInfo = (
  entity: SigmaRuleset,
  pendingEntity: SigmaRuleset,
  handleUpdate: (field: 'metadata', value: SigmaRuleset['metadata']) => void,
  editing: boolean,
) => {
  function updatePendingMeta<T extends keyof SigmaRuleMetaFields>(field: T, value: SigmaRuleMetaFields[T]): void {
    const updates: SigmaRuleMetaFields = structuredClone(pendingEntity.metadata.SigmaRule);
    updates[field] = value;
    handleUpdate('metadata', { SigmaRule: updates });
  }

  return (
    <>
      <Row>
        <InfoHeader>Rule</InfoHeader>
        <InfoValue>
          {editing ? (
            <CodeEditor
              value={pendingEntity.metadata.SigmaRule.rule}
              onChange={(text) => updatePendingMeta('rule', text)}
              checker={sigmaChecker}
              format="yaml"
              height="400px"
            />
          ) : (
            <RulePreview>{entity.metadata.SigmaRule.rule}</RulePreview>
          )}
        </InfoValue>
      </Row>
      <hr className="my-3" />
      <Row>
        <InfoHeader>Applies To</InfoHeader>
        <InfoValue>
          {editing ? (
            <SelectInputArray
              isCreatable={false}
              options={Object.values(SigmaRuleAppliesTo)}
              values={pendingEntity.metadata.SigmaRule.applies_to}
              onChange={(selected) => updatePendingMeta('applies_to', selected as SigmaRuleAppliesTo[])}
            />
          ) : (
            <FieldBadge color="Gray" noNull field={entity.metadata.SigmaRule.applies_to} />
          )}
        </InfoValue>
      </Row>
      <hr className="my-3" />
      <Row>
        <InfoHeader>Score</InfoHeader>
        <InfoValue>
          {editing ? (
            <NumberInput
              value={pendingEntity.metadata.SigmaRule.score}
              onChange={(v) => updatePendingMeta('score', v ?? 0)}
              min={0}
              step={1}
            />
          ) : (
            <>{entity.metadata.SigmaRule.score}</>
          )}
        </InfoValue>
      </Row>
      <hr className="my-3" />
      <Row>
        <InfoHeader>Actions</InfoHeader>
        <InfoValue>
          {editing ? (
            <ActionsEditor
              actions={pendingEntity.metadata.SigmaRule.actions}
              onChange={(actions) => updatePendingMeta('actions', actions)}
            />
          ) : (
            <ActionsView actions={entity.metadata.SigmaRule.actions} />
          )}
        </InfoValue>
      </Row>
    </>
  );
};

// --- Standalone example page with edit toggle ---

const SigmaRuleDetailsExample: React.FC = () => {
  const [entity] = useState<SigmaRuleset>(SAMPLE_ENTITY);
  const [pendingEntity, setPendingEntity] = useState<SigmaRuleset>(SAMPLE_ENTITY);
  const [editing, setEditing] = useState(false);

  const handleUpdate = (field: 'metadata', value: SigmaRuleset['metadata']) => {
    setPendingEntity((prev) => ({ ...prev, [field]: value }));
  };

  const handleSave = () => {
    setEditing(false);
  };

  const handleCancel = () => {
    setPendingEntity(entity);
    setEditing(false);
  };

  return (
    <PageContainer>
      <h2>Sigma Rule Details — Example</h2>

      <Toolbar>
        {editing ? (
          <>
            <IconButton onClick={handleCancel} title="Cancel changes">
              <FaBackspace size={22} />
            </IconButton>
            <IconButton onClick={handleSave} title="Save changes">
              <FaSave size={20} />
            </IconButton>
          </>
        ) : (
          <IconButton onClick={() => setEditing(true)} title="Edit">
            <FaRegEdit size={22} />
          </IconButton>
        )}
      </Toolbar>

      <Card className="panel">
        <Card.Body>
          <Row>
            <InfoHeader>ID</InfoHeader>
            <InfoValue>{entity.id}</InfoValue>
          </Row>
          <hr className="my-3" />
          <Row>
            <InfoHeader>Name</InfoHeader>
            <InfoValue>{entity.name}</InfoValue>
          </Row>
          <hr className="my-3" />
          {SigmaRuleMetaInfo(entity, pendingEntity, handleUpdate, editing)}
        </Card.Body>
      </Card>

      <StatePreview>{JSON.stringify(pendingEntity, null, 2)}</StatePreview>
    </PageContainer>
  );
};

export default SigmaRuleDetailsExample;
