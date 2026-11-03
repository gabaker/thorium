import React from 'react';
import { Link } from 'react-router';
import { Badge, Row } from 'react-bootstrap';

// project imports
import { EntityBrowseConfig } from './config';
import {
  BrowsingCard,
  BrowsingContents,
  EntityGroups,
  EntityName,
  EntitySecondary,
  EntitySubmitters,
  LinkFields,
} from '@entities/browsing/shared';
import { quarantineStatusValues } from '@entities/details/ClearQuarantineControl';
import CondensedEntityTags from '@components/tags/condensed/CondensedEntityTags';
import { filterExcludedTags } from '@components/tags/utilities';
import FieldBadge from '@components/shared/badges/FieldBadge';
import { listEntities } from '@thorpi/entities';
import { Filters } from '@models/search';
import { Entities } from '@models/entities/entities';
import { YaraRule, YARA_RULE_STATUS_TAG } from '@models/entities/rules/yara';
import { SUBMITTER_TAG } from '@models/tags';
import { getDetailsBasePathByEntity } from '../../details/EntityDetailsRoutes';

// intrinsic tag chips duplicating row fields stay hidden: submitter has its own column
// and the quarantine status renders as the row's warning badges
const YARA_RULE_ROW_HIDDEN_TAG_KEYS = [SUBMITTER_TAG, YARA_RULE_STATUS_TAG];

interface YaraRuleItemProps {
  yara: YaraRule;
}

const YaraRuleItem: React.FC<YaraRuleItemProps> = ({ yara }) => {
  // chips render only for tags not already shown as row fields
  const visibleTags = filterExcludedTags(yara.tags ?? {}, YARA_RULE_ROW_HIDDEN_TAG_KEYS);
  return (
    <BrowsingCard>
      <BrowsingContents>
        <Link to={`${getDetailsBasePathByEntity(Entities.YaraRule)}/${yara.id}`} state={{ yara: yara }} className="no-decoration">
          <LinkFields>
            <EntityName>{yara.name}</EntityName>
            <EntitySecondary>
              {yara.metadata.YaraRule.applies_to.map((target, idx) => (
                <FieldBadge key={`${target}_${idx}`} color="gray" field={`${target}`} />
              ))}
              {/* current quarantine status as warning badges, matching the frozen rule rows */}
              {quarantineStatusValues(yara).map((status) => (
                <Badge bg="warning" className="ms-2" key={`yara-rule-status-${yara.id}-${status}`}>
                  {status}
                </Badge>
              ))}
            </EntitySecondary>
            <EntityGroups>
              <small>
                <i>
                  {yara.groups &&
                    (yara.groups.toString().length > 75
                      ? yara.groups.toString().replaceAll(',', ', ').substring(0, 75) + '...'
                      : yara.groups.toString().replaceAll(',', ', '))}
                </i>
              </small>
            </EntityGroups>
            <EntitySubmitters>{yara.submitter}</EntitySubmitters>
          </LinkFields>
        </Link>
        {Object.keys(visibleTags).length > 0 && (
          <>
            <hr />
            <CondensedEntityTags resource={Entities.YaraRule} tags={visibleTags} />
          </>
        )}
      </BrowsingContents>
    </BrowsingCard>
  );
};

const YaraRuleListHeaders = () => (
  <BrowsingCard>
    <BrowsingContents>
      <Row>
        <EntityName>Name</EntityName>
        <EntitySecondary>Targets</EntitySecondary>
        <EntityGroups>Groups(s)</EntityGroups>
        <EntitySubmitters>Submitter</EntitySubmitters>
      </Row>
    </BrowsingContents>
  </BrowsingCard>
);

const getYaraRules = async (filters: Filters, cursor: string | null, errorHandler: (error: string) => void) => {
  const listFilters = structuredClone(filters);
  listFilters.kinds = [Entities.YaraRule];
  const { entityList, entityCursor } = await listEntities(listFilters, errorHandler, true, cursor);
  return { entitiesList: entityList as YaraRule[], entitiesCursor: entityCursor };
};

const YaraRulesBrowsingConfig: EntityBrowseConfig<Entities.YaraRule> = {
  docTitle: 'Yara Rules · Thorium',
  title: 'Yara Rules',
  typeLabel: '',
  kind: Entities.YaraRule,
  creatable: true,
  entityHeaders: <YaraRuleListHeaders />,
  renderEntity: (entity) => <YaraRuleItem yara={entity} />,
  fetchEntities: getYaraRules,
};

export default YaraRulesBrowsingConfig;
