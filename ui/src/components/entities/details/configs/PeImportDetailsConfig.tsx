import { JSX } from 'react';
import { Row } from 'react-bootstrap';
import { FaFileImport } from 'react-icons/fa';

// project imports
import { EntityDetailsConfig } from './configs';
import { DetailsMetadataProps } from '../EntityDetails';
import InfoHeader from '@entities/shared/InfoHeader';
import InfoValue from '@entities/shared/InfoValue';
import FieldBadge from '@components/shared/badges/FieldBadge';
import SelectInputArray from '@components/shared/inputs/selectable/SelectInputArray';
import { getEntity } from '@thorpi/entities';
import { Entities } from '@models/entities';
import { BlankPeImport, PeImport, PeImportMetaFields } from '@models/entities/pe';

const PeImportMetaInfo = ({ entity, pendingEntity, handleUpdate, editing }: DetailsMetadataProps<Entities.PeImport>): JSX.Element => {
  // apply a single metadata field change and hand the updated metadata back to the entity update
  function updatePendingMeta<T extends keyof PeImportMetaFields>(field: T, value: PeImportMetaFields[T]): void {
    const updates: PeImportMetaFields = structuredClone(pendingEntity.metadata.PeImport);
    updates[field] = value;
    handleUpdate('metadata', { PeImport: updates });
  }

  return (
    <>
      <Row className="mt-3">
        <InfoHeader>Functions</InfoHeader>
        <InfoValue>
          {editing ? (
            <>
              <SelectInputArray
                values={pendingEntity.metadata.PeImport.functions}
                onChange={(functions) => updatePendingMeta('functions', functions)}
              />
              {/* the API's PeImport update ignores an empty list, so clearing every function won't persist */}
              <small className="text-muted">Removing every function won't be saved — at least one must remain.</small>
            </>
          ) : (
            <FieldBadge color="Gray" noNull field={entity.metadata.PeImport.functions} />
          )}
        </InfoValue>
      </Row>
    </>
  );
};

const getPeImportDetails = (entityID: string, setError: (err: string) => void, updateEntity: (entity: PeImport) => void) => {
  void getEntity(entityID, setError).then((data) => {
    if (data && data.kind == Entities.PeImport) {
      updateEntity(data);
    }
  });
};

const PeImportDetailsConfig: EntityDetailsConfig<Entities.PeImport> = {
  getEntityDetails: getPeImportDetails,
  EntityMetaInfo: PeImportMetaInfo,
  BlankEntity: BlankPeImport,
  icon: (size: number) => <FaFileImport size={size} />,
};

export default PeImportDetailsConfig;
