import React from 'react';
import { Link } from 'react-router-dom';
import { Row } from 'react-bootstrap';

// project imports
import { EntityBrowseConfig } from './config';
import { makeEntityFetcher } from './entityFetcher';
import {
  BrowsingCard,
  BrowsingContents,
  EntityGroups,
  EntityName,
  EntityNameWithIcon,
  EntitySecondary,
  EntitySubmitters,
  LinkFields,
} from '@entities/browsing/shared';
import CondensedEntityTags from '@components/tags/condensed/CondensedEntityTags';
import { filterExcludedTags } from '@components/tags/utilities';
import { Entities } from '@models/entities/entities';
import { RuleSetEntity, RuleSetMember } from '@models/entities/rulesets';
import { SUBMITTER_TAG } from '@models/tags';
import { getDetailsBasePathByEntity } from '@components/entities/details/EntityDetailsRoutes';

// spec: ../EntityBrowsing.spec.md

// the submitter chip duplicates the row's submitter column (rulesets carry no other
// intrinsic tags), so it stays hidden
const RULESET_ROW_HIDDEN_TAG_KEYS = [SUBMITTER_TAG];

// member names joined for the secondary column, truncated like the groups column
const memberNames = (rules: RuleSetMember[] | undefined): string => {
  if (!rules || rules.length === 0) {
    return '';
  }
  const joined = rules.map((rule) => rule.name).join(', ');
  return joined.length > 75 ? joined.substring(0, 75) + '...' : joined;
};

const RuleSetListHeaders = () => {
  return (
    <BrowsingCard>
      <BrowsingContents>
        <Row>
          <EntityName>Name</EntityName>
          <EntitySecondary>Rules</EntitySecondary>
          <EntityGroups>Group(s)</EntityGroups>
          <EntitySubmitters>Submitter(s)</EntitySubmitters>
        </Row>
      </BrowsingContents>
    </BrowsingCard>
  );
};

interface RuleSetItemProps {
  ruleSet: RuleSetEntity;
}

const RuleSetItem: React.FC<RuleSetItemProps> = ({ ruleSet }) => {
  // chips render only for tags not already shown as row fields
  const visibleTags = filterExcludedTags(ruleSet.tags ?? {}, RULESET_ROW_HIDDEN_TAG_KEYS);
  return (
    <BrowsingCard>
      <BrowsingContents>
        <Link to={`${getDetailsBasePathByEntity(Entities.RuleSet)}/${ruleSet.id}`} state={{ ruleSet: ruleSet }} className="no-decoration">
          <LinkFields>
            <EntityName>
              <EntityNameWithIcon entityId={ruleSet.id} hasImage={ruleSet.image != null}>
                {ruleSet.name}
              </EntityNameWithIcon>
            </EntityName>
            <EntitySecondary>{memberNames(ruleSet.metadata.RuleSet.rules)}</EntitySecondary>
            <EntityGroups>
              <small>
                <i>
                  {ruleSet.groups &&
                    (ruleSet.groups.toString().length > 75
                      ? ruleSet.groups.toString().replaceAll(',', ', ').substring(0, 75) + '...'
                      : ruleSet.groups.toString().replaceAll(',', ', '))}
                </i>
              </small>
            </EntityGroups>
            <EntitySubmitters>
              {ruleSet.submitter ? (
                <small>
                  <i>{ruleSet.submitter.length > 75 ? ruleSet.submitter.substring(0, 75) + '...' : ruleSet.submitter}</i>
                </small>
              ) : null}
            </EntitySubmitters>
          </LinkFields>
        </Link>
        {Object.keys(visibleTags).length > 0 && (
          <>
            <hr />
            <CondensedEntityTags resource={Entities.RuleSet} tags={visibleTags} />
          </>
        )}
      </BrowsingContents>
    </BrowsingCard>
  );
};

const RuleSetBrowsingConfig: EntityBrowseConfig<Entities.RuleSet> = {
  docTitle: 'Rule Sets · Thorium',
  title: 'Rule Sets',
  typeLabel: '',
  kind: Entities.RuleSet,
  creatable: true,
  entityHeaders: <RuleSetListHeaders />,
  renderEntity: (entity) => <RuleSetItem ruleSet={entity} />,
  fetchEntities: makeEntityFetcher(Entities.RuleSet),
};

export default RuleSetBrowsingConfig;
