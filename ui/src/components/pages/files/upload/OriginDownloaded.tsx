import React from 'react';
import { Col, Form, Row } from 'react-bootstrap';
import Subtitle from '@components/shared/titles/Subtitle';
import { OriginState } from './types';

interface OriginDownloadedProps {
  originState: OriginState;
  onOriginChange: (field: keyof OriginState, value: string) => void;
  onResetStatus: () => void;
}

const OriginDownloaded: React.FC<OriginDownloadedProps> = ({ originState, onOriginChange, onResetStatus }) => {
  return (
    <>
      <br />
      <Row>
        <Col className="name-width" xs={2}>
          <Subtitle>URL</Subtitle>
        </Col>
        <Col xs={5}>
          <Form.Control
            type="text"
            value={originState.originUrl}
            placeholder="badsite.xyz"
            onChange={(e) => {
              onOriginChange('originUrl', String(e.target.value));
              onResetStatus();
            }}
            isInvalid={!originState.originUrl && !!originState.originName}
          />
          <Form.Control.Feedback type="invalid">Please enter a site URL.</Form.Control.Feedback>
        </Col>
      </Row>
      <br />
      <Row>
        <Col className="name-width" xs={2}>
          <Subtitle>Site Name</Subtitle>
        </Col>
        <Col xs={5}>
          <Form.Control
            type="text"
            value={originState.originName}
            placeholder="optional"
            onChange={(e) => {
              onOriginChange('originName', String(e.target.value));
              onResetStatus();
            }}
          />
        </Col>
      </Row>
    </>
  );
};

export default OriginDownloaded;
