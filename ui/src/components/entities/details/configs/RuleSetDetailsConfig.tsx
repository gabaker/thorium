import { useEffect, useState, JSX } from 'react';
import { Row } from 'react-bootstrap';
import { FaBoxesStacked } from 'react-icons/fa6';

// project imports
import { DetailsMetadataProps } from '../EntityDetails';
import { EntityDetailsConfig } from './configs';
import { makeGetEntityDetails } from './factory';
import { sortMembersList } from './ruleSetRules';
import RuleSetRulesSection from './RuleSetRulesSection';
import InfoValue from '@entities/shared/InfoValue';
import InfoHeader from '@entities/shared/InfoHeader';
import { getAvailableEntities } from '@entities/utilities';
import SelectInputArray from '@components/shared/inputs/selectable/SelectInputArray';
import { Entities } from '@models/entities';
import { BlankRuleSet, RuleSetMember, RuleSetMetaFields } from '@models/entities/rulesets';

// rule kinds a rule set may hold members of (members can mix engines)
const RULE_KINDS = [Entities.SigmaRule, Entities.YaraRule];

/**
 * Map the member picker's selected ids to pending members without losing their kinds.
 *
 * The save path publishes pendingEntity straight into read mode before the corrective
 * refetch lands, so pending members must already carry their real kinds: already-known
 * ids keep their member object untouched, and newly added ids resolve their kind from
 * the same listing that built the picker options. Only a genuinely unknown id (possible
 * while the picker maps are still loading) falls back to the bare id as its name — with
 * no kind on purpose, so read mode renders it linkless via `memberDetailsUrl`
 * instead of guessing an engine.
 *
 * @param ids - The ids selected in the member picker.
 * @param knownMembers - Already-resolved members to preserve; the first id match wins.
 * @param rulesMap - The id -> name map backing the picker options.
 * @param kindsMap - The id -> kind map built from the same listing.
 * @returns The pending member list for the selected ids.
 */
export const resolvePendingMembers = (
  ids: string[],
  knownMembers: RuleSetMember[],
  rulesMap: { [key: string]: string },
  kindsMap: { [key: string]: Entities },
): RuleSetMember[] => {
  return ids.map((id) => {
    // preserve already-known member objects (real kind and hydrated name)
    const known = knownMembers.find((member) => member.id === id);
    if (known) {
      return known;
    }
    // a kind the listing never resolved is undefined at runtime despite the required
    // member type: deliberate, so the unknown member renders linkless instead of wrong
    return { id, name: rulesMap[id] ?? id, kind: kindsMap[id] };
  });
};

const RuleSetMetaInfo = ({ entity, pendingEntity, handleUpdate, editing }: DetailsMetadataProps<Entities.RuleSet>): JSX.Element => {
  const [rulesMap, setRulesMap] = useState<{ [key: string]: string }>({});
  const [ruleKindsMap, setRuleKindsMap] = useState<{ [key: string]: Entities }>({});

  // update metadata and then pass back to entity update
  function updatePendingMeta<T extends keyof RuleSetMetaFields>(field: T, value: RuleSetMetaFields[T]): void {
    const updates: RuleSetMetaFields = structuredClone(pendingEntity.metadata.RuleSet);
    updates[field] = value;
    handleUpdate('metadata', { RuleSet: updates });
  }

  // resolve the picker's selected ids into the pending member list
  function updatePendingMembers(ids: string[]): void {
    // current pending members win (they keep this edit's resolutions), then the hydrated
    // entity members cover a remove + re-add of an original member before the maps load
    const knownMembers = [...(pendingEntity.metadata.RuleSet.rules ?? []), ...(entity.metadata.RuleSet.rules ?? [])];
    updatePendingMeta('rules', resolvePendingMembers(ids, knownMembers, rulesMap, ruleKindsMap));
  }

  // get the id -> name and id -> kind maps of selectable rules when entering edit mode
  useEffect(() => {
    if (editing) {
      void getAvailableEntities(RULE_KINDS, setRulesMap).then((entityList) => {
        // the same listing carries each option's kind; keep it so members added in this
        // edit link to the right engine even before the post-save refetch lands
        const kindsMap: { [key: string]: Entities } = {};
        entityList.forEach((listed) => {
          kindsMap[listed.id] = listed.kind;
        });
        setRuleKindsMap(kindsMap);
      });
    }
  }, [editing]);

  return (
    <Row className="d-flex flex-row justify-content-center">
      <InfoHeader>Rules</InfoHeader>
      <InfoValue>
        {editing ? (
          <SelectInputArray
            // the picker maps its value chips to labels only at mount, so remount it when
            // the lazily fetched id -> name map lands (selections live in pendingEntity
            // and survive the remount)
            key={`ruleset-members-${Object.keys(rulesMap).length}`}
            isCreatable={false}
            values={
              pendingEntity.metadata.RuleSet.rules && pendingEntity.metadata.RuleSet.rules.length > 0
                ? sortMembersList(pendingEntity.metadata.RuleSet.rules).map((rule: RuleSetMember) => rule.id)
                : []
            }
            options={Object.keys(rulesMap)}
            onChange={updatePendingMembers}
            valuesMap={rulesMap}
          />
        ) : (
          // the full member list (with filtering and paging) lives in the Rules section
          // below; show just the count so the metadata row keeps its divider spacing
          <>{entity.metadata.RuleSet.rules?.length ?? 0}</>
        )}
      </InfoValue>
    </Row>
  );
};

const RuleSetDetailsConfig: EntityDetailsConfig<Entities.RuleSet> = {
  getEntityDetails: makeGetEntityDetails(Entities.RuleSet),
  EntityMetaInfo: RuleSetMetaInfo,
  AssociationsSection: RuleSetRulesSection,
  BlankEntity: BlankRuleSet,
  icon: (size: number) => <FaBoxesStacked size={size} />,
};

export default RuleSetDetailsConfig;
