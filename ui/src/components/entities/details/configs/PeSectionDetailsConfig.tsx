import { JSX } from 'react';
import { Row } from 'react-bootstrap';

// project imports
import { EntityDetailsConfig } from './configs';
import { DetailsMetadataProps } from '../EntityDetails';
import { CreateOnlyNote } from './shared';
import EntityTypeIcon from '@entities/shared/EntityTypeIcon';
import InfoHeader from '@entities/shared/InfoHeader';
import InfoValue from '@entities/shared/InfoValue';
import NumberInput from '@components/shared/inputs/NumberInput';
import TextInput from '@components/shared/inputs/TextInput';
import { getEntity } from '@thorpi/entities';
import { Entities } from '@models/entities';
import { BlankPeSection, PeSection, PeSectionMetaFields } from '@models/entities/pe';

// spec: ../EntityDetails.spec.md

const PeSectionMetaInfo = ({ entity, pendingEntity, handleUpdate, editing }: DetailsMetadataProps<Entities.PeSection>): JSX.Element => {
  // apply a single metadata field change and hand the updated metadata back to the entity update
  function updatePendingMeta<T extends keyof PeSectionMetaFields>(field: T, value: PeSectionMetaFields[T]): void {
    const updates: PeSectionMetaFields = structuredClone(pendingEntity.metadata.PeSection);
    updates[field] = value;
    handleUpdate('metadata', { PeSection: updates });
  }
  const meta = entity.metadata.PeSection;
  const pending = pendingEntity.metadata.PeSection;

  return (
    <>
      <Row className="mt-3">
        <InfoHeader>MD5</InfoHeader>
        <InfoValue>
          {editing ? (
            <>
              <TextInput
                type="text"
                value={pending.md5 ?? ''}
                onChange={(e) => updatePendingMeta('md5', e.target.value === '' ? undefined : e.target.value)}
              />
              {/* the API's PeSection update only sets present values, so clearing a field back to empty won't persist */}
              <CreateOnlyNote>Clearing a set value won't be saved.</CreateOnlyNote>
            </>
          ) : (
            (meta.md5 ?? '')
          )}
        </InfoValue>
      </Row>
      <hr className="my-3" />
      <Row>
        <InfoHeader>Raw Size</InfoHeader>
        <InfoValue>
          {editing ? (
            // optional field: undefined ↔ null and required=false so an empty box stays unset, not 0
            <NumberInput value={pending.raw_size ?? null} onChange={(v) => updatePendingMeta('raw_size', v ?? undefined)} min={0} required={false} />
          ) : (
            (meta.raw_size ?? '')
          )}
        </InfoValue>
      </Row>
      <hr className="my-3" />
      <Row>
        <InfoHeader>Virtual Size</InfoHeader>
        <InfoValue>
          {editing ? (
            <NumberInput
              value={pending.virtual_size ?? null}
              onChange={(v) => updatePendingMeta('virtual_size', v ?? undefined)}
              min={0}
              required={false}
            />
          ) : (
            (meta.virtual_size ?? '')
          )}
        </InfoValue>
      </Row>
      <hr className="my-3" />
      <Row>
        <InfoHeader>Entropy</InfoHeader>
        <InfoValue>
          {editing ? (
            <>
              <NumberInput
                value={pending.entropy ?? null}
                onChange={(v) => updatePendingMeta('entropy', v ?? undefined)}
                min={0}
                step={0.01}
                required={false}
              />
              {/* raw size, virtual size, and entropy are only updated when set; emptying them won't persist */}
              <CreateOnlyNote>Clearing a set value won't be saved.</CreateOnlyNote>
            </>
          ) : (
            (meta.entropy ?? '')
          )}
        </InfoValue>
      </Row>
    </>
  );
};

const getPeSectionDetails = (entityID: string, setError: (err: string) => void, updateEntity: (entity: PeSection) => void) => {
  void getEntity(entityID, setError).then((data) => {
    if (data && data.kind == Entities.PeSection) {
      updateEntity(data);
    }
  });
};

const PeSectionDetailsConfig: EntityDetailsConfig<Entities.PeSection> = {
  getEntityDetails: getPeSectionDetails,
  EntityMetaInfo: PeSectionMetaInfo,
  BlankEntity: BlankPeSection,
  icon: (size: number) => <EntityTypeIcon kind={Entities.PeSection} size={size} />,
};

export default PeSectionDetailsConfig;
