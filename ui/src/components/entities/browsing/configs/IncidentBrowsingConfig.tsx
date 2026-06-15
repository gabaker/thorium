import React from 'react';
import { Link } from 'react-router-dom';
import { Row } from 'react-bootstrap';

// project imports
import { EntityBrowseConfig } from './config';
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
import { getDetailsBasePathByEntity } from '@components/entities/details/EntityDetailsRoutes';
import { listEntities } from '@thorpi/entities';
import { Entities, entityLabel } from '@models/entities/entities';
import { Incident } from '@models/entities/incident';
import { Filters } from '@models/search';

// spec: ../EntityBrowsing.spec.md

// get incidents using filters and an optional cursor
const getIncidents = async (filters: Filters, cursor: string | null, errorHandler: (error: string) => void) => {
  const listFilters = structuredClone(filters);
  listFilters.kinds = [Entities.Incident];
  const { entityList, entityCursor } = await listEntities(listFilters, errorHandler, true, cursor);
  return {
    entitiesList: entityList as Incident[],
    entitiesCursor: entityCursor,
  };
};

const IncidentListHeaders = () => {
  return (
    <BrowsingCard>
      <BrowsingContents>
        <Row>
          <EntityName>Name</EntityName>
          <EntitySecondary>Cover Term</EntitySecondary>
          <EntityGroups>Group(s)</EntityGroups>
          <EntitySubmitters>Submitter(s)</EntitySubmitters>
        </Row>
      </BrowsingContents>
    </BrowsingCard>
  );
};

interface IncidentItemProps {
  incident: Incident;
}

const IncidentItem: React.FC<IncidentItemProps> = ({ incident }) => {
  const meta = incident.metadata.Incident;
  return (
    <BrowsingCard>
      <BrowsingContents>
        <Link
          to={`${getDetailsBasePathByEntity(Entities.Incident)}/${incident.id}`}
          state={{ incident: incident }}
          className="no-decoration"
        >
          <LinkFields>
            <EntityName>
              <EntityNameWithIcon entityId={incident.id} hasImage={incident.image != null}>
                {incident.name}
              </EntityNameWithIcon>
            </EntityName>
            <EntitySecondary>{meta.cover_term ?? ''}</EntitySecondary>
            <EntityGroups>
              <small>
                <i>{incident.groups && incident.groups.join(', ')}</i>
              </small>
            </EntityGroups>
            <EntitySubmitters>{incident.submitter ? <small><i>{incident.submitter}</i></small> : null}</EntitySubmitters>
          </LinkFields>
        </Link>
        {incident.tags && Object.keys(incident.tags).length > 1 && (
          <>
            <hr />
            <CondensedEntityTags resource={Entities.Incident} tags={incident.tags} />
          </>
        )}
      </BrowsingContents>
    </BrowsingCard>
  );
};

const IncidentBrowsingConfig: EntityBrowseConfig<Entities.Incident> = {
  docTitle: `${entityLabel(Entities.Incident)}s · Thorium`,
  title: 'Incidents',
  typeLabel: entityLabel(Entities.Incident),
  kind: Entities.Incident,
  creatable: true,
  entityHeaders: <IncidentListHeaders />,
  renderEntity: (entity) => <IncidentItem incident={entity} />,
  fetchEntities: getIncidents,
};

export default IncidentBrowsingConfig;
