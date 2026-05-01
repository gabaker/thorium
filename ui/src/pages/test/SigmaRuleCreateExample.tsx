import React, { useState } from 'react';
import { Row, Form, Card } from 'react-bootstrap';
import styled from 'styled-components';

// project imports
import CodeEditor from '@components/shared/inputs/code/CodeEditor';
import NumberInput from '@components/shared/inputs/NumberInput';
import SelectInputArray from '@components/shared/selectable/SelectInputArray';
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

type SigmaRuleset = {
  rule: string;
  applies_to: SigmaRuleAppliesTo[];
  score: number;
  actions: SigmaActionToTake[];
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

const BLANK_STATE: SigmaRuleset = {
  rule: SAMPLE_RULE,
  applies_to: [],
  score: 0,
  actions: [],
};

const sigmaChecker = new SigmaRuleChecker();

// --- Styled components ---

const PageContainer = styled.div`
  padding: 24px;
  max-width: 900px;
  margin: 0 auto;
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

const StatePreview = styled.pre`
  margin-top: 24px;
  font-size: 11px;
  opacity: 0.7;
  max-height: 400px;
  overflow: auto;
`;

// --- Actions subcomponent ---

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

// --- Main component ---

const SigmaRuleCreateExample: React.FC = () => {
  const [state, setState] = useState<SigmaRuleset>(BLANK_STATE);

  const updateField = <T extends keyof SigmaRuleset>(field: T, value: SigmaRuleset[T]) => {
    setState((prev) => ({ ...prev, [field]: value }));
  };

  return (
    <PageContainer>
      <h2>Sigma Rule Create — Example</h2>

      <Row className="mt-4">
        <InfoHeader>Rule</InfoHeader>
        <InfoValue>
          <CodeEditor
            value={state.rule}
            onChange={(text) => updateField('rule', text)}
            checker={sigmaChecker}
            format="yaml"
            height="400px"
          />
        </InfoValue>
      </Row>

      <hr className="my-3" />

      <Row>
        <InfoHeader>Applies To</InfoHeader>
        <InfoValue>
          <SelectInputArray
            isCreatable={false}
            options={Object.values(SigmaRuleAppliesTo)}
            values={state.applies_to}
            onChange={(selected) => updateField('applies_to', selected as SigmaRuleAppliesTo[])}
          />
        </InfoValue>
      </Row>

      <hr className="my-3" />

      <Row>
        <InfoHeader>Score</InfoHeader>
        <InfoValue>
          <NumberInput value={state.score} onChange={(v) => updateField('score', v ?? 0)} min={0} step={1} />
        </InfoValue>
      </Row>

      <hr className="my-3" />

      <Row>
        <InfoHeader>Actions</InfoHeader>
        <InfoValue>
          <ActionsEditor actions={state.actions} onChange={(actions) => updateField('actions', actions)} />
        </InfoValue>
      </Row>

      <StatePreview>{JSON.stringify(state, null, 2)}</StatePreview>
    </PageContainer>
  );
};

export default SigmaRuleCreateExample;
