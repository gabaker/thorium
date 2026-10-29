import { JSX, useEffect, useState } from 'react';
import { Row } from 'react-bootstrap';

// project imports
import { EntityCreateConfig } from './config';
import { CreateMetadataProps } from '../EntityCreate';
import InfoHeader from '@entities/shared/InfoHeader';
import InfoValue from '@entities/shared/InfoValue';
import { getAvailableEntities } from '@entities/utilities';
import SelectInputArray from '@components/shared/inputs/selectable/SelectInputArray';
import { Entities } from '@models/entities/entities';
import { BlankCreateRuleSet, RuleSetCreateMetaFields } from '@models/entities/rulesets';

// rule kinds a rule set may hold members of (members can mix engines)
const RULE_KINDS = [Entities.SigmaRule, Entities.YaraRule];

const RuleSetMetaInfo = ({ entity, onChange }: CreateMetadataProps<Entities.RuleSet>): JSX.Element => {
  const [rulesMap, setRulesMap] = useState<{ [key: string]: string }>({});

  useEffect(() => {
    void getAvailableEntities(RULE_KINDS, setRulesMap);
  }, []);

  // update metadata and then pass back to entity update
  function updatePendingMeta<T extends keyof RuleSetCreateMetaFields>(field: T, value: RuleSetCreateMetaFields[T]): void {
    const updates: RuleSetCreateMetaFields = structuredClone(entity.metadata.RuleSet);
    updates[field] = value;
    onChange('metadata', { RuleSet: updates });
  }

  return (
    <Row className="d-flex flex-row justify-content-center">
      <InfoHeader>Rules</InfoHeader>
      <InfoValue>
        <SelectInputArray
          isCreatable={false}
          values={entity.metadata.RuleSet.rules && entity.metadata.RuleSet.rules.length > 0 ? entity.metadata.RuleSet.rules : []}
          options={Object.keys(rulesMap)}
          onChange={(ids) => updatePendingMeta('rules', ids)}
          valuesMap={rulesMap}
        />
      </InfoValue>
    </Row>
  );
};

const RuleSetCreateConfig: EntityCreateConfig<Entities.RuleSet> = {
  kind: Entities.RuleSet,
  EntityMetadata: RuleSetMetaInfo,
  BlankCreateEntity: BlankCreateRuleSet,
};

export default RuleSetCreateConfig;
