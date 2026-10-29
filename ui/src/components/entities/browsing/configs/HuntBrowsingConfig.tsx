import React from 'react';
import { Link } from 'react-router-dom';
import { Badge, Row } from 'react-bootstrap';

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
import { huntStatusVariant } from '@entities/details/configs/huntDisplay';
import CondensedEntityTags from '@components/tags/condensed/CondensedEntityTags';
import { filterExcludedTags } from '@components/tags/utilities';
import { Entities } from '@models/entities/entities';
import { Hunt, HUNT_ACTIVE_TAG, HUNT_ENGINES, HUNT_ENGINE_TAG, HUNT_STATUS_TAG } from '@models/entities/hunts';
import { SUBMITTER_TAG } from '@models/tags';
import { getDetailsBasePathByEntity } from '@components/entities/details/EntityDetailsRoutes';

// spec: ../EntityBrowsing.spec.md

// intrinsic tag chips duplicating row fields stay hidden: the Engine · Status column
// shows engine and status (HuntActive derives from status) and submitter has its own
// column, so their chips carry no extra information here
const HUNT_ROW_HIDDEN_TAG_KEYS = [SUBMITTER_TAG, HUNT_STATUS_TAG, HUNT_ENGINE_TAG, HUNT_ACTIVE_TAG];

const HuntListHeaders = () => {
  return (
    <BrowsingCard>
      <BrowsingContents>
        <Row>
          <EntityName>Name</EntityName>
          <EntitySecondary>Engine · Status</EntitySecondary>
          <EntityGroups>Group(s)</EntityGroups>
          <EntitySubmitters>Submitter(s)</EntitySubmitters>
        </Row>
      </BrowsingContents>
    </BrowsingCard>
  );
};

interface HuntItemProps {
  hunt: Hunt;
}

const HuntItem: React.FC<HuntItemProps> = ({ hunt }) => {
  const meta = hunt.metadata.Hunt;
  // chips render only for tags not already shown as row fields
  const visibleTags = filterExcludedTags(hunt.tags ?? {}, HUNT_ROW_HIDDEN_TAG_KEYS);
  return (
    <BrowsingCard>
      <BrowsingContents>
        <Link to={`${getDetailsBasePathByEntity(Entities.Hunt)}/${hunt.id}`} state={{ hunt: hunt }} className="no-decoration">
          <LinkFields>
            <EntityName>
              <EntityNameWithIcon entityId={hunt.id} hasImage={hunt.image != null}>
                {hunt.name}
              </EntityNameWithIcon>
            </EntityName>
            <EntitySecondary>
              {HUNT_ENGINES[meta.engine]?.label ?? meta.engine}
              {' · '}
              <Badge bg={huntStatusVariant(meta.status)}>{meta.status}</Badge>
            </EntitySecondary>
            <EntityGroups>
              <small>
                <i>
                  {hunt.groups &&
                    (hunt.groups.toString().length > 75
                      ? hunt.groups.toString().replaceAll(',', ', ').substring(0, 75) + '...'
                      : hunt.groups.toString().replaceAll(',', ', '))}
                </i>
              </small>
            </EntityGroups>
            <EntitySubmitters>
              {hunt.submitter ? (
                <small>
                  <i>{hunt.submitter.length > 75 ? hunt.submitter.substring(0, 75) + '...' : hunt.submitter}</i>
                </small>
              ) : null}
            </EntitySubmitters>
          </LinkFields>
        </Link>
        {Object.keys(visibleTags).length > 0 && (
          <>
            <hr />
            <CondensedEntityTags resource={Entities.Hunt} tags={visibleTags} />
          </>
        )}
      </BrowsingContents>
    </BrowsingCard>
  );
};

const HuntBrowsingConfig: EntityBrowseConfig<Entities.Hunt> = {
  docTitle: 'Hunts · Thorium',
  title: 'Hunts',
  typeLabel: '',
  kind: Entities.Hunt,
  creatable: true,
  entityHeaders: <HuntListHeaders />,
  renderEntity: (entity) => <HuntItem hunt={entity} />,
  fetchEntities: makeEntityFetcher(Entities.Hunt),
};

export default HuntBrowsingConfig;
