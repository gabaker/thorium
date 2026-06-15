import { JSX } from 'react';
import { Row } from 'react-bootstrap';
import styled from 'styled-components';

// project imports
import { EntityCreateConfig } from './config';
import { CreateMetadataProps } from '../EntityCreate';
import InfoHeader from '@entities/shared/InfoHeader';
import InfoValue from '@entities/shared/InfoValue';
import NumberInput from '@components/shared/inputs/NumberInput';
import { Entities } from '@models/entities/entities';
import { BlankCreateFlag, Confidence, FlagCreateMetaFields } from '@models/entities/flag';

const Select = styled.select`
  width: 100%;
  padding: 0.375rem 0.75rem;
  font-size: inherit;
  border-radius: 0.375rem;
  background-color: var(--thorium-secondary-panel-bg);
  color: var(--thorium-text);
  border: 1px solid var(--thorium-panel-border);

  &:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--thorium-highlight-text);
  }
`;

const TextInput = styled.input`
  width: 100%;
  padding: 0.375rem 0.75rem;
  font-size: inherit;
  border-radius: 0.375rem;
  background-color: var(--thorium-secondary-panel-bg);
  color: var(--thorium-text);
  border: 1px solid var(--thorium-panel-border);

  &:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--thorium-highlight-text);
  }
`;

const TextArea = styled(TextInput).attrs({ as: 'textarea' })`
  min-height: 80px;
  resize: vertical;
`;

const FlagMetaInfo = ({ entity, onChange }: CreateMetadataProps<Entities.Flag>): JSX.Element => {
  // update one metadata field then push the whole metadata object back up
  function updatePendingMeta<T extends keyof FlagCreateMetaFields>(field: T, value: FlagCreateMetaFields[T]): void {
    const updates: FlagCreateMetaFields = structuredClone(entity.metadata.Flag);
    updates[field] = value;
    onChange('metadata', { Flag: updates });
  }
  const meta = entity.metadata.Flag;
  return (
    <>
      <Row className="mt-3">
        <InfoHeader>Suspicion</InfoHeader>
        <InfoValue>
          <NumberInput value={meta.suspicion} onChange={(v) => updatePendingMeta('suspicion', v ?? 0)} />
        </InfoValue>
      </Row>
      <hr className="my-3" />
      <Row className="mt-3">
        <InfoHeader>Confidence</InfoHeader>
        <InfoValue>
          <Select value={meta.confidence} onChange={(e) => updatePendingMeta('confidence', e.target.value as Confidence)}>
            {Object.values(Confidence).map((c) => (
              <option key={c} value={c}>
                {c}
              </option>
            ))}
          </Select>
        </InfoValue>
      </Row>
      <hr className="my-3" />
      <Row className="mt-3">
        <InfoHeader>Reasoning</InfoHeader>
        <InfoValue>
          <TextArea
            value={meta.reasoning}
            onChange={(e) => updatePendingMeta('reasoning', e.target.value)}
            placeholder="Why is this flagged?"
          />
        </InfoValue>
      </Row>
      <hr className="my-3" />
      <Row className="mt-3">
        <InfoHeader>Content</InfoHeader>
        <InfoValue>
          <TextInput
            value={meta.content ?? ''}
            onChange={(e) => updatePendingMeta('content', e.target.value === '' ? null : e.target.value)}
            placeholder="The interesting/odd/suspicious characteristic (optional)"
          />
        </InfoValue>
      </Row>
    </>
  );
};

const FlagCreateConfig: EntityCreateConfig<Entities.Flag> = {
  kind: Entities.Flag,
  EntityMetadata: FlagMetaInfo,
  BlankCreateEntity: BlankCreateFlag,
};

export default FlagCreateConfig;
