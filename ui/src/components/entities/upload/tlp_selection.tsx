import React from 'react';
import { Col, Row } from 'react-bootstrap';
import { Button, Card } from 'react-bootstrap';
import { Subtitle } from '@components';
import { TLPColors, TLPState } from './types';
import { useUploadForm } from './upload_context';

/**
 * Component for TLP (Traffic Light Protocol) selection
 * Allows users to select one TLP level for the uploaded file
 */
export const TLPSelectionSection: React.FC = () => {
  const { state, dispatch } = useUploadForm();

  const handleTLPClick = (tlp: string) => {
    const tempSelection: TLPState = {};
    Object.keys(TLPColors).map((color) => {
      if (color !== tlp) {
        tempSelection[color] = false;
      } else {
        tempSelection[color] = !state.tlp[color];
      }
    });
    dispatch({ type: 'SET_TLP', payload: tempSelection });
    dispatch({ type: 'RESET_STATUS_MESSAGES' });
  };

  return (
    <>
      <Row className="mb-4 alt-label">
        <Col className="upload-field-name"></Col>
        <Col className="upload-field-name-alt">
          <Subtitle>
            TLP <sup>T</sup>
          </Subtitle>
        </Col>
      </Row>
      <Row className="mb-2">
        <Col className="upload-field-name">
          <Subtitle>
            TLP <sup>T</sup>
          </Subtitle>
        </Col>
        <Col className={(state.uploadInProgress ? 'disabled ' : '') + 'upload-field'}>
          <Card className="panel">
            <Card.Body className="d-flex justify-content-center">
              {Object.keys(TLPColors).map((tlp) => (
                <Button
                  variant=""
                  className={`tlp-btn ${TLPColors[tlp]}-btn ${state.tlp[tlp] ? 'selected' : ''}`}
                  key={tlp}
                  onClick={() => handleTLPClick(tlp)}
                >
                  <b>{tlp}</b>
                </Button>
              ))}
            </Card.Body>
          </Card>
        </Col>
      </Row>
    </>
  );
};
