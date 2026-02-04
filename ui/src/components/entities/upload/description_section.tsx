import React from 'react';
import { Col, Form, Row } from 'react-bootstrap';
import { Subtitle } from '@components';
import { useUploadForm } from './upload_context';

/**
 * Component for the file description section
 * Allows users to add a description to the uploaded file
 */
export const DescriptionSection: React.FC = () => {
  const { state, dispatch } = useUploadForm();

  const handleDescriptionChange = (value: string) => {
    dispatch({ type: 'SET_DESCRIPTION', payload: value });
    dispatch({ type: 'RESET_STATUS_MESSAGES' });
  };

  return (
    <>
      <Row className="mb-4 alt-label">
        <Col className="upload-field-name"></Col>
        <Col className="upload-field-name-alt">
          <Subtitle>Description</Subtitle>
        </Col>
      </Row>
      <Row>
        <Col className="upload-field-name">
          <Subtitle>Description</Subtitle>
        </Col>
        <Col className={(state.uploadInProgress ? 'disabled ' : '') + 'upload-field'}>
          <Form.Control
            className="description-field"
            as="textarea"
            placeholder="Add Description"
            value={state.description}
            onChange={(e) => handleDescriptionChange(e.target.value)}
          />
        </Col>
      </Row>
    </>
  );
};
