import React from 'react';
import { Col, Form, Row } from 'react-bootstrap';
import Subtitle from '@components/shared/titles/Subtitle';
import { OriginState } from './types';

interface OriginWireProps {
  originState: OriginState;
  onOriginChange: (field: keyof OriginState, value: string) => void;
  onResetStatus: () => void;
}

const OriginWire: React.FC<OriginWireProps> = ({ originState, onOriginChange, onResetStatus }) => {
  return (
    <>
      <br />
      <Row>
        <Col className="name-width" xs={2}>
          <Subtitle>Sniffer</Subtitle>
        </Col>
        <Col xs={5}>
          <Form.Control
            type="text"
            placeholder="name"
            value={originState.originSniffer}
            onChange={(e) => {
              onOriginChange('originSniffer', String(e.target.value));
              onResetStatus();
            }}
            isInvalid={!originState.originSniffer && (!!originState.originSource || !!originState.originDestination)}
          />
          <Form.Control.Feedback type="invalid">Please enter a name for the Sniffer.</Form.Control.Feedback>
        </Col>
      </Row>
      <br />
      <Row>
        <Col className="name-width" xs={2}>
          <Subtitle>Source</Subtitle>
        </Col>
        <Col xs={5}>
          <Form.Control
            type="text"
            placeholder="optional"
            value={originState.originSource}
            onChange={(e) => {
              onOriginChange('originSource', String(e.target.value));
              onResetStatus();
            }}
          />
        </Col>
      </Row>
      <br />
      <Row>
        <Col className="name-width" xs={2}>
          <Subtitle>Destination</Subtitle>
        </Col>
        <Col xs={5}>
          <Form.Control
            type="text"
            placeholder="optional"
            value={originState.originDestination}
            onChange={(e) => {
              onOriginChange('originDestination', String(e.target.value));
              onResetStatus();
            }}
          />
        </Col>
      </Row>
    </>
  );
};

export default OriginWire;
