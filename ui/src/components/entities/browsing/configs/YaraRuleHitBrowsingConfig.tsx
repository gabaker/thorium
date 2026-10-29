import React from 'react';
import { Link } from 'react-router-dom';
import { Row } from 'react-bootstrap';

// project imports
// type-only on purpose: a runtime import would evaluate the all-configs module, and this
// file is also imported by the hunt details findings section (see HuntFindingsSection)
import type { EntityBrowseConfig } from './config';
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
import { YaraRuleHit } from '@models/entities/rules/yara_hits';
import { getDetailsBasePathByEntity } from '@components/entities/details/EntityDetailsRoutes';

// spec: ../EntityBrowsing.spec.md

// shorten a sample sha256 so the column stays scannable; the details page has the full hash
const shortSha256 = (sha256: string): string => (sha256.length > 16 ? `${sha256.substring(0, 16)}...` : sha256);

/**
 * The hit list's column header row; exported for reuse by the hunt findings panel.
 */
export const YaraRuleHitListHeaders = () => {
  return (
    <BrowsingCard>
      <BrowsingContents>
        <Row>
          <EntityName>Name</EntityName>
          <EntitySecondary>Sample · Scanned</EntitySecondary>
          <EntityGroups>Group(s)</EntityGroups>
          <EntitySubmitters>Submitter(s)</EntitySubmitters>
        </Row>
      </BrowsingContents>
    </BrowsingCard>
  );
};

interface YaraRuleHitItemProps {
  hit: YaraRuleHit;
  // tag keys to hide from the row's chips (the hunt findings panel hides HuntId/submitter);
  // a plain prop on purpose — Filters.hideTags would ride into the listing query params
  excludeKeys?: string[];
}

/**
 * One hit row linking to the hit's details page; exported for reuse by the hunt
 * findings panel, which hides the chips its context already shows.
 */
export const YaraRuleHitItem: React.FC<YaraRuleHitItemProps> = ({ hit, excludeKeys }) => {
  const excluded = excludeKeys ?? [];
  const visibleTags = filterExcludedTags(hit.tags ?? {}, excluded);
  // browse rows keep the legacy "more than just submitter" gate; a caller hiding keys
  // shows chips whenever anything is left after the hide
  const showTags = excluded.length > 0 ? Object.keys(visibleTags).length > 0 : Object.keys(hit.tags ?? {}).length > 1;
  return (
    <BrowsingCard>
      <BrowsingContents>
        <Link to={`${getDetailsBasePathByEntity(Entities.YaraRuleHit)}/${hit.id}`} state={{ hit: hit }} className="no-decoration">
          <LinkFields>
            <EntityName>
              <EntityNameWithIcon entityId={hit.id} hasImage={hit.image != null}>
                {hit.name}
              </EntityNameWithIcon>
            </EntityName>
            <EntitySecondary>
              {shortSha256(hit.metadata.YaraRuleHit.sha256)}
              {hit.metadata.YaraRuleHit.scanned_at ? ` · ${new Date(hit.metadata.YaraRuleHit.scanned_at).toLocaleString()}` : ''}
            </EntitySecondary>
            <EntityGroups>
              <small>
                <i>
                  {hit.groups &&
                    (hit.groups.toString().length > 75
                      ? hit.groups.toString().replaceAll(',', ', ').substring(0, 75) + '...'
                      : hit.groups.toString().replaceAll(',', ', '))}
                </i>
              </small>
            </EntityGroups>
            <EntitySubmitters>
              {hit.submitter ? (
                <small>
                  <i>{hit.submitter.length > 75 ? hit.submitter.substring(0, 75) + '...' : hit.submitter}</i>
                </small>
              ) : null}
            </EntitySubmitters>
          </LinkFields>
        </Link>
        {showTags && (
          <>
            <hr />
            <CondensedEntityTags resource={Entities.YaraRuleHit} tags={visibleTags} />
          </>
        )}
      </BrowsingContents>
    </BrowsingCard>
  );
};

const YaraRuleHitBrowsingConfig: EntityBrowseConfig<Entities.YaraRuleHit> = {
  docTitle: 'Yara Rule Hits · Thorium',
  title: 'Yara Rule Hits',
  typeLabel: '',
  kind: Entities.YaraRuleHit,
  // hits are machine-written by the scan path and hunts, never created in the UI
  creatable: false,
  entityHeaders: <YaraRuleHitListHeaders />,
  renderEntity: (entity) => <YaraRuleHitItem hit={entity} />,
  fetchEntities: makeEntityFetcher(Entities.YaraRuleHit),
};

export default YaraRuleHitBrowsingConfig;
