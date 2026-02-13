import React from 'react';
import { Col, Row } from 'react-bootstrap';
import { Subtitle, UploadDropzone } from '@components';
import { useUploadForm } from '../context';

/**
 * Component for the file upload section
 * Handles file selection via dropzone
 */
export const FileUploadSection: React.FC = () => {
  const { state, dispatch } = useUploadForm();

  return (
    <>
      <Row className="mb-4 alt-label">
        <Col className="upload-field-name"></Col>
        <Col className="upload-field-name-alt">
          <Subtitle>
            File <sup>*</sup>
          </Subtitle>
        </Col>
      </Row>
      <Row>
        <Col className="upload-field-name">
          <Subtitle>
            File <sup>*</sup>
          </Subtitle>
        </Col>
        <Col className={(state.uploadInProgress ? 'disabled ' : '') + 'upload-field'}>
          <UploadDropzone
            width=""
            onChange={(files: any[]) => dispatch({ type: 'SET_FILES', payload: files })}
            onError={(errors: string[]) => dispatch({ type: 'SET_UPLOAD_ERROR', payload: errors })}
            selectedFiles={state.files}
          />
          <br />
        </Col>
      </Row>
    </>
  );
};
