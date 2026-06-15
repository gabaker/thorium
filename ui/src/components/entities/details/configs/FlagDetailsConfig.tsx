import { JSX } from 'react';
import { Row } from 'react-bootstrap';
import { FaFlag } from 'react-icons/fa6';

// project imports
import { EntityDetailsConfig } from './configs';
import { DetailsMetadataProps } from '../EntityDetails';
import InfoHeader from '@entities/shared/InfoHeader';
import InfoValue from '@entities/shared/InfoValue';
import { getEntity } from '@thorpi/entities';
import { Entities } from '@models/entities';
import { BlankFlag, Flag } from '@models/entities/flag';

// Read-only display of a flag's metadata fields.
const FlagMetaInfo = ({ entity }: DetailsMetadataProps<Entities.Flag>): JSX.Element => {
  const meta = entity.metadata.Flag;
  return (
    <>
      <Row className="mt-3">
        <InfoHeader>Suspicion</InfoHeader>
        <InfoValue>{meta.suspicion}</InfoValue>
      </Row>
      <hr className="my-3" />
      <Row className="mt-3">
        <InfoHeader>Confidence</InfoHeader>
        <InfoValue>{meta.confidence}</InfoValue>
      </Row>
      <hr className="my-3" />
      <Row className="mt-3">
        <InfoHeader>Reasoning</InfoHeader>
        <InfoValue>{meta.reasoning}</InfoValue>
      </Row>
      {meta.content && (
        <>
          <hr className="my-3" />
          <Row className="mt-3">
            <InfoHeader>Content</InfoHeader>
            <InfoValue>{meta.content}</InfoValue>
          </Row>
        </>
      )}
    </>
  );
};

// Get Flag entity details from the API
const getFlagDetails = (entityID: string, setError: (err: string) => void, updateEntity: (entity: Flag) => void) => {
  void getEntity(entityID, setError).then((data) => {
    if (data && data.kind == Entities.Flag) {
      updateEntity(data);
    }
  });
};

const FlagDetailsConfig: EntityDetailsConfig<Entities.Flag> = {
  getEntityDetails: getFlagDetails,
  EntityMetaInfo: FlagMetaInfo,
  BlankEntity: BlankFlag,
  icon: (size: number) => <FaFlag size={size} />,
};

export default FlagDetailsConfig;
