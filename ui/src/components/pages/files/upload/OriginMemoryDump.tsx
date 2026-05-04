import React from 'react';
import { Col, Form, Row } from 'react-bootstrap';
import Subtitle from '@components/shared/titles/Subtitle';
import SelectableArray from '@components/shared/selectable/SelectableArray';
import { OriginState } from './types';

interface OriginMemoryDumpProps {
  originState: OriginState;
  onOriginChange: (field: keyof OriginState, value: string) => void;
  onResetStatus: () => void;
}

const OriginMemoryDump: React.FC<OriginMemoryDumpProps> = ({ originState, onOriginChange, onResetStatus }) => {
  return (
    <>
      <br />
      <Row>
        <Col className="name-width" xs={2}>
          <Subtitle>Memory Type</Subtitle>
        </Col>
        <Col xs={5}>
          <Form.Control
            type="text"
            value={originState.originMemoryType}
            placeholder="type"
            onChange={(e) => {
              onOriginChange('originMemoryType', String(e.target.value));
              onResetStatus();
            }}
            isInvalid={
              !originState.originMemoryType &&
              (!!originState.originParentFile || originState.originReconstructed.length > 0 || !!originState.originBaseAddress)
            }
          />
          <Form.Control.Feedback type="invalid">Please enter a Memory Type.</Form.Control.Feedback>
        </Col>
      </Row>
      <br />
      <Row>
        <Col className="name-width" xs={2}>
          <Subtitle>Parent</Subtitle>
        </Col>
        <Col xs={5}>
          <Form.Control
            type="text"
            placeholder="optional"
            value={originState.originParentFile}
            onChange={(e) => {
              onOriginChange('originParentFile', String(e.target.value));
              onResetStatus();
            }}
          />
        </Col>
      </Row>
      <br />
      <Row>
        <Col className="name-width" xs={2}>
          <Subtitle>Reconstructed</Subtitle>
        </Col>
        <Col xs={5}>
          <SelectableArray
            initialEntries={[]}
            setEntries={(val: string) => onOriginChange('originReconstructed', val)}
            disabled={false}
            placeholder="optional"
            trim={false}
          />
        </Col>
      </Row>
      <br />
      <Row>
        <Col className="name-width" xs={2}>
          <Subtitle>Base Address</Subtitle>
        </Col>
        <Col xs={5}>
          <Form.Control
            type="text"
            value={originState.originBaseAddress}
            placeholder="optional"
            onChange={(e) => {
              onOriginChange('originBaseAddress', String(e.target.value));
              onResetStatus();
            }}
          />
        </Col>
      </Row>
    </>
  );
};

export default OriginMemoryDump;
