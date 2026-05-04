import React from 'react';
import { Col, Form, Row } from 'react-bootstrap';
import Subtitle from '@components/shared/titles/Subtitle';
import { OriginState } from './types';

interface OriginUnpackedProps {
  originState: OriginState;
  onOriginChange: (field: keyof OriginState, value: string) => void;
  onResetStatus: () => void;
}

const OriginUnpacked: React.FC<OriginUnpackedProps> = ({ originState, onOriginChange, onResetStatus }) => {
  return (
    <>
      <br />
      <Row>
        <Col className="name-width" xs={2}>
          <Subtitle>Parent</Subtitle>
        </Col>
        <Col xs={5}>
          <Form.Control
            type="text"
            placeholder="SHA256"
            value={originState.originParentFile}
            onChange={(e) => {
              onOriginChange('originParentFile', String(e.target.value));
              onResetStatus();
            }}
            isInvalid={!originState.originParentFile && (!!originState.originTool || !!originState.originToolFlags)}
          />
          <Form.Control.Feedback type="invalid">Please enter a SHA256 value for the Parent.</Form.Control.Feedback>
        </Col>
      </Row>
      <br />
      <Row>
        <Col className="name-width" xs={2}>
          <Subtitle>Tool</Subtitle>
        </Col>
        <Col xs={5}>
          <Form.Control
            type="text"
            placeholder="optional"
            value={originState.originTool}
            onChange={(e) => {
              onOriginChange('originTool', String(e.target.value));
              onResetStatus();
            }}
          />
        </Col>
      </Row>
      <br />
      <Row>
        <Col className="name-width" xs={2}>
          <Subtitle>Flags</Subtitle>
        </Col>
        <Col xs={5}>
          <Form.Control
            type="text"
            value={originState.originToolFlags}
            placeholder="optional"
            onChange={(e) => {
              onOriginChange('originToolFlags', String(e.target.value));
              onResetStatus();
            }}
          />
        </Col>
      </Row>
    </>
  );
};

export default OriginUnpacked;
