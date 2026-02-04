import React from 'react';
import { Card, Col, Form, Row, Tabs, Tab } from 'react-bootstrap';
import { isIP } from 'is-ip';
import { SelectableArray, Subtitle } from '@components';
import { OriginFormField } from './origin_form_field';
import { useUploadForm } from './upload_context';
import { CarvedOriginType, OriginType } from './types';

const ParentToolFlagsFields: React.FC = () => {
  const { state, dispatch } = useUploadForm();
  const { origin } = state;

  return (
    <>
      <OriginFormField
        label="Parent"
        value={origin.parent}
        placeholder="SHA256"
        onChange={(val) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { parent: String(val) } })}
        isInvalid={!origin.parent && (origin.tool || origin.flags.length > 0)}
        invalidMessage="Please enter a SHA256 value for the Parent."
      />
      <OriginFormField
        label="Tool"
        value={origin.tool}
        placeholder="optional"
        onChange={(val) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { tool: String(val) } })}
      />
      <br />
      <Row>
        <Col className="name-width" xs={2}>
          <Subtitle>Flags</Subtitle>
        </Col>
        <Col xs={5}>
          <SelectableArray
            initialEntries={origin.flags}
            setEntries={(val: string[]) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { flags: val } })}
            disabled={false}
            placeholder="optional"
            trim={false}
          />
        </Col>
      </Row>
    </>
  );
};

export const OriginFormTabsSection: React.FC = () => {
  const { state, dispatch } = useUploadForm();
  const { origin } = state;

  return (
    <>
      <Row className="mb-4 alt-label">
        <Col className="upload-field-name"></Col>
        <Col className="upload-field-name-alt">
          <Subtitle>
            Origin <sup>T</sup>
          </Subtitle>
        </Col>
      </Row>
      <Row className="mb-4">
        <Col className="upload-field-name">
          <Subtitle>
            Origin <sup>T</sup>
          </Subtitle>
        </Col>
        <Col className={(state.uploadInProgress ? 'disabled ' : '') + 'upload-field'}>
          <Card className="panel">
            <Card.Body>
              <Tabs
                fill
                activeKey={origin.origin_type}
                onSelect={(k) => k && dispatch({ type: 'SET_ORIGIN_FIELD', payload: { origin_type: k as OriginType } })}
              >
                <Tab eventKey={OriginType.Downloaded} title="Downloaded">
                  <OriginFormField
                    label="URL"
                    value={origin.url}
                    placeholder="badsite.xyz"
                    onChange={(val) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { url: String(val) } })}
                    isInvalid={!origin.url && origin.name}
                    invalidMessage="Please enter a site URL."
                  />
                  <OriginFormField
                    label="Site Name"
                    value={origin.name}
                    placeholder="optional"
                    onChange={(val) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { name: String(val) } })}
                  />
                </Tab>
                <Tab eventKey={OriginType.Transformed} title="Transformed">
                  <ParentToolFlagsFields />
                </Tab>
                <Tab eventKey={OriginType.Unpacked} title="Unpacked">
                  <ParentToolFlagsFields />
                </Tab>
                <Tab eventKey={OriginType.Carved} title="Carved">
                  <OriginFormField
                    label="Parent"
                    value={origin.parent}
                    placeholder="SHA256"
                    onChange={(val) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { parent: String(val) } })}
                    isInvalid={!origin.parent && (origin.tool || origin.flags.length > 0)}
                    invalidMessage="Please enter a SHA256 value for the Parent."
                  />
                  <OriginFormField
                    label="Tool"
                    value={origin.tool}
                    placeholder="optional"
                    onChange={(val) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { tool: String(val) } })}
                  />
                  <Row>
                    <Card className="panel" style={{ boxShadow: 'none' as any, border: 'none' }}>
                      <Card.Body>
                        <Tabs
                          fill
                          activeKey={origin.carved_type}
                          onSelect={(k) => k && dispatch({ type: 'SET_ORIGIN_FIELD', payload: { carved_type: k as CarvedOriginType } })}
                        >
                          <Tab eventKey={CarvedOriginType.Pcap} title="PCAP">
                            <OriginFormField
                              label="Source IP"
                              value={origin.src_ip}
                              placeholder="optional"
                              onChange={(val) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { src_ip: String(val) } })}
                              isInvalid={origin.src_ip && !isIP(origin.src_ip)}
                              invalidMessage="Please enter a valid IPv4/IPv6 address."
                            />
                            <OriginFormField
                              label="Destination IP"
                              value={origin.dest_ip}
                              placeholder="optional"
                              onChange={(val) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { dest_ip: String(val) } })}
                              isInvalid={origin.dest_ip && !isIP(origin.dest_ip)}
                              invalidMessage="Please enter a valid IPv4/IPv6 address."
                            />
                            <OriginFormField
                              label="Source Port"
                              type="number"
                              value={origin.src_port}
                              placeholder="optional"
                              onChange={(val) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { src_port: val } })}
                              isInvalid={origin.src_port && (origin.src_port < 1 || origin.src_port > 65535)}
                              invalidMessage="Please enter a valid port (between 1 and 65535, inclusive)."
                            />
                            <OriginFormField
                              label="Destination Port"
                              type="number"
                              value={origin.dest_port}
                              placeholder="optional"
                              onChange={(val) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { dest_port: val } })}
                              isInvalid={origin.dest_port && (origin.dest_port < 1 || origin.dest_port > 65535)}
                              invalidMessage="Please enter a valid port (between 1 and 65535, inclusive)."
                            />
                            <OriginFormField
                              label="Protocol"
                              value={origin.proto}
                              placeholder="TCP/UDP (optional)"
                              onChange={(val) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { proto: val } })}
                              isInvalid={
                                origin.proto &&
                                origin.proto !== 'TCP' &&
                                origin.proto !== 'Tcp' &&
                                origin.proto !== 'tcp' &&
                                origin.proto !== 'UDP' &&
                                origin.proto !== 'Udp' &&
                                origin.proto !== 'udp'
                              }
                              invalidMessage='Please enter a valid protocol ("TCP/Tcp/tcp" or "UDP/Udp/udp").'
                            />
                            <OriginFormField
                              label="URL"
                              value={origin.pcap_url}
                              placeholder="optional"
                              onChange={(val) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { pcap_url: String(val) } })}
                            />
                          </Tab>
                          <Tab eventKey={CarvedOriginType.Unknown} title="Unknown"></Tab>
                        </Tabs>
                      </Card.Body>
                    </Card>
                  </Row>
                </Tab>
                <Tab eventKey={OriginType.Wire} title="Wire">
                  <OriginFormField
                    label="Sniffer"
                    value={origin.sniffer}
                    placeholder="name"
                    onChange={(val) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { sniffer: String(val) } })}
                    isInvalid={!origin.sniffer && (origin.source || origin.destination)}
                    invalidMessage="Please enter a name for the Sniffer."
                  />
                  <OriginFormField
                    label="Source"
                    value={origin.source}
                    placeholder="optional"
                    onChange={(val) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { source: String(val) } })}
                  />
                  <OriginFormField
                    label="Destination"
                    value={origin.destination}
                    placeholder="optional"
                    onChange={(val) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { destination: String(val) } })}
                  />
                </Tab>
                <Tab eventKey={OriginType.Incident} title="Incident">
                  <OriginFormField
                    label="Incident ID"
                    value={origin.incident}
                    placeholder="name"
                    onChange={(val) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { incident: String(val) } })}
                    isInvalid={
                      !origin.incident && (origin.cover_term || origin.mission_team || origin.network || origin.machine || origin.location)
                    }
                    invalidMessage="Please enter an Incident ID."
                  />
                  <OriginFormField
                    label="Cover Term"
                    value={origin.cover_term}
                    placeholder="optional"
                    onChange={(val) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { cover_term: String(val) } })}
                  />
                  <OriginFormField
                    label="Mission Team"
                    value={origin.mission_team}
                    placeholder="optional"
                    onChange={(val) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { mission_team: String(val) } })}
                  />
                  <OriginFormField
                    label="Network"
                    value={origin.network}
                    placeholder="optional"
                    onChange={(val) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { network: String(val) } })}
                  />
                  <OriginFormField
                    label="Machine"
                    value={origin.machine}
                    placeholder="optional"
                    onChange={(val) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { machine: String(val) } })}
                  />
                  <OriginFormField
                    label="Location"
                    value={origin.location}
                    placeholder="optional"
                    onChange={(val) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { location: String(val) } })}
                  />
                </Tab>
                <Tab eventKey={OriginType.MemoryDump} title="Memory Dump">
                  <OriginFormField
                    label="Memory Type"
                    value={origin.memory_type}
                    placeholder="type"
                    onChange={(val) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { memory_type: String(val) } })}
                    isInvalid={!origin.memory_type && (origin.parent || origin.reconstructed.length > 0 || origin.base_addr)}
                    invalidMessage="Please enter a Memory Type."
                  />
                  <OriginFormField
                    label="Parent"
                    value={origin.parent}
                    placeholder="optional"
                    onChange={(val) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { parent: String(val) } })}
                  />
                  <br />
                  <Row>
                    <Col className="name-width" xs={2}>
                      <Subtitle>Reconstructed</Subtitle>
                    </Col>
                    <Col xs={5}>
                      <SelectableArray
                        initialEntries={[]}
                        setEntries={(val: any) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { reconstructed: val } })}
                        disabled={false}
                        placeholder="optional"
                        trim={false}
                      />
                    </Col>
                  </Row>
                  <OriginFormField
                    label="Base Address"
                    value={origin.base_addr}
                    placeholder="optional"
                    onChange={(val) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { base_addr: String(val) } })}
                  />
                </Tab>
                <Tab eventKey={OriginType.Source} title="Source">
                  <OriginFormField
                    label="Repository"
                    value={origin.repo}
                    placeholder="repository URL"
                    onChange={(val) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { repo: String(val) } })}
                    isInvalid={!origin.repo && (origin.commit || origin.system || origin.commitish || origin.flags.length > 0)}
                    invalidMessage="Please enter a repository."
                  />
                  <OriginFormField
                    label="Commitish"
                    value={origin.commitish}
                    placeholder="branch, tag, or commit (optional)"
                    onChange={(val) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { commitish: String(val) } })}
                  />
                  <OriginFormField
                    label="Commit"
                    value={origin.commit}
                    placeholder="commit hash"
                    onChange={(val) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { commit: String(val) } })}
                  />
                  <OriginFormField
                    label="Build System"
                    value={origin.system}
                    placeholder="build system"
                    onChange={(val) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { system: String(val) } })}
                  />
                  <br />
                  <Row>
                    <Col className="name-width" xs={2}>
                      <Subtitle>Flags</Subtitle>
                    </Col>
                    <Col xs={5}>
                      <SelectableArray
                        initialEntries={origin.flags}
                        setEntries={(val: string[]) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { flags: val } })}
                        disabled={false}
                        placeholder="optional"
                        trim={false}
                      />
                    </Col>
                  </Row>
                  <br />
                  <Row>
                    <Col className="name-width" xs={2}>
                      <Subtitle>Supporting</Subtitle>
                    </Col>
                    <Col xs={5}>
                      <Form.Check
                        type="checkbox"
                        label="This is a supporting build file"
                        checked={origin.supporting ?? false}
                        onChange={(e) => dispatch({ type: 'SET_ORIGIN_FIELD', payload: { supporting: e.target.checked } })}
                      />
                    </Col>
                  </Row>
                </Tab>
              </Tabs>
            </Card.Body>
          </Card>
        </Col>
      </Row>
    </>
  );
};
