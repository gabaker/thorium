import React from 'react';
import { Card, Col, Form, Row, Tab, Tabs } from 'react-bootstrap';
import { isIP } from 'is-ip';
import Subtitle from '@components/shared/titles/Subtitle';
import { OriginState } from './types';

interface OriginCarvedProps {
  originState: OriginState;
  onOriginChange: (field: keyof OriginState, value: string) => void;
  onResetStatus: () => void;
}

const OriginCarved: React.FC<OriginCarvedProps> = ({ originState, onOriginChange, onResetStatus }) => {
  const isValidProtocol = (proto: string) => ['TCP', 'Tcp', 'tcp', 'UDP', 'Udp', 'udp'].includes(proto);

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
      <Row>
        <Card className="panel" style={{ boxShadow: 'none', border: 'none' }}>
          <Card.Body>
            <Tabs fill activeKey={originState.carvedType} onSelect={(k) => onOriginChange('carvedType', k || 'Pcap')}>
              <Tab eventKey="Pcap" title="PCAP">
                <br />
                <Row>
                  <Col className="name-width" xs={2}>
                    <Subtitle>Source IP</Subtitle>
                  </Col>
                  <Col xs={5}>
                    <Form.Control
                      type="text"
                      placeholder="optional"
                      value={originState.originSourceIp}
                      onChange={(e) => {
                        onOriginChange('originSourceIp', String(e.target.value));
                        onResetStatus();
                      }}
                      isInvalid={!!originState.originSourceIp && !isIP(originState.originSourceIp)}
                    />
                    <Form.Control.Feedback type="invalid">Please enter a valid IPv4/IPv6 address.</Form.Control.Feedback>
                  </Col>
                </Row>
                <br />
                <Row>
                  <Col className="name-width" xs={2}>
                    <Subtitle>Destination IP</Subtitle>
                  </Col>
                  <Col xs={5}>
                    <Form.Control
                      type="text"
                      placeholder="optional"
                      value={originState.originDestinationIp}
                      onChange={(e) => {
                        onOriginChange('originDestinationIp', String(e.target.value));
                        onResetStatus();
                      }}
                      isInvalid={!!originState.originDestinationIp && !isIP(originState.originDestinationIp)}
                    />
                    <Form.Control.Feedback type="invalid">Please enter a valid IPv4/IPv6 address.</Form.Control.Feedback>
                  </Col>
                </Row>
                <br />
                <Row>
                  <Col className="name-width" xs={2}>
                    <Subtitle>Source Port</Subtitle>
                  </Col>
                  <Col xs={5}>
                    <Form.Control
                      type="number"
                      placeholder="optional"
                      value={originState.originSourcePort}
                      onChange={(e) => {
                        onOriginChange('originSourcePort', e.target.value);
                        onResetStatus();
                      }}
                      isInvalid={
                        !!originState.originSourcePort &&
                        (Number(originState.originSourcePort) < 1 || Number(originState.originSourcePort) > 65535)
                      }
                    />
                    <Form.Control.Feedback type="invalid">
                      Please enter a valid port (between 1 and 65535, inclusive).
                    </Form.Control.Feedback>
                  </Col>
                </Row>
                <br />
                <Row>
                  <Col className="name-width" xs={2}>
                    <Subtitle>Destination Port</Subtitle>
                  </Col>
                  <Col xs={5}>
                    <Form.Control
                      type="number"
                      placeholder="optional"
                      value={originState.originDestinationPort}
                      onChange={(e) => {
                        onOriginChange('originDestinationPort', e.target.value);
                        onResetStatus();
                      }}
                      isInvalid={
                        !!originState.originDestinationPort &&
                        (Number(originState.originDestinationPort) < 1 || Number(originState.originDestinationPort) > 65535)
                      }
                    />
                    <Form.Control.Feedback type="invalid">
                      Please enter a valid port (between 1 and 65535, inclusive).
                    </Form.Control.Feedback>
                  </Col>
                </Row>
                <br />
                <Row>
                  <Col className="name-width" xs={2}>
                    <Subtitle>Protocol</Subtitle>
                  </Col>
                  <Col xs={5}>
                    <Form.Control
                      type="text"
                      placeholder="TCP/UDP (optional)"
                      value={originState.originProtocol}
                      onChange={(e) => {
                        onOriginChange('originProtocol', e.target.value);
                        onResetStatus();
                      }}
                      isInvalid={!!originState.originProtocol && !isValidProtocol(originState.originProtocol)}
                    />
                    <Form.Control.Feedback type="invalid">
                      Please enter a valid protocol ("TCP/Tcp/tcp" or "UDP/Udp/udp").
                    </Form.Control.Feedback>
                  </Col>
                </Row>
                <br />
                <Row>
                  <Col className="name-width" xs={2}>
                    <Subtitle>URL</Subtitle>
                  </Col>
                  <Col xs={5}>
                    <Form.Control
                      type="text"
                      value={originState.originCarvedPcapUrl}
                      placeholder="optional"
                      onChange={(e) => {
                        onOriginChange('originCarvedPcapUrl', String(e.target.value));
                        onResetStatus();
                      }}
                    />
                  </Col>
                </Row>
              </Tab>
              <Tab eventKey="Unknown" title="Unknown"></Tab>
            </Tabs>
          </Card.Body>
        </Card>
      </Row>
    </>
  );
};

export default OriginCarved;
