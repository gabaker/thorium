import { JSX } from 'react';
import { Badge, Row } from 'react-bootstrap';
import styled from 'styled-components';
import { FaMagnifyingGlass } from 'react-icons/fa6';

// project imports
import { EntityDetailsConfig } from './configs';
import { DetailsMetadataProps } from '../EntityDetails';
import ClearQuarantineControl, { quarantineStatusValues } from '../ClearQuarantineControl';
import QuarantineExemptControl from '../QuarantineExemptControl';
import InfoValue from '@entities/shared/InfoValue';
import FieldBadge from '@components/shared/badges/FieldBadge';
import { getEntity } from '@thorpi/entities';
import { Entities } from '@models/entities';
import { BlankYaraRule, YaraActionToTake, YaraRule, YaraRuleAppliesTo, YaraRuleMetaFields } from '@models/entities/rules/yara';
import InfoHeader from '@components/entities/shared/InfoHeader';
import CodeEditor from '@components/shared/inputs/code/CodeEditor/CodeEditor';
import { YaraRuleChecker } from '@utilities/rules/yara';
import SelectInputArray from '@components/shared/inputs/selectable/SelectInputArray';
import NumberInput from '@components/shared/inputs/NumberInput';
import { YaraActionsEditor } from '@components/entities/create/configs/YaraRuleCreateConfig';
import { FormatType } from '@utilities/rules/types';

const RulePreview = styled.pre`
  background-color: var(--thorium-secondary-panel-bg);
  border: 1px solid var(--thorium-panel-border);
  border-radius: 0.375rem;
  padding: 0.75rem;
  font-size: 0.8rem;
  color: var(--thorium-text);
  overflow: auto;
  white-space: pre-wrap;
  margin: 0;
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

const ActionsView: React.FC<{ actions: YaraActionToTake[] }> = ({ actions }) => {
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

const YaraRuleMetaInfo = ({
  entity,
  pendingEntity,
  handleUpdate,
  editing,
  refreshEntity,
  setError,
}: DetailsMetadataProps<Entities.YaraRule>): JSX.Element => {
  // handle any updates to YaraRule entity metadata
  function updatePendingMeta<T extends keyof YaraRuleMetaFields>(field: T, value: YaraRuleMetaFields[T]) {
    const updates: YaraRuleMetaFields = structuredClone(pendingEntity.metadata.YaraRule);
    updates[field] = value;
    handleUpdate('metadata', { YaraRule: updates });
  }
  const yaraChecker = new YaraRuleChecker();
  // the rule's current quarantine state: machine-written status tag values plus the
  // admin-set exemption flag (serde-defaulted on rules from older builds)
  const statusValues = quarantineStatusValues(entity);
  const exempt = entity.metadata.YaraRule.quarantine_exempt ?? false;
  return (
    <>
      <Row className="mt-3">
        <InfoHeader>Rule</InfoHeader>
        <InfoValue>
          {editing ? (
            <CodeEditor
              value={pendingEntity.metadata.YaraRule.rule}
              onChange={(text) => updatePendingMeta('rule', text)}
              checker={yaraChecker}
              format={FormatType.YARA}
              height="400px"
            />
          ) : (
            <RulePreview>{entity.metadata.YaraRule.rule}</RulePreview>
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
              options={Object.values(YaraRuleAppliesTo)}
              values={pendingEntity.metadata.YaraRule.applies_to}
              onChange={(selected) => updatePendingMeta('applies_to', selected as YaraRuleAppliesTo[])}
            />
          ) : (
            <FieldBadge color="Gray" noNull field={entity.metadata.YaraRule.applies_to} />
          )}
        </InfoValue>
      </Row>
      <hr className="my-3" />
      <Row>
        <InfoHeader>Score</InfoHeader>
        <InfoValue>
          {editing ? (
            <NumberInput
              value={pendingEntity.metadata.YaraRule.score}
              onChange={(v) => updatePendingMeta('score', v ?? 0)}
              min={0}
              step={10}
            />
          ) : (
            <>{entity.metadata.YaraRule.score}</>
          )}
        </InfoValue>
      </Row>
      <hr className="my-3" />
      <Row>
        <InfoHeader>Actions</InfoHeader>
        <InfoValue>
          {editing ? (
            <YaraActionsEditor
              actions={pendingEntity.metadata.YaraRule.actions}
              onChange={(actions) => updatePendingMeta('actions', actions)}
            />
          ) : (
            <ActionsView actions={entity.metadata.YaraRule.actions} />
          )}
        </InfoValue>
      </Row>
      <hr className="my-3" />
      {/* quarantine state is machine/admin-written, so the row is display-only in edit
          mode and the controls below never join the generic edit form */}
      <Row>
        <InfoHeader>Quarantine</InfoHeader>
        <InfoValue>
          {statusValues.map((status) => (
            <Badge bg="warning" className="me-2" key={`yara-rule-status-${status}`}>
              {status}
            </Badge>
          ))}
          {exempt && (
            <Badge bg="info" className="me-2">
              Exempt
            </Badge>
          )}
          {statusValues.length === 0 && !exempt && <span>None</span>}
          {!editing && (
            <div className="mt-2">
              {/* each control renders only when its own guard passes (admin / tag present) */}
              <QuarantineExemptControl rule={entity} setError={setError} onChanged={refreshEntity} />
              <ClearQuarantineControl rule={entity} setError={setError} onCleared={refreshEntity} />
            </div>
          )}
        </InfoValue>
      </Row>
    </>
  );
};

const getYaraRuleDetails = (entityId: string, setError: (e: string) => void, updateEntity: (c: YaraRule) => void) => {
  void getEntity(entityId, setError).then((data) => {
    if (data && data.kind === Entities.YaraRule) {
      updateEntity(data);
    }
  });
};

const YaraRuleDetailsConfig: EntityDetailsConfig<Entities.YaraRule> = {
  getEntityDetails: getYaraRuleDetails,
  EntityMetaInfo: YaraRuleMetaInfo,
  BlankEntity: BlankYaraRule,
  icon: (size: number) => <FaMagnifyingGlass size={size} />,
};

export default YaraRuleDetailsConfig;
