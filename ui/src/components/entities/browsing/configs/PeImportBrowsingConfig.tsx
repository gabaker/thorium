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
import { PeImport } from '@models/entities/pe';
import { Filters } from '@models/search';

// spec: ../EntityBrowsing.spec.md

// get PE imports using filters and an optional cursor
const getPeImports = async (filters: Filters, cursor: string | null, errorHandler: (error: string) => void) => {
  const listFilters = structuredClone(filters);
  listFilters.kinds = [Entities.PeImport];
  const { entityList, entityCursor } = await listEntities(listFilters, errorHandler, true, cursor);
  return {
    entitiesList: entityList as PeImport[],
    entitiesCursor: entityCursor,
  };
};

const PeImportListHeaders = () => {
  return (
    <BrowsingCard>
      <BrowsingContents>
        <Row>
          <EntityName>Library</EntityName>
          <EntitySecondary>Functions</EntitySecondary>
          <EntityGroups>Group(s)</EntityGroups>
          <EntitySubmitters>Submitter(s)</EntitySubmitters>
        </Row>
      </BrowsingContents>
    </BrowsingCard>
  );
};

interface PeImportItemProps {
  library: PeImport;
}

const PeImportItem: React.FC<PeImportItemProps> = ({ library }) => {
  const count = library.metadata.PeImport.functions.length;
  return (
    <BrowsingCard>
      <BrowsingContents>
        <Link
          to={`${getDetailsBasePathByEntity(Entities.PeImport)}/${library.id}`}
          state={{ peImport: library }}
          className="no-decoration"
        >
          <LinkFields>
            <EntityName>
              <EntityNameWithIcon entityId={library.id} hasImage={library.image != null}>
                {library.name}
              </EntityNameWithIcon>
            </EntityName>
            <EntitySecondary>{`${count} function${count === 1 ? '' : 's'}`}</EntitySecondary>
            <EntityGroups>
              <small>
                <i>{library.groups && library.groups.join(', ')}</i>
              </small>
            </EntityGroups>
            <EntitySubmitters>{library.submitter ? <small><i>{library.submitter}</i></small> : null}</EntitySubmitters>
          </LinkFields>
        </Link>
        {library.tags && Object.keys(library.tags).length > 1 && (
          <>
            <hr />
            <CondensedEntityTags resource={Entities.PeImport} tags={library.tags} />
          </>
        )}
      </BrowsingContents>
    </BrowsingCard>
  );
};

const PeImportBrowsingConfig: EntityBrowseConfig<Entities.PeImport> = {
  docTitle: `${entityLabel(Entities.PeImport)}s · Thorium`,
  title: `${entityLabel(Entities.PeImport)}s`,
  typeLabel: entityLabel(Entities.PeImport),
  kind: Entities.PeImport,
  creatable: true,
  entityHeaders: <PeImportListHeaders />,
  renderEntity: (entity) => <PeImportItem library={entity} />,
  fetchEntities: getPeImports,
};

export default PeImportBrowsingConfig;
