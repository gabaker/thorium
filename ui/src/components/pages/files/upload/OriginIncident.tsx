import React from 'react';
import { Col, Form, Row } from 'react-bootstrap';
import Subtitle from '@components/shared/titles/Subtitle';
import { OriginState } from './types';

interface OriginIncidentProps {
  originState: OriginState;
  onOriginChange: (field: keyof OriginState, value: string) => void;
  onResetStatus: () => void;
}

const OriginIncident: React.FC<OriginIncidentProps> = ({ originState, onOriginChange, onResetStatus }) => {
  return (
    <>
      <br />
      <Row>
        <Col className="name-width" xs={2}>
          <Subtitle>Incident ID</Subtitle>
        </Col>
        <Col xs={5}>
          <Form.Control
            type="text"
            value={originState.originIncident}
            placeholder="name"
            onChange={(e) => {
              onOriginChange('originIncident', String(e.target.value));
              onResetStatus();
            }}
            isInvalid={
              !originState.originIncident &&
              (!!originState.originCoverTerm ||
                !!originState.originMissionTeam ||
                !!originState.originNetwork ||
                !!originState.originMachine ||
                !!originState.originLocation)
            }
          />
          <Form.Control.Feedback type="invalid">Please enter an Incident ID.</Form.Control.Feedback>
        </Col>
      </Row>
      <br />
      <Row>
        <Col className="name-width" xs={2}>
          <Subtitle>Cover Term</Subtitle>
        </Col>
        <Col xs={5}>
          <Form.Control
            type="text"
            value={originState.originCoverTerm}
            placeholder="optional"
            onChange={(e) => {
              onOriginChange('originCoverTerm', String(e.target.value));
              onResetStatus();
            }}
          />
        </Col>
      </Row>
      <br />
      <Row>
        <Col className="name-width" xs={2}>
          <Subtitle>Mission Team</Subtitle>
        </Col>
        <Col xs={5}>
          <Form.Control
            type="text"
            value={originState.originMissionTeam}
            placeholder="optional"
            onChange={(e) => {
              onOriginChange('originMissionTeam', String(e.target.value));
              onResetStatus();
            }}
          />
        </Col>
      </Row>
      <br />
      <Row>
        <Col className="name-width" xs={2}>
          <Subtitle>Network</Subtitle>
        </Col>
        <Col xs={5}>
          <Form.Control
            type="text"
            placeholder="optional"
            value={originState.originNetwork}
            onChange={(e) => {
              onOriginChange('originNetwork', String(e.target.value));
              onResetStatus();
            }}
          />
        </Col>
      </Row>
      <br />
      <Row>
        <Col className="name-width" xs={2}>
          <Subtitle>Machine</Subtitle>
        </Col>
        <Col xs={5}>
          <Form.Control
            type="text"
            placeholder="optional"
            value={originState.originMachine}
            onChange={(e) => {
              onOriginChange('originMachine', String(e.target.value));
              onResetStatus();
            }}
          />
        </Col>
      </Row>
      <br />
      <Row>
        <Col className="name-width" xs={2}>
          <Subtitle>Location</Subtitle>
        </Col>
        <Col xs={5}>
          <Form.Control
            type="text"
            placeholder="optional"
            value={originState.originLocation}
            onChange={(e) => {
              onOriginChange('originLocation', String(e.target.value));
              onResetStatus();
            }}
          />
        </Col>
      </Row>
    </>
  );
};

export default OriginIncident;
